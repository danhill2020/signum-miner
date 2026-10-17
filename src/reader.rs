use crate::buffer::AlignedBuffer;
use crate::metrics::SharedDiskHealth;
use crate::miner::Buffer;
#[cfg(feature = "opencl")]
use crate::miner::CpuBuffer;
use crate::plot::{Meta, Plot};
use crate::utils::{lock_or_recover, new_thread_pool, write_or_recover};
use crossbeam_channel::{Receiver, Sender};
use pbr::{ProgressBar, Units};
use rayon::prelude::*;
use std::collections::HashMap;
use std::io::Stdout;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct BufferInfo {
    pub len: usize,
    pub height: u64,
    pub block: u64,
    pub base_target: u64,
    pub gensig: Arc<[u8; 32]>,
    pub start_nonce: u64,
    pub finished: bool,
    pub account_id: u64,
    pub gpu_signal: u64,
    /// On the reply that carries `finished`, the number of replies this
    /// drive produced this round (that one included).  0 otherwise.
    pub drive_chunks: u64,
}

pub struct ReadReply {
    pub buffer: Box<dyn Buffer + Send>,
    pub info: BufferInfo,
}

pub type DriveMap = HashMap<String, Arc<Vec<Mutex<Plot>>>>;

pub struct Reader {
    drive_id_to_plots: DriveMap,
    pub total_size: u64,
    pool: rayon::ThreadPool,
    /// `hdd_reader_thread_count` from the config; 0 means one thread per drive.
    configured_threads: usize,
    pool_threads: usize,
    thread_pinning: bool,
    rx_empty_buffers: Receiver<Box<dyn Buffer + Send>>,
    tx_empty_buffers: Sender<Box<dyn Buffer + Send>>,
    tx_read_replies_cpu: Sender<ReadReply>,
    #[allow(dead_code)]
    tx_read_replies_gpu: Option<Vec<Sender<ReadReply>>>,
    interrupts: Vec<Sender<()>>,
    show_progress: bool,
    show_drive_stats: bool,
    disk_health: SharedDiskHealth,
}

/// Everything a per-drive read task needs, bundled so the task body stays readable.
struct ReadTaskContext {
    drive: String,
    plots: Arc<Vec<Mutex<Plot>>>,
    height: u64,
    block: u64,
    base_target: u64,
    scoop: u32,
    gensig: Arc<[u8; 32]>,
    show_drive_stats: bool,
    pb: Option<Arc<Mutex<ProgressBar<Stdout>>>>,
    rx_interrupt: Receiver<()>,
    rx_empty_buffers: Receiver<Box<dyn Buffer + Send>>,
    tx_empty_buffers: Sender<Box<dyn Buffer + Send>>,
    tx_read_replies_cpu: Sender<ReadReply>,
    #[allow(dead_code)]
    tx_read_replies_gpu: Option<Vec<Sender<ReadReply>>>,
    disk_health: SharedDiskHealth,
}

fn pool_size_for(configured_threads: usize, drive_count: usize) -> usize {
    if configured_threads == 0 {
        drive_count.max(1)
    } else {
        configured_threads
    }
}

impl Reader {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        drive_id_to_plots: DriveMap,
        total_size: u64,
        num_threads: usize,
        rx_empty_buffers: Receiver<Box<dyn Buffer + Send>>,
        tx_empty_buffers: Sender<Box<dyn Buffer + Send>>,
        tx_read_replies_cpu: Sender<ReadReply>,
        tx_read_replies_gpu: Option<Vec<Sender<ReadReply>>>,
        show_progress: bool,
        show_drive_stats: bool,
        thread_pinning: bool,
        benchmark: bool,
        disk_health: SharedDiskHealth,
    ) -> Reader {
        if !benchmark {
            check_overlap(&drive_id_to_plots);
        }

        let pool_threads = pool_size_for(num_threads, drive_id_to_plots.len());
        Reader {
            drive_id_to_plots,
            total_size,
            pool: new_thread_pool(pool_threads, thread_pinning),
            configured_threads: num_threads,
            pool_threads,
            thread_pinning,
            rx_empty_buffers,
            tx_empty_buffers,
            tx_read_replies_cpu,
            tx_read_replies_gpu,
            interrupts: Vec::new(),
            show_progress,
            show_drive_stats,
            disk_health,
        }
    }

    /// Number of drives, which is also the number of "drive finished"
    /// signals the miner has to wait for before a round is complete.
    pub fn drive_count(&self) -> usize {
        self.drive_id_to_plots.len()
    }

    /// Current plots per drive (shared handles, cheap to clone).
    pub fn drives(&self) -> DriveMap {
        self.drive_id_to_plots.clone()
    }

    pub fn start_reading(
        &mut self,
        height: u64,
        block: u64,
        base_target: u64,
        scoop: u32,
        gensig: &Arc<[u8; 32]>,
    ) {
        for interrupt in &self.interrupts {
            interrupt.send(()).ok();
        }

        let pb = if self.show_progress && self.total_size > 0 {
            let mut pb = ProgressBar::new(self.total_size);
            pb.format("│██░│");
            pb.set_width(Some(80));
            pb.set_units(Units::Bytes);
            pb.set_max_refresh_rate(Some(Duration::from_millis(250)));
            pb.message("Searching your hashes: ");
            Some(Arc::new(Mutex::new(pb)))
        } else {
            None
        };

        // send start signals (dummy buffer) to gpu threads
        #[cfg(feature = "opencl")]
        if let Some(txs) = &self.tx_read_replies_gpu {
            for tx in txs {
                if let Err(e) = tx.send(ReadReply {
                    buffer: Box::new(CpuBuffer::new(0)) as Box<dyn Buffer + Send>,
                    info: BufferInfo {
                        len: 1,
                        height,
                        block,
                        base_target,
                        gensig: gensig.clone(),
                        start_nonce: 0,
                        finished: false,
                        account_id: 0,
                        gpu_signal: 1,
                        drive_chunks: 0,
                    },
                }) {
                    error!(
                        "reader: failed to send 'round start' signal to GPU thread: {}",
                        e
                    );
                }
            }
        }

        self.interrupts = self
            .drive_id_to_plots
            .iter()
            .map(|(drive, plots)| {
                let (tx_interrupt, rx_interrupt) = crossbeam_channel::unbounded();
                let ctx = ReadTaskContext {
                    drive: drive.clone(),
                    plots: plots.clone(),
                    height,
                    block,
                    base_target,
                    scoop,
                    gensig: gensig.clone(),
                    show_drive_stats: self.show_drive_stats,
                    pb: pb.clone(),
                    rx_interrupt,
                    rx_empty_buffers: self.rx_empty_buffers.clone(),
                    tx_empty_buffers: self.tx_empty_buffers.clone(),
                    tx_read_replies_cpu: self.tx_read_replies_cpu.clone(),
                    tx_read_replies_gpu: self.tx_read_replies_gpu.clone(),
                    disk_health: self.disk_health.clone(),
                };
                self.pool.spawn(move || read_drive(ctx));
                tx_interrupt
            })
            .collect();
    }

    /// Issue one real read on every drive so external enclosures do not spin down.
    pub fn wakeup(&mut self) {
        for plots in self.drive_id_to_plots.values() {
            let plots = plots.clone();
            self.pool.spawn(move || {
                let Some(first) = plots.first() else {
                    return;
                };
                let mut p = lock_or_recover(first);
                if let Err(e) = p.wakeup() {
                    error!("wakeup: error during wakeup {}: {}", p.meta.name, e);
                }
            });
        }
    }

    pub fn update_plots(&mut self, drive_id_to_plots: DriveMap, total_size: u64, benchmark: bool) {
        if !benchmark {
            check_overlap(&drive_id_to_plots);
        }
        let wanted = pool_size_for(self.configured_threads, drive_id_to_plots.len());
        if wanted > self.pool_threads {
            info!(
                "reader: growing reader thread pool from {} to {} threads",
                self.pool_threads, wanted
            );
            // Tasks from the previous round keep running on the old pool until
            // they finish; the old pool is dropped once they are done.
            self.pool = new_thread_pool(wanted, self.thread_pinning);
            self.pool_threads = wanted;
        }
        self.drive_id_to_plots = drive_id_to_plots;
        self.total_size = total_size;
    }
}

/// Hand a filled (or empty) buffer to the worker responsible for it.
/// Returns false when the receiving side is gone and reading must stop.
fn route_reply(ctx: &ReadTaskContext, reply: ReadReply) -> bool {
    #[cfg(feature = "opencl")]
    {
        let id = reply.buffer.get_id();
        if id > 0 {
            let Some(txs) = ctx.tx_read_replies_gpu.as_ref() else {
                error!("reader: GPU buffer without GPU channels -> stopping");
                return false;
            };
            if let Err(e) = txs[id - 1].send(reply) {
                error!(
                    "reader: failed to send read data to GPU thread: {} -> stopping",
                    e
                );
                return false;
            }
            return true;
        }
    }
    if let Err(e) = ctx.tx_read_replies_cpu.send(reply) {
        error!(
            "reader: failed to send read data to CPU thread: {} -> stopping",
            e
        );
        return false;
    }
    true
}

#[cfg(feature = "opencl")]
fn signal_gpu_drive_finished(ctx: &ReadTaskContext) {
    if let Some(txs) = ctx.tx_read_replies_gpu.as_ref() {
        for tx in txs {
            if let Err(e) = tx.send(ReadReply {
                buffer: Box::new(CpuBuffer::new(0)) as Box<dyn Buffer + Send>,
                info: BufferInfo {
                    len: 1,
                    height: ctx.height,
                    block: ctx.block,
                    base_target: ctx.base_target,
                    gensig: ctx.gensig.clone(),
                    start_nonce: 0,
                    finished: false,
                    account_id: 0,
                    gpu_signal: 2,
                    drive_chunks: 0,
                },
            }) {
                error!(
                    "reader: failed to send 'drive finished' signal to GPU thread: {}",
                    e
                );
            }
        }
    }
}

fn record_read_result(ctx: &ReadTaskContext, ok: bool) {
    let mut health = write_or_recover(&ctx.disk_health);
    let info = health.get_or_create(&ctx.drive);
    if ok {
        info.record_success();
    } else {
        info.record_failure();
    }
}

/// Read the current scoop of every plot on one drive and hand the chunks to
/// the hashing workers.
///
/// Exactly one reply with `finished == true` is sent per round and drive
/// unless the round is interrupted by a new block, and it carries the number
/// of replies the drive produced.  Every reply turns into exactly one hashing
/// result, so the miner can tell when a round is really complete (all drives
/// done *and* every chunk hashed), even when a plot cannot be opened or read.
fn read_drive(ctx: ReadTaskContext) {
    let mut finished_sent = false;
    let mut interrupted = false;
    let mut nonces_processed = 0u64;
    let mut chunks_sent = 0u64;
    let started = Instant::now();
    let plot_count = ctx.plots.len();

    'outer: for (i_p, plot) in ctx.plots.iter().enumerate() {
        if ctx.rx_interrupt.try_recv().is_ok() {
            interrupted = true;
            break 'outer;
        }

        let mut p = lock_or_recover(plot);
        if let Err(e) = p.prepare(ctx.scoop) {
            error!(
                "reader: error preparing {} for reading: {} -> skip one round",
                p.meta.name, e
            );
            record_read_result(&ctx, false);
            continue 'outer;
        }
        let last_plot = i_p + 1 == plot_count;

        loop {
            // Stop before taking a buffer: when a new block has arrived there
            // is no point in reading (and hashing) another chunk.
            if ctx.rx_interrupt.try_recv().is_ok() {
                interrupted = true;
                break 'outer;
            }
            let Ok(mut buffer) = ctx.rx_empty_buffers.recv() else {
                // Buffer pool is gone: the miner is shutting down.
                break 'outer;
            };
            if ctx.rx_interrupt.try_recv().is_ok() {
                buffer.unmap();
                if let Err(e) = ctx.tx_empty_buffers.send(buffer) {
                    error!("reader: failed to return buffer to pool: {}", e);
                }
                interrupted = true;
                break 'outer;
            }

            let (bytes_read, start_nonce, next_plot) = {
                let bs = buffer.get_buffer_for_writing();
                let mut bs = lock_or_recover(&bs);
                match p.read(&mut bs) {
                    Ok(x) => {
                        record_read_result(&ctx, true);
                        x
                    }
                    Err(e) => {
                        error!(
                            "reader: error reading chunk from {}: {} -> skip one round",
                            p.meta.name, e
                        );
                        record_read_result(&ctx, false);
                        (0, 0, true)
                    }
                }
            };

            if ctx.rx_interrupt.try_recv().is_ok() {
                buffer.unmap();
                if let Err(e) = ctx.tx_empty_buffers.send(buffer) {
                    error!("reader: failed to return buffer to pool: {}", e);
                }
                interrupted = true;
                break 'outer;
            }

            let finished = last_plot && next_plot;
            chunks_sent += 1;
            let reply = ReadReply {
                buffer,
                info: BufferInfo {
                    len: bytes_read,
                    height: ctx.height,
                    block: ctx.block,
                    base_target: ctx.base_target,
                    gensig: ctx.gensig.clone(),
                    start_nonce,
                    finished,
                    account_id: p.meta.account_id,
                    gpu_signal: 0,
                    drive_chunks: if finished { chunks_sent } else { 0 },
                },
            };
            if !route_reply(&ctx, reply) {
                break 'outer;
            }
            if finished {
                finished_sent = true;
            }

            nonces_processed += bytes_read as u64 / 64;
            if let Some(pb) = &ctx.pb {
                lock_or_recover(pb).add(bytes_read as u64);
            }

            if finished {
                #[cfg(feature = "opencl")]
                signal_gpu_drive_finished(&ctx);
                let elapsed_ms = started.elapsed().as_millis().max(1) as u64;
                write_or_recover(&ctx.disk_health)
                    .get_or_create(&ctx.drive)
                    .record_round(elapsed_ms, nonces_processed * 64);
                if ctx.show_drive_stats {
                    info!(
                        "{: <80}",
                        format!(
                            "drive {} finished, speed={} MiB/s",
                            ctx.drive,
                            nonces_processed * 64 * 1000 / elapsed_ms / 1024 / 1024,
                        )
                    );
                }
            }

            if next_plot {
                break;
            }
        }
    }

    if !interrupted && !finished_sent {
        // Every plot on this drive failed before producing a final chunk.
        // Close the round out for this drive anyway so the miner does not wait
        // forever (and never submits its best deadline).
        warn!(
            "reader: drive {} produced no data this round, reporting it as finished",
            ctx.drive
        );
        if let Ok(buffer) = ctx.rx_empty_buffers.recv() {
            buffer.unmap();
            let reply = ReadReply {
                buffer,
                info: BufferInfo {
                    len: 0,
                    height: ctx.height,
                    block: ctx.block,
                    base_target: ctx.base_target,
                    gensig: ctx.gensig.clone(),
                    start_nonce: 0,
                    finished: true,
                    account_id: 0,
                    gpu_signal: 0,
                    drive_chunks: chunks_sent + 1,
                },
            };
            route_reply(&ctx, reply);
            #[cfg(feature = "opencl")]
            signal_gpu_drive_finished(&ctx);
        }
    }
}

/// Reports (and returns) whether any two plots of the same account overlap.
pub fn check_overlap(drive_id_to_plots: &DriveMap) -> bool {
    let plots: Vec<Meta> = drive_id_to_plots
        .values()
        .flat_map(|a| a.iter())
        .map(|plot| lock_or_recover(plot).meta.clone())
        .collect();
    plots
        .par_iter()
        .enumerate()
        .filter(|(i, plot_a)| {
            plots[i + 1..]
                .par_iter()
                .filter(|plot_b| {
                    plot_a.account_id == plot_b.account_id && plot_b.overlaps_with(plot_a)
                })
                .count()
                > 0
        })
        .count()
        > 0
}

// Keep the buffer type visible to callers that only import this module.
#[allow(dead_code)]
pub type ReadBuffer = AlignedBuffer;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_size_follows_drive_count_when_auto() {
        assert_eq!(pool_size_for(0, 0), 1);
        assert_eq!(pool_size_for(0, 3), 3);
        assert_eq!(pool_size_for(2, 5), 2);
    }
}
