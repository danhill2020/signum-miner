use crate::miner::{Buffer, NonceData};
use crate::plot::SCOOP_SIZE;
use crate::reader::ReadReply;
use crate::utils::lock_or_recover;
use crossbeam_channel::{Receiver, Sender};
use tokio::sync::mpsc::Sender as TokioSender;

#[cfg(not(simd_backend = "none"))]
use std::os::raw::c_void;

#[cfg(simd_backend = "avx512f")]
extern "C" {
    fn find_best_deadline_avx512f(
        scoops: *mut c_void,
        nonce_count: u64,
        gensig: *const c_void,
        best_deadline: *mut u64,
        best_offset: *mut u64,
    );
}

#[cfg(simd_backend = "avx2")]
extern "C" {
    fn find_best_deadline_avx2(
        scoops: *mut c_void,
        nonce_count: u64,
        gensig: *const c_void,
        best_deadline: *mut u64,
        best_offset: *mut u64,
    );
}

#[cfg(simd_backend = "avx")]
extern "C" {
    fn find_best_deadline_avx(
        scoops: *mut c_void,
        nonce_count: u64,
        gensig: *const c_void,
        best_deadline: *mut u64,
        best_offset: *mut u64,
    );
}

#[cfg(simd_backend = "sse2")]
extern "C" {
    fn find_best_deadline_sse2(
        scoops: *mut c_void,
        nonce_count: u64,
        gensig: *const c_void,
        best_deadline: *mut u64,
        best_offset: *mut u64,
    );
}

#[cfg(simd_backend = "neon")]
extern "C" {
    fn find_best_deadline_neon(
        scoops: *mut c_void,
        nonce_count: u64,
        gensig: *const c_void,
        best_deadline: *mut u64,
        best_offset: *mut u64,
    );
}

/// One-time initialisation of the SIMD hashing context.  Must run before the
/// first call to [`find_best_deadline`]; calling it again is harmless.
pub fn init_backend() {
    cfg_if! {
        if #[cfg(simd_backend = "avx512f")] {
            extern "C" { fn init_shabal_avx512f(); }
            unsafe { init_shabal_avx512f() }
        } else if #[cfg(simd_backend = "avx2")] {
            extern "C" { fn init_shabal_avx2(); }
            unsafe { init_shabal_avx2() }
        } else if #[cfg(simd_backend = "avx")] {
            extern "C" { fn init_shabal_avx(); }
            unsafe { init_shabal_avx() }
        } else if #[cfg(simd_backend = "sse2")] {
            extern "C" { fn init_shabal_sse2(); }
            unsafe { init_shabal_sse2() }
        } else if #[cfg(simd_backend = "neon")] {
            extern "C" { fn init_shabal_neon(); }
            unsafe { init_shabal_neon() }
        }
    }
}

/// Name of the hashing backend compiled into this binary.
pub fn backend_name() -> &'static str {
    cfg_if! {
        if #[cfg(simd_backend = "avx512f")] {
            "AVX512F"
        } else if #[cfg(simd_backend = "avx2")] {
            "AVX2"
        } else if #[cfg(simd_backend = "avx")] {
            "AVX"
        } else if #[cfg(simd_backend = "sse2")] {
            "SSE2"
        } else if #[cfg(simd_backend = "neon")] {
            "NEON"
        } else {
            "none (portable Rust)"
        }
    }
}

/// Find the best deadline in `scoops` (a multiple of 64 bytes).
/// Returns `(deadline, offset_in_nonces)`.
pub fn find_best_deadline(scoops: &[u8], nonce_count: u64, gensig: &[u8; 32]) -> (u64, u64) {
    debug_assert!(scoops.len() as u64 >= nonce_count * SCOOP_SIZE);
    #[allow(unused_mut)]
    let mut deadline: u64 = u64::MAX;
    #[allow(unused_mut)]
    let mut offset: u64 = 0;
    if nonce_count == 0 {
        return (deadline, offset);
    }

    cfg_if! {
        if #[cfg(simd_backend = "avx512f")] {
            unsafe {
                find_best_deadline_avx512f(
                    scoops.as_ptr() as *mut c_void,
                    nonce_count,
                    gensig.as_ptr() as *const c_void,
                    &mut deadline,
                    &mut offset,
                );
            }
        } else if #[cfg(simd_backend = "avx2")] {
            unsafe {
                find_best_deadline_avx2(
                    scoops.as_ptr() as *mut c_void,
                    nonce_count,
                    gensig.as_ptr() as *const c_void,
                    &mut deadline,
                    &mut offset,
                );
            }
        } else if #[cfg(simd_backend = "avx")] {
            unsafe {
                find_best_deadline_avx(
                    scoops.as_ptr() as *mut c_void,
                    nonce_count,
                    gensig.as_ptr() as *const c_void,
                    &mut deadline,
                    &mut offset,
                );
            }
        } else if #[cfg(simd_backend = "sse2")] {
            unsafe {
                find_best_deadline_sse2(
                    scoops.as_ptr() as *mut c_void,
                    nonce_count,
                    gensig.as_ptr() as *const c_void,
                    &mut deadline,
                    &mut offset,
                );
            }
        } else if #[cfg(simd_backend = "neon")] {
            unsafe {
                find_best_deadline_neon(
                    scoops.as_ptr() as *mut c_void,
                    nonce_count,
                    gensig.as_ptr() as *const c_void,
                    &mut deadline,
                    &mut offset,
                );
            }
        } else {
            let result = crate::poc_hashing::find_best_deadline_rust(scoops, nonce_count, gensig);
            deadline = result.0;
            offset = result.1;
        }
    }
    (deadline, offset)
}

pub fn create_cpu_worker_task(
    benchmark: bool,
    thread_pool: rayon::ThreadPool,
    rx_read_replies: Receiver<ReadReply>,
    tx_empty_buffers: Sender<Box<dyn Buffer + Send>>,
    tx_nonce_data: TokioSender<NonceData>,
) -> impl FnOnce() + Send + 'static {
    move || {
        for read_reply in rx_read_replies {
            let task = hash(
                read_reply,
                tx_empty_buffers.clone(),
                tx_nonce_data.clone(),
                benchmark,
            );

            thread_pool.spawn(task);
        }
    }
}

pub fn hash(
    read_reply: ReadReply,
    tx_empty_buffers: Sender<Box<dyn Buffer + Send>>,
    tx_nonce_data: TokioSender<NonceData>,
    benchmark: bool,
) -> impl FnOnce() + Send + 'static {
    move || {
        let mut buffer = read_reply.buffer;

        if read_reply.info.len == 1 && read_reply.info.gpu_signal > 0 {
            return;
        }

        // Every chunk produces exactly one result, empty or not, so the miner
        // can account for all of them before declaring a round finished.
        if read_reply.info.len == 0 || benchmark {
            let _ = tx_nonce_data.blocking_send(NonceData {
                height: read_reply.info.height,
                block: read_reply.info.block,
                base_target: read_reply.info.base_target,
                deadline: u64::MAX,
                nonce: 0,
                reader_task_processed: read_reply.info.finished,
                drive_chunks: read_reply.info.drive_chunks,
                account_id: read_reply.info.account_id,
            });
            let _ = tx_empty_buffers.send(buffer);
            return;
        }

        let (deadline, offset) = {
            let bs = buffer.get_buffer_for_writing();
            let bs = lock_or_recover(&bs);
            find_best_deadline(
                &bs,
                (read_reply.info.len as u64) / SCOOP_SIZE,
                &read_reply.info.gensig,
            )
        };

        let _ = tx_nonce_data.blocking_send(NonceData {
            height: read_reply.info.height,
            block: read_reply.info.block,
            base_target: read_reply.info.base_target,
            deadline,
            nonce: offset.saturating_add(read_reply.info.start_nonce),
            reader_task_processed: read_reply.info.finished,
            drive_chunks: read_reply.info.drive_chunks,
            account_id: read_reply.info.account_id,
        });

        let _ = tx_empty_buffers.send(buffer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::poc_hashing::find_best_deadline_rust;
    use crate::test_support::{deadline_for_scoop, generate_nonce, scoop_of};

    fn test_gensig() -> [u8; 32] {
        let gensig =
            hex::decode("4a6f686e6e7946464d206861742064656e206772f6df74656e2050656e697321")
                .unwrap();
        let mut gensig_array = [0u8; 32];
        gensig_array.copy_from_slice(&gensig[..]);
        gensig_array
    }

    #[test]
    fn test_deadline_hashing() {
        let gensig_array = test_gensig();

        let winner: [u8; 64] = [0; 64];
        let loser: [u8; 64] = [5; 64];
        let mut data: [u8; 64 * 32] = [5; 64 * 32];

        for i in 0..32 {
            data[i * 64..i * 64 + 64].clone_from_slice(&winner);
            let result = find_best_deadline_rust(&data, (i + 1) as u64, &gensig_array);
            assert_eq!(3084580316385335914u64, result.0);
            assert_eq!(i as u64, result.1);
            data[i * 64..i * 64 + 64].clone_from_slice(&loser);
        }
    }

    /// The compiled backend (SIMD C code or portable Rust) must agree with the
    /// independent oracle on real plot data, for every chunk length including
    /// the SIMD remainder lanes.
    #[test]
    fn compiled_backend_matches_oracle_on_real_scoops() {
        init_backend();
        let gensig = test_gensig();
        let account = 10282355196851764065u64;
        let scoop = 1337u32;
        let nonces: Vec<Vec<u8>> = (0..37).map(|n| generate_nonce(account, n)).collect();
        let mut data = Vec::new();
        for n in &nonces {
            data.extend_from_slice(scoop_of(n, scoop));
        }

        for count in 1..=37u64 {
            let mut best = (u64::MAX, 0u64);
            for (i, n) in nonces.iter().enumerate().take(count as usize) {
                let dl = deadline_for_scoop(&gensig, scoop_of(n, scoop));
                if dl < best.0 {
                    best = (dl, i as u64);
                }
            }
            assert_eq!(
                find_best_deadline(&data, count, &gensig),
                best,
                "backend {} disagrees with oracle for {} nonces",
                backend_name(),
                count
            );
        }
    }
}
