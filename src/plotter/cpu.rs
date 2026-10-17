//! CPU nonce generation through the C `noncegen` (see `src/c/noncegen.c`):
//! several nonces are hashed at once with a SIMD Shabal implementation
//! (AVX-512: 16, AVX2: 8, SSE2/AVX/NEON: 4), each rayon worker taking one
//! group of nonces at a time.  On x86 the widest instruction set the CPU
//! supports is picked at run time.

use super::poc2::NONCE_SIZE;
use super::NonceGenerator;
use rayon::prelude::*;
use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::io;

/// One compiled nonce generator: the C entry points of `src/c/noncegen.c`.
#[derive(Clone, Copy)]
struct Backend {
    name: &'static str,
    init: unsafe extern "C" fn(),
    lanes: unsafe extern "C" fn() -> u32,
    scratch_words: unsafe extern "C" fn() -> usize,
    generate: unsafe extern "C" fn(u64, u64, *mut u32, *mut u8, usize, usize, usize),
}

// Which backends a given build actually calls depends on the target and on
// run-time detection, so each declaration is allowed to go unused.
macro_rules! declare_backend {
    ($const_name:ident, $label:expr, $init:ident, $lanes:ident, $words:ident, $gen:ident) => {
        #[allow(dead_code)]
        extern "C" {
            fn $init();
            fn $lanes() -> u32;
            fn $words() -> usize;
            fn $gen(
                account_id: u64,
                start_nonce: u64,
                scratch: *mut u32,
                out: *mut u8,
                chunk_nonces: usize,
                offset: usize,
                count: usize,
            );
        }
        #[allow(dead_code)]
        const $const_name: Backend = Backend {
            name: $label,
            init: $init,
            lanes: $lanes,
            scratch_words: $words,
            generate: $gen,
        };
    };
}

declare_backend!(
    SCALAR,
    "scalar",
    noncegen_scalar_init,
    noncegen_scalar_lanes,
    noncegen_scalar_scratch_words,
    noncegen_scalar
);

// The build's own backend, when it is not one of the x86 variants above
// (those are reached through run-time detection instead).
#[cfg(any(simd_backend = "sse2", simd_backend = "avx", simd_backend = "neon"))]
declare_backend!(
    SIMD,
    SIMD_LABEL,
    noncegen_simd_init,
    noncegen_simd_lanes,
    noncegen_simd_scratch_words,
    noncegen_simd
);

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
declare_backend!(
    AVX2,
    "AVX2",
    noncegen_avx2_init,
    noncegen_avx2_lanes,
    noncegen_avx2_scratch_words,
    noncegen_avx2
);

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
declare_backend!(
    AVX512F,
    "AVX-512",
    noncegen_avx512f_init,
    noncegen_avx512f_lanes,
    noncegen_avx512f_scratch_words,
    noncegen_avx512f
);

cfg_if! {
    if #[cfg(simd_backend = "avx")] {
        const SIMD_LABEL: &str = "AVX";
    } else if #[cfg(simd_backend = "sse2")] {
        const SIMD_LABEL: &str = "SSE2";
    } else if #[cfg(simd_backend = "neon")] {
        const SIMD_LABEL: &str = "NEON";
    }
}

/// The widest generator this CPU can run.  On x86 the AVX-512 and AVX2
/// variants are always compiled and detected at run time, so a default build
/// uses 16 lanes on an AVX-512 machine and 8 on an AVX2 one; otherwise the
/// backend the build selected (SSE2/AVX/NEON), and the scalar code last.
fn select_backend() -> Backend {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if std::is_x86_feature_detected!("avx512f") {
            return AVX512F;
        }
        if std::is_x86_feature_detected!("avx2") {
            return AVX2;
        }
    }
    cfg_if! {
        if #[cfg(any(simd_backend = "sse2", simd_backend = "avx", simd_backend = "neon"))] {
            SIMD
        } else {
            // "none", or an AVX2/AVX-512 build on a CPU without them.
            SCALAR
        }
    }
}

/// Refuse to run SIMD code the CPU does not have (a clear message instead of
/// an illegal instruction crash).  The plotter itself falls back to scalar
/// code, but the miner built alongside it would not start on this CPU.
pub fn check_cpu_support() -> Result<(), String> {
    cfg_if! {
        if #[cfg(all(simd_backend = "avx", any(target_arch = "x86", target_arch = "x86_64")))] {
            if !std::is_x86_feature_detected!("avx") {
                return Err("this build uses AVX but the CPU lacks it; rebuild with --no-default-features --features simd_sse2".into());
            }
        }
    }
    Ok(())
}

/// 64-byte aligned scratch memory for one lane group.
struct Scratch {
    ptr: *mut u32,
    layout: Layout,
}

impl Scratch {
    fn new(words: usize) -> Self {
        let layout = Layout::from_size_align(words * 4, 64).expect("scratch layout");
        // SAFETY: non-zero size, valid layout; zeroed so every word is initialised.
        let ptr = unsafe { alloc_zeroed(layout) } as *mut u32;
        assert!(!ptr.is_null(), "out of memory allocating plotter scratch");
        Self { ptr, layout }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // SAFETY: allocated with this layout in `new`.
        unsafe { dealloc(self.ptr as *mut u8, self.layout) }
    }
}

// SAFETY: the scratch is only ever used by the thread that owns it.
unsafe impl Send for Scratch {}

/// Base pointer of the output chunk, shared between workers that write
/// disjoint 64-byte scoop slots.
#[derive(Clone, Copy)]
struct OutPtr(*mut u8);
// SAFETY: every worker writes only the slots of its own nonce indexes.
unsafe impl Send for OutPtr {}
unsafe impl Sync for OutPtr {}

impl OutPtr {
    /// Accessor so closures capture the wrapper, not the raw pointer field.
    fn get(&self) -> *mut u8 {
        self.0
    }
}

pub struct CpuGenerator {
    backend: Backend,
    lanes: usize,
    scratch_words: usize,
}

impl CpuGenerator {
    pub fn new() -> Self {
        let backend = select_backend();
        // SAFETY: plain FFI initialisation of a static Shabal context; called
        // before any worker uses it.  The getters are trivial.
        let (lanes, scratch_words) = unsafe {
            (backend.init)();
            ((backend.lanes)() as usize, (backend.scratch_words)())
        };
        Self {
            backend,
            lanes,
            scratch_words,
        }
    }

    #[allow(dead_code)]
    pub fn lanes(&self) -> usize {
        self.lanes
    }
}

impl Default for CpuGenerator {
    fn default() -> Self {
        Self::new()
    }
}

impl NonceGenerator for CpuGenerator {
    fn name(&self) -> String {
        format!(
            "CPU ({} threads, {} x{} lanes)",
            rayon::current_num_threads(),
            self.backend.name,
            self.lanes
        )
    }

    fn max_batch(&self) -> usize {
        0
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
        if offset + count > chunk_nonces || chunk.len() < chunk_nonces * NONCE_SIZE {
            return Err(io::Error::other("nonce range does not fit the chunk"));
        }
        let lanes = self.lanes;
        let words = self.scratch_words;
        let generate = self.backend.generate;
        let out = OutPtr(chunk.as_mut_ptr());
        (0..count.div_ceil(lanes)).into_par_iter().for_each_init(
            || Scratch::new(words),
            |scratch, group| {
                let first = group * lanes;
                let n = lanes.min(count - first);
                // SAFETY: scratch is large enough and aligned; the output
                // slots for nonce indexes offset+first .. +n are inside
                // `chunk` (checked above) and disjoint between groups.
                unsafe {
                    generate(
                        account_id,
                        start_nonce + first as u64,
                        scratch.ptr,
                        out.get(),
                        chunk_nonces,
                        offset + first,
                        n,
                    )
                }
            },
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plotter::poc2::{
        generate_nonce, scoop_major_reference, SCOOPS_PER_NONCE, SCOOP_SIZE,
    };

    /// The SIMD path must produce exactly what the byte-level reference does,
    /// including a count that is not a multiple of the lane width and a
    /// non-zero offset inside the chunk.
    #[test]
    fn simd_generator_matches_reference() {
        let mut gen = CpuGenerator::new();
        let account = 10282355196851764065u64;
        let chunk_nonces = gen.lanes() * 2 + 3;
        let offset = 2;
        let count = chunk_nonces - offset;
        let start = 123_456_789_000u64;
        let mut chunk = vec![0u8; chunk_nonces * NONCE_SIZE];
        gen.generate(account, start, count, &mut chunk, chunk_nonces, offset)
            .unwrap();
        let expected = scoop_major_reference(account, start, count, chunk_nonces, offset);
        for s in 0..SCOOPS_PER_NONCE {
            for n in offset..chunk_nonces {
                let at = (s * chunk_nonces + n) * SCOOP_SIZE;
                assert!(
                    chunk[at..at + SCOOP_SIZE] == expected[at..at + SCOOP_SIZE],
                    "scoop {} of nonce index {} differs from the reference",
                    s,
                    n
                );
            }
        }
        // Untouched slots stay zero.
        assert!(chunk[..offset * SCOOP_SIZE].iter().all(|&b| b == 0));
        // And the plain reference agrees with itself through generate_nonce.
        let one = generate_nonce(account, start);
        assert_eq!(
            &one[..SCOOP_SIZE],
            &expected[offset * SCOOP_SIZE..(offset + 1) * SCOOP_SIZE]
        );
    }

    /// Every generator this binary carries and this CPU can run must agree
    /// with the reference, not just the one `select_backend` prefers.
    #[test]
    fn every_runnable_backend_matches_reference() {
        let mut backends: Vec<Backend> = vec![SCALAR];
        #[cfg(any(simd_backend = "sse2", simd_backend = "avx", simd_backend = "neon"))]
        {
            if check_cpu_support().is_ok() {
                backends.push(SIMD);
            }
        }
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            if std::is_x86_feature_detected!("avx2") {
                backends.push(AVX2);
            }
            if std::is_x86_feature_detected!("avx512f") {
                backends.push(AVX512F);
            }
        }
        let account = 10282355196851764065u64;
        let start = 987_654_321u64;
        let count = 5;
        let expected = scoop_major_reference(account, start, count, count, 0);
        for b in backends {
            let mut gen = CpuGenerator {
                backend: b,
                lanes: 0,
                scratch_words: 0,
            };
            // SAFETY: trivial FFI initialisation and getters.
            unsafe {
                (b.init)();
                gen.lanes = (b.lanes)() as usize;
                gen.scratch_words = (b.scratch_words)();
            }
            let mut chunk = vec![0u8; count * NONCE_SIZE];
            gen.generate(account, start, count, &mut chunk, count, 0)
                .unwrap();
            assert!(
                chunk == expected,
                "backend {} ({} lanes) differs from the reference",
                b.name,
                gen.lanes
            );
            eprintln!("backend {} ({} lanes) ok", b.name, gen.lanes);
        }
    }
}
