//! OpenCL nonce generation.  One work-item per nonce; the kernel is in
//! `src/ocl/plot_kernel.cl` and is checked against the CPU reference in the
//! tests (run with an OpenCL runtime such as pocl present).

use super::poc2::GEN_SIZE;
use super::NonceGenerator;
use ocl_core as core;
use ocl_core::{ArgVal, ContextProperties, DeviceInfo, DeviceInfoResult, Event, PlatformInfo};
use std::ffi::CString;
use std::io;

static SRC: &str = include_str!("../ocl/plot_kernel.cl");

fn cl_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("OpenCL: {}", e))
}

fn device_string(id: core::DeviceId, info: DeviceInfo) -> String {
    match core::get_device_info(id, info) {
        Ok(r) => r.to_string(),
        Err(_) => "?".to_string(),
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
            let mem = match core::get_device_info(device, DeviceInfo::GlobalMemSize) {
                Ok(DeviceInfoResult::GlobalMemSize(bytes)) => {
                    format!("{:.1} GiB", bytes as f64 / 1024.0 / 1024.0 / 1024.0)
                }
                _ => "?".into(),
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

pub struct GpuGenerator {
    name: String,
    queue: core::CommandQueue,
    kernel: core::Kernel,
    gendata: core::Mem,
    batch: usize,
    work_group: usize,
    _context: core::Context,
}

impl GpuGenerator {
    /// Set up `platform:device` with room for `batch` nonces (`batch * 256 KiB` of device memory).
    pub fn new(platform_index: usize, device_index: usize, batch: usize) -> io::Result<Self> {
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
        let kernel = core::create_kernel(&program, "generate_nonces").map_err(cl_err)?;
        let queue = core::create_command_queue(&context, device, None).map_err(cl_err)?;

        let work_group = match core::get_kernel_work_group_info(
            &kernel,
            device,
            core::KernelWorkGroupInfo::WorkGroupSize,
        ) {
            Ok(core::KernelWorkGroupInfoResult::WorkGroupSize(s)) => s.clamp(1, 64),
            _ => 32,
        };
        let batch = batch.max(1);
        // SAFETY: plain buffer allocation of a known size.
        let gendata = unsafe {
            core::create_buffer::<_, u8>(&context, core::MEM_READ_WRITE, batch * GEN_SIZE, None)
                .map_err(cl_err)?
        };
        Ok(Self {
            name,
            queue,
            kernel,
            gendata,
            batch,
            work_group,
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
        format!("GPU {}", self.name)
    }

    fn stride(&self) -> usize {
        GEN_SIZE
    }

    fn generate(
        &mut self,
        account_id: u64,
        start_nonce: u64,
        count: usize,
        out: &mut [u8],
    ) -> io::Result<()> {
        if count > self.batch {
            return Err(cl_err(format!(
                "batch of {} exceeds the configured {}",
                count, self.batch
            )));
        }
        if out.len() < count * GEN_SIZE {
            return Err(cl_err("output buffer too small"));
        }
        core::set_kernel_arg(&self.kernel, 0, ArgVal::scalar(&account_id)).map_err(cl_err)?;
        core::set_kernel_arg(&self.kernel, 1, ArgVal::scalar(&start_nonce)).map_err(cl_err)?;
        core::set_kernel_arg(&self.kernel, 2, ArgVal::scalar(&(count as u32))).map_err(cl_err)?;
        core::set_kernel_arg(&self.kernel, 3, ArgVal::mem(&self.gendata)).map_err(cl_err)?;

        let global = count.div_ceil(self.work_group) * self.work_group;
        // SAFETY: kernel arguments are set and the buffer is large enough for `count` work-items.
        unsafe {
            core::enqueue_kernel(
                &self.queue,
                &self.kernel,
                1,
                None,
                &[global, 1, 1],
                Some([self.work_group, 1, 1]),
                None::<Event>,
                None::<&mut Event>,
            )
            .map_err(cl_err)?;
            core::enqueue_read_buffer(
                &self.queue,
                &self.gendata,
                true,
                0,
                &mut out[..count * GEN_SIZE],
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
    use crate::plotter::poc2::{generate_batch, NONCE_SIZE};

    /// Needs an OpenCL runtime (e.g. `pocl-opencl-icd`); skipped when none is present.
    #[test]
    fn gpu_nonces_match_cpu_reference() {
        let mut gpu = match GpuGenerator::new(0, 0, 11) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping GPU test: {}", e);
                return;
            }
        };
        eprintln!("testing on {}", gpu.name());
        let account = 10282355196851764065u64;
        for (start, count) in [(0u64, 8usize), (123_456_789, 11)] {
            let mut gpu_out = vec![0u8; count * GEN_SIZE];
            gpu.generate(account, start, count, &mut gpu_out).unwrap();
            let mut cpu_out = vec![0u8; count * NONCE_SIZE];
            generate_batch(account, start, &mut cpu_out);
            for n in 0..count {
                assert!(
                    gpu_out[n * GEN_SIZE..n * GEN_SIZE + NONCE_SIZE]
                        == cpu_out[n * NONCE_SIZE..(n + 1) * NONCE_SIZE],
                    "nonce {} differs between GPU and CPU",
                    start + n as u64
                );
            }
        }
    }
}
