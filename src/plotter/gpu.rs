//! OpenCL nonce generation.  The kernels are in `src/ocl/plot_kernel.cl`;
//! see the comment there for the memory layout.  Checked against the CPU
//! reference in the tests (run with an OpenCL runtime such as pocl present).

use super::poc2::{GEN_SIZE, NONCE_SIZE, SCOOPS_PER_NONCE, SCOOP_SIZE};
use super::NonceGenerator;
use ocl_core as core;
use ocl_core::{ArgVal, ContextProperties, DeviceInfo, DeviceInfoResult, Event, PlatformInfo};
use std::ffi::CString;
use std::io;

static SRC: &str = include_str!("../ocl/plot_kernel.cl");

/// Device memory per nonce in flight: work buffer, output, final hash.
const BYTES_PER_NONCE: usize = GEN_SIZE + NONCE_SIZE + 32;

fn cl_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("OpenCL: {}", e))
}

fn device_string(id: core::DeviceId, info: DeviceInfo) -> String {
    match core::get_device_info(id, info) {
        Ok(r) => r.to_string(),
        Err(_) => "?".to_string(),
    }
}

fn device_memory(id: core::DeviceId) -> Option<u64> {
    match core::get_device_info(id, DeviceInfo::GlobalMemSize) {
        Ok(DeviceInfoResult::GlobalMemSize(bytes)) => Some(bytes),
        _ => None,
    }
}

/// Human readable list of every OpenCL device, as `platform:device  name`.
pub fn list_devices() -> io::Result<Vec<String>> {
    let mut out = Vec::new();
    let platforms = core::get_platform_ids().map_err(cl_err)?;
    for (pi, platform) in platforms.iter().enumerate() {
        let pname = core::get_platform_info(platform, PlatformInfo::Name)
            .map(|r| r.to_string())
            .unwrap_or_else(|_| "?".into());
        let devices = core::get_device_ids(platform, None, None).unwrap_or_default();
        for (di, device) in devices.iter().enumerate() {
            let mem = match device_memory(*device) {
                Some(bytes) => format!("{:.1} GiB", bytes as f64 / 1024.0 / 1024.0 / 1024.0),
                None => "?".into(),
            };
            out.push(format!(
                "{}:{}  {} / {} ({} compute units, {})",
                pi,
                di,
                pname.trim(),
                device_string(*device, DeviceInfo::Name).trim(),
                device_string(*device, DeviceInfo::MaxComputeUnits),
                mem
            ));
        }
    }
    Ok(out)
}

/// Batch size that keeps the buffers within about half of the device memory.
pub fn default_batch(device_mem: Option<u64>) -> usize {
    match device_mem {
        Some(bytes) => ((bytes / 2) as usize / BYTES_PER_NONCE).clamp(64, 4096),
        None => 1024,
    }
}

pub struct GpuGenerator {
    name: String,
    queue: core::CommandQueue,
    generate: core::Kernel,
    finish: core::Kernel,
    gendata: core::Mem,
    finals: core::Mem,
    out: core::Mem,
    batch: usize,
    /// Batch rounded up to a multiple of the work-group size.
    padded: usize,
    work_group: usize,
    finish_group: usize,
    _context: core::Context,
}

impl GpuGenerator {
    /// Set up `platform:device`.  `batch` is the number of nonces per kernel
    /// launch (`None`: sized from the device memory).
    pub fn new(
        platform_index: usize,
        device_index: usize,
        batch: Option<usize>,
    ) -> io::Result<Self> {
        let platforms = core::get_platform_ids().map_err(cl_err)?;
        let platform = *platforms
            .get(platform_index)
            .ok_or_else(|| cl_err(format!("no OpenCL platform {}", platform_index)))?;
        let devices = core::get_device_ids(platform, None, None).map_err(cl_err)?;
        let device = *devices.get(device_index).ok_or_else(|| {
            cl_err(format!(
                "no device {} on platform {}",
                device_index, platform_index
            ))
        })?;
        let name = device_string(device, DeviceInfo::Name).trim().to_string();
        let mem = device_memory(device);

        let props = ContextProperties::new().platform(platform);
        let context = core::create_context(Some(&props), &[device], None, None).map_err(cl_err)?;
        let src = CString::new(SRC).map_err(cl_err)?;
        let program = core::create_program_with_source(&context, &[src]).map_err(cl_err)?;
        if let Err(e) = core::build_program(
            &program,
            Some(&[device]),
            &CString::new("").unwrap(),
            None,
            None,
        ) {
            let log =
                core::get_program_build_info(&program, device, core::ProgramBuildInfo::BuildLog)
                    .map(|l| l.to_string())
                    .unwrap_or_default();
            return Err(cl_err(format!("kernel build failed: {}\n{}", e, log)));
        }
        let generate = core::create_kernel(&program, "generate_nonces").map_err(cl_err)?;
        let finish = core::create_kernel(&program, "finish_nonces").map_err(cl_err)?;
        let queue = core::create_command_queue(&context, device, None).map_err(cl_err)?;

        let group_of = |kernel: &core::Kernel, max: usize| match core::get_kernel_work_group_info(
            kernel,
            device,
            core::KernelWorkGroupInfo::WorkGroupSize,
        ) {
            Ok(core::KernelWorkGroupInfoResult::WorkGroupSize(s)) => s.clamp(1, max),
            _ => 32,
        };
        let work_group = group_of(&generate, 64);
        let finish_group = group_of(&finish, 256);

        let batch = batch.unwrap_or_else(|| default_batch(mem)).max(1);
        let padded = batch.div_ceil(work_group) * work_group;
        if let Some(bytes) = mem {
            if (padded * BYTES_PER_NONCE) as u64 > bytes {
                return Err(cl_err(format!(
                    "batch of {} nonces needs {:.1} GiB of device memory, the device has {:.1} GiB; use a smaller --batch",
                    batch,
                    (padded * BYTES_PER_NONCE) as f64 / (1u64 << 30) as f64,
                    bytes as f64 / (1u64 << 30) as f64
                )));
            }
        }
        // SAFETY: plain buffer allocations of known sizes.
        let (gendata, finals, out) = unsafe {
            (
                core::create_buffer::<_, u8>(
                    &context,
                    core::MEM_READ_WRITE,
                    padded * GEN_SIZE,
                    None,
                )
                .map_err(cl_err)?,
                core::create_buffer::<_, u8>(&context, core::MEM_READ_WRITE, padded * 32, None)
                    .map_err(cl_err)?,
                core::create_buffer::<_, u8>(
                    &context,
                    core::MEM_WRITE_ONLY,
                    batch * NONCE_SIZE,
                    None,
                )
                .map_err(cl_err)?,
            )
        };
        Ok(Self {
            name,
            queue,
            generate,
            finish,
            gendata,
            finals,
            out,
            batch,
            padded,
            work_group,
            finish_group,
            _context: context,
        })
    }

    #[allow(dead_code)]
    pub fn batch(&self) -> usize {
        self.batch
    }
}

impl NonceGenerator for GpuGenerator {
    fn name(&self) -> String {
        format!("GPU {} ({} nonces per launch)", self.name, self.batch)
    }

    fn max_batch(&self) -> usize {
        self.batch
    }

    fn generate(
        &mut self,
        account_id: u64,
        start_nonce: u64,
        count: usize,
        chunk: &mut [u8],
        chunk_nonces: usize,
        offset: usize,
    ) -> io::Result<()> {
        if count == 0 || count > self.batch {
            return Err(cl_err(format!(
                "batch of {} exceeds the configured {}",
                count, self.batch
            )));
        }
        if offset + count > chunk_nonces || chunk.len() < chunk_nonces * NONCE_SIZE {
            return Err(cl_err("nonce range does not fit the chunk"));
        }
        let n = self.padded as u32;
        core::set_kernel_arg(&self.generate, 0, ArgVal::scalar(&account_id)).map_err(cl_err)?;
        core::set_kernel_arg(&self.generate, 1, ArgVal::scalar(&start_nonce)).map_err(cl_err)?;
        core::set_kernel_arg(&self.generate, 2, ArgVal::scalar(&(count as u32))).map_err(cl_err)?;
        core::set_kernel_arg(&self.generate, 3, ArgVal::scalar(&n)).map_err(cl_err)?;
        core::set_kernel_arg(&self.generate, 4, ArgVal::mem(&self.gendata)).map_err(cl_err)?;
        core::set_kernel_arg(&self.generate, 5, ArgVal::mem(&self.finals)).map_err(cl_err)?;

        core::set_kernel_arg(&self.finish, 0, ArgVal::scalar(&(count as u32))).map_err(cl_err)?;
        core::set_kernel_arg(&self.finish, 1, ArgVal::scalar(&n)).map_err(cl_err)?;
        core::set_kernel_arg(&self.finish, 2, ArgVal::mem(&self.gendata)).map_err(cl_err)?;
        core::set_kernel_arg(&self.finish, 3, ArgVal::mem(&self.finals)).map_err(cl_err)?;
        core::set_kernel_arg(&self.finish, 4, ArgVal::mem(&self.out)).map_err(cl_err)?;

        let finish_global = self.padded * SCOOPS_PER_NONCE;
        let finish_global = finish_global.div_ceil(self.finish_group) * self.finish_group;
        // SAFETY: kernel arguments are set and every buffer is sized for
        // `padded` work-items; the rect read targets `count` rows of the
        // chunk that lie inside `chunk` (checked above).
        unsafe {
            core::enqueue_kernel(
                &self.queue,
                &self.generate,
                1,
                None,
                &[self.padded, 1, 1],
                Some([self.work_group, 1, 1]),
                None::<Event>,
                None::<&mut Event>,
            )
            .map_err(cl_err)?;
            core::enqueue_kernel(
                &self.queue,
                &self.finish,
                1,
                None,
                &[finish_global, 1, 1],
                Some([self.finish_group, 1, 1]),
                None::<Event>,
                None::<&mut Event>,
            )
            .map_err(cl_err)?;
            // Device: [scoop][count][64]; host chunk: [scoop][chunk_nonces][64]
            // at nonce index `offset`.  One rectangular copy does the transpose
            // of the row pitch.
            core::enqueue_read_buffer_rect(
                &self.queue,
                &self.out,
                true,
                [0, 0, 0],
                [offset * SCOOP_SIZE, 0, 0],
                [count * SCOOP_SIZE, SCOOPS_PER_NONCE, 1],
                count * SCOOP_SIZE,
                0,
                chunk_nonces * SCOOP_SIZE,
                0,
                &mut chunk[..chunk_nonces * NONCE_SIZE],
                None::<Event>,
                None::<&mut Event>,
            )
            .map_err(cl_err)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plotter::poc2::scoop_major_reference;

    /// Needs an OpenCL runtime (e.g. `pocl-opencl-icd`); skipped when none is present.
    #[test]
    fn gpu_nonces_match_cpu_reference() {
        let mut gpu = match GpuGenerator::new(0, 0, Some(11)) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping GPU test: {}", e);
                return;
            }
        };
        eprintln!("testing on {}", gpu.name());
        let account = 10282355196851764065u64;
        // Two batches into one chunk, the second not a multiple of anything.
        let chunk_nonces = 19;
        let mut chunk = vec![0u8; chunk_nonces * NONCE_SIZE];
        gpu.generate(account, 0, 11, &mut chunk, chunk_nonces, 0)
            .unwrap();
        gpu.generate(account, 11, 7, &mut chunk, chunk_nonces, 11)
            .unwrap();
        let expected = scoop_major_reference(account, 0, 18, chunk_nonces, 0);
        for s in 0..SCOOPS_PER_NONCE {
            for n in 0..chunk_nonces {
                let at = (s * chunk_nonces + n) * SCOOP_SIZE;
                assert!(
                    chunk[at..at + SCOOP_SIZE] == expected[at..at + SCOOP_SIZE],
                    "scoop {} of nonce index {} differs between GPU and reference",
                    s,
                    n
                );
            }
        }
        assert_eq!(default_batch(Some(8 << 30)), 4096);
        assert!((1000..=1024).contains(&default_batch(Some(1 << 30))));
    }
}
