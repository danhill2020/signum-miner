use crate::miner::Buffer;
#[cfg(feature = "opencl")]
use crate::miner::CpuBuffer;
use crate::plot::{Meta, Plot};
use crate::utils::new_thread_pool;
use crossbeam_channel;
use crossbeam_channel::{Receiver, Sender};
use pbr::{ProgressBar, Units};
use rayon::prelude::*;
use std::collections::HashMap;
use std::io::Stdout;
use std::sync::Arc;
#[cfg(feature = "async_io")]
use tokio::sync::Mutex;
#[cfg(not(feature = "async_io"))]
use std::sync::Mutex;
use stopwatch::Stopwatch;

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
}
pub struct ReadReply {
    pub buffer: Box<dyn Buffer + Send>,
    pub info: BufferInfo,
}

#[allow(dead_code)]
pub struct Reader {
    drive_id_to_plots: HashMap<String, Arc<Vec<Mutex<Plot>>>>,
    pub total_size: u64,
    pool: rayon::ThreadPool,
    rx_empty_buffers: Receiver<Box<dyn Buffer + Send>>,
    tx_empty_buffers: Sender<Box<dyn Buffer + Send>>,
    tx_read_replies_cpu: Sender<ReadReply>,
    tx_read_replies_gpu: Option<Vec<Sender<ReadReply>>>,
    interupts: Vec<Sender<()>>,
    show_progress: bool,
    show_drive_stats: bool,
}

impl Reader {
    pub fn new(
        drive_id_to_plots: HashMap<String, Arc<Vec<Mutex<Plot>>>>,
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
    ) -> Reader {
        if !benchmark {
            check_overlap(&drive_id_to_plots);
        }

        Reader {
            drive_id_to_plots,
            total_size,
            pool: new_thread_pool(num_threads, thread_pinning),
            rx_empty_buffers,
            tx_empty_buffers,
            tx_read_replies_cpu,
            tx_read_replies_gpu,
            interupts: Vec::new(),
            show_progress,
            show_drive_stats,
        }
    }

    pub fn start_reading(
        &mut self,
        height: u64,
        block: u64,
        base_target: u64,
        scoop: u32,
        gensig: &Arc<[u8; 32]>,
    ) {
        for interupt in &self.interupts {
            interupt.send(()).ok();
        }
        let mut pb = ProgressBar::new(self.total_size);
        pb.format("│██░│");
        pb.set_width(Some(80));
        pb.set_units(Units::Bytes);
        pb.message("Searching your hashes: ");
        let pb = Arc::new(Mutex::new(pb));

        // send start signals (dummy buffer) to gpu threads
        #[cfg(feature = "opencl")]
        for i in 0..self.tx_read_replies_gpu.as_ref().unwrap().len() {
            if let Err(e) = self.tx_read_replies_gpu.as_ref().unwrap()[i].send(ReadReply {
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
                },
            }) {
                error!("reader: failed to send 'round start' signal to GPU thread: {}", e);
            }
        }

        self.interupts = self
            .drive_id_to_plots
            .iter()
            .map(|(drive, plots)| {
                let (interupt, task) = if self.show_progress {
                    self.create_read_task(
                        Some(pb.clone()),
                        drive.clone(),
                        plots.clone(),
                        height,
                        block,
                        base_target,
                        scoop,
                        gensig.clone(),
                        self.show_drive_stats,
                    )
                } else {
                    self.create_read_task(
                        None,
                        drive.clone(),
                        plots.clone(),
                        height,
                        block,
                        base_target,
                        scoop,
                        gensig.clone(),
                        self.show_drive_stats,
                    )
                };

                self.pool.spawn(task);
                interupt
            })
            .collect();
    }

    pub fn wakeup(&mut self) {
        for plots in self.drive_id_to_plots.values() {
            let plots = plots.clone();
            self.pool.spawn(move || {
#[cfg(feature = "async_io")]
                let mut p = plots[0].blocking_lock();
#[cfg(not(feature = "async_io"))]
                let mut p = match plots[0].lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => {
                        error!("wakeup: mutex poisoned, recovering...");
                        poisoned.into_inner()
                    }
                };

                if let Err(e) = p.seek_random() {
                    error!(
                        "wakeup: error during wakeup {}: {} -> skip one round",
                        p.meta.name, e
                    );
                }
            });
        }
    }

    pub fn update_plots(
        &mut self,
        drive_id_to_plots: HashMap<String, Arc<Vec<Mutex<Plot>>>>,
        total_size: u64,
        benchmark: bool,
    ) {
        if !benchmark {
            check_overlap(&drive_id_to_plots);
        }
        self.drive_id_to_plots = drive_id_to_plots;
        self.total_size = total_size;
    }

    #[cfg(not(feature = "async_io"))]
    fn create_read_task(
        &self,
        pb: Option<Arc<Mutex<pbr::ProgressBar<Stdout>>>>,
        drive: String,
        plots: Arc<Vec<Mutex<Plot>>>,
        height: u64,
        block: u64,
        base_target: u64,
        scoop: u32,
        gensig: Arc<[u8; 32]>,
        show_drive_stats: bool,
    ) -> (Sender<()>, impl FnOnce()) {
        let (tx_interupt, rx_interupt) = crossbeam_channel::unbounded();
        let rx_empty_buffers = self.rx_empty_buffers.clone();
        let tx_empty_buffers = self.tx_empty_buffers.clone();
        let tx_read_replies_cpu = self.tx_read_replies_cpu.clone();
        #[cfg(feature = "opencl")]
        let tx_read_replies_gpu = self.tx_read_replies_gpu.clone();

        (tx_interupt, move || {
            let mut sw = Stopwatch::new();
            let mut elapsed = 0i64;
            let mut nonces_processed = 0u64;
            let plot_count = plots.len();
            'outer: for (i_p, p) in plots.iter().enumerate() {
                let mut p = match p.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => {
                        error!("reader: mutex poisoned for plot, recovering...");
                        poisoned.into_inner()
                    }
                };
                if let Err(e) = p.prepare(scoop) {
                    error!(
                        "reader: error preparing {} for reading: {} -> skip one round",
                        p.meta.name, e
                    );
                    continue 'outer;
                }

                'inner: for mut buffer in rx_empty_buffers.clone() {
                    if show_drive_stats {
                        sw.restart();
                    }
                    let mut_bs = buffer.get_buffer_for_writing();
                    let mut bs = match mut_bs.lock() {
                        Ok(guard) => guard,
                        Err(poisoned) => {
                            error!("reader: buffer mutex poisoned, recovering...");
                            poisoned.into_inner()
                        }
                    };
                    let (bytes_read, start_nonce, next_plot) = match p.read(&mut bs, scoop) {
                        Ok(x) => x,
                        Err(e) => {
                            error!(
                                "reader: error reading chunk from {}: {} -> skip one round",
                                p.meta.name, e
                            );
                            buffer.unmap();
                            (0, 0, true)
                        }
                    };

                    if rx_interupt.try_recv().is_ok() {
                        buffer.unmap();
                        if let Err(e) = tx_empty_buffers.send(buffer) {
                            error!("reader: failed to return buffer to pool: {} -> stopping", e);
                        }
                        break 'outer;
                    }

                    let finished = i_p == (plot_count - 1) && next_plot;
                    // buffer routing
                    #[cfg(feature = "opencl")]
                    match buffer.get_id() {
                        0 => {
                            if let Err(e) = tx_read_replies_cpu.send(ReadReply {
                                buffer,
                                info: BufferInfo {
                                    len: bytes_read,
                                    height,
                                    block,
                                    base_target,
                                    gensig: gensig.clone(),
                                    start_nonce,
                                    finished,
                                    account_id: p.meta.account_id,
                                    gpu_signal: 0,
                                },
                            }) {
                                error!("reader: failed to send read data to CPU thread: {} -> stopping", e);
                                break 'outer;
                            }
                        }
                        i => {
                            if let Err(e) = tx_read_replies_gpu.as_ref().unwrap()[i - 1].send(ReadReply {
                                buffer,
                                info: BufferInfo {
                                    len: bytes_read,
                                    height,
                                    block,
                                    base_target,
                                    gensig: gensig.clone(),
                                    start_nonce,
                                    finished,
                                    account_id: p.meta.account_id,
                                    gpu_signal: 0,
                                },
                            }) {
                                error!("reader: failed to send read data to GPU thread: {} -> stopping", e);
                                break 'outer;
                            }
                        }
                    }
                    #[cfg(not(feature = "opencl"))]
                    if let Err(e) = tx_read_replies_cpu.send(ReadReply {
                        buffer,
                        info: BufferInfo {
                            len: bytes_read,
                            height,
                            block,
                            base_target,
                            gensig: gensig.clone(),
                            start_nonce,
                            finished,
                            account_id: p.meta.account_id,
                            gpu_signal: 0,
                        },
                    }) {
                        error!("reader: failed to send read data to CPU thread: {} -> stopping", e);
                        break 'outer;
                    }

                    nonces_processed += bytes_read as u64 / 64;

                    match &pb {
                        Some(pb) => {
                            match pb.lock() {
                                Ok(mut pb) => pb.add(bytes_read as u64),
                                Err(poisoned) => {
                                    error!("reader: progress bar mutex poisoned, recovering...");
                                    let mut pb = poisoned.into_inner();
                                    pb.add(bytes_read as u64);
                                }
                            }
                        }
                        None => (),
                    }

                    if show_drive_stats {
                        elapsed += sw.elapsed_ms();
                    }

                    // send termination signal (dummy buffer) to gpu
                    if finished {
                        #[cfg(feature = "opencl")]
                        for i in 0..tx_read_replies_gpu.as_ref().unwrap().len() {
                            if let Err(e) = tx_read_replies_gpu.as_ref().unwrap()[i].send(ReadReply {
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
                                    gpu_signal: 2,
                                },
                            }) {
                                error!("reader: failed to send 'drive finished' signal to GPU thread: {}", e);
                            }
                        }
                    }

                    if finished && show_drive_stats {
                        info!(
                            "{: <80}",
                            format!(
                                "drive {} finished, speed={} MiB/s",
                                drive,
                                nonces_processed * 1000 / (elapsed + 1) as u64 * 64 / 1024 / 1024,
                            )
                        );
                    }

                    if next_plot {
                        break 'inner;
                    }
                }
            }
        })
    }

    #[cfg(feature = "async_io")]
    fn create_read_task(
        &self,
        pb: Option<Arc<Mutex<pbr::ProgressBar<Stdout>>>>,
        drive: String,
        plots: Arc<Vec<Mutex<Plot>>>,
        height: u64,
        block: u64,
        base_target: u64,
        scoop: u32,
        gensig: Arc<[u8; 32]>,
        show_drive_stats: bool,
    ) -> (Sender<()>, impl FnOnce()) {
        let (tx_interupt, rx_interupt) = crossbeam_channel::unbounded();
        let rx_empty_buffers = self.rx_empty_buffers.clone();
        let tx_empty_buffers = self.tx_empty_buffers.clone();
        let tx_read_replies_cpu = self.tx_read_replies_cpu.clone();
        #[cfg(feature = "opencl")]
        let tx_read_replies_gpu = self.tx_read_replies_gpu.clone();

        (tx_interupt, move || {
            tokio::spawn(async move {
                let mut sw = Stopwatch::new();
                let mut elapsed = 0i64;
                let mut nonces_processed = 0u64;
                let plot_count = plots.len();
                'outer: for (i_p, p) in plots.iter().enumerate() {
                    let mut p = p.lock().await;
                    if let Err(e) = p.prepare_async(scoop).await {
                        error!(
                            "reader: error preparing {} for reading: {} -> skip one round",
                            p.meta.name,
                            e
                        );
                        continue 'outer;
                    }

                    'inner: for mut buffer in rx_empty_buffers.clone() {
                        if show_drive_stats {
                            sw.restart();
                        }
                        let mut_bs = buffer.get_buffer_for_writing();
                        let mut bs = mut_bs.lock().await;
                        let (bytes_read, start_nonce, next_plot) = match p.read_async(&mut bs, scoop).await {
                            Ok(x) => x,
                            Err(e) => {
                                error!(
                                    "reader: error reading chunk from {}: {} -> skip one round",
                                    p.meta.name,
                                    e
                                );
                                buffer.unmap();
                                (0, 0, true)
                            }
                        };

                        if rx_interupt.try_recv().is_ok() {
                            buffer.unmap();
                            if let Err(e) = tx_empty_buffers.send(buffer) {
                                error!("reader: failed to return buffer to pool (async): {} -> stopping", e);
                            }
                            break 'outer;
                        }

                        let finished = i_p == (plot_count - 1) && next_plot;
                        #[cfg(feature = "opencl")]
                        match buffer.get_id() {
                            0 => {
                                if let Err(e) = tx_read_replies_cpu.send(ReadReply {
                                    buffer,
                                    info: BufferInfo {
                                        len: bytes_read,
                                        height,
                                        block,
                                        base_target,
                                        gensig: gensig.clone(),
                                        start_nonce,
                                        finished,
                                        account_id: p.meta.account_id,
                                        gpu_signal: 0,
                                    },
                                }) {
                                    error!("reader: failed to send read data to CPU thread (async): {} -> stopping", e);
                                    break 'outer;
                                }
                            }
                            i => {
                                if let Err(e) = tx_read_replies_gpu.as_ref().unwrap()[i - 1].send(ReadReply {
                                    buffer,
                                    info: BufferInfo {
                                        len: bytes_read,
                                        height,
                                        block,
                                        base_target,
                                        gensig: gensig.clone(),
                                        start_nonce,
                                        finished,
                                        account_id: p.meta.account_id,
                                        gpu_signal: 0,
                                    },
                                }) {
                                    error!("reader: failed to send read data to GPU thread (async): {} -> stopping", e);
                                    break 'outer;
                                }
                            }
                        }
                        #[cfg(not(feature = "opencl"))]
                        if let Err(e) = tx_read_replies_cpu.send(ReadReply {
                            buffer,
                            info: BufferInfo {
                                len: bytes_read,
                                height,
                                block,
                                base_target,
                                gensig: gensig.clone(),
                                start_nonce,
                                finished,
                                account_id: p.meta.account_id,
                                gpu_signal: 0,
                            },
                        }) {
                            error!("reader: failed to send read data to CPU thread (async): {} -> stopping", e);
                            break 'outer;
                        }

                        nonces_processed += bytes_read as u64 / 64;

                        match &pb {
                            Some(pb) => {
                                let mut pb = pb.lock().await;
                                pb.add(bytes_read as u64);
                            }
                            None => (),
                        }

                        if show_drive_stats {
                            elapsed += sw.elapsed_ms();
                        }

                        if finished {
                            #[cfg(feature = "opencl")]
                            for i in 0..tx_read_replies_gpu.as_ref().unwrap().len() {
                                if let Err(e) = tx_read_replies_gpu.as_ref().unwrap()[i].send(ReadReply {
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
                                        gpu_signal: 2,
                                    },
                                }) {
                                    error!("reader: failed to send 'drive finished' signal to GPU thread (async): {}", e);
                                }
                            }
                        }

                        if finished && show_drive_stats {
                            info!(
                                "{: <80}",
                                format!(
                                    "drive {} finished, speed={} MiB/s",
                                    drive,
                                    nonces_processed * 1000 / (elapsed + 1) as u64 * 64 / 1024 / 1024,
                                )
                            );
                        }

                        if next_plot {
                            break 'inner;
                        }
                    }
                }
            });
        })
    }
}

// Don't waste your time striving for perfection; instead, strive for excellence - doing your best.
// let my_best = perfection;
pub fn check_overlap(drive_id_to_plots: &HashMap<String, Arc<Vec<Mutex<Plot>>>>) -> bool {
    let plots: Vec<Meta> = drive_id_to_plots
        .values()
        .map(|a| a.iter())
        .flatten()
        .filter_map(|plot| {
            #[cfg(feature = "async_io")]
            {
                Some(plot.blocking_lock().meta.clone())
            }
            #[cfg(not(feature = "async_io"))]
            {
                match plot.lock() {
                    Ok(guard) => Some(guard.meta.clone()),
                    Err(poisoned) => {
                        error!("check_overlap: mutex poisoned, recovering...");
                        Some(poisoned.into_inner().meta.clone())
                    }
                }
            }
        })
        .collect();
    plots
        .par_iter()
        .enumerate()
        .filter(|(i, plot_a)| {
            plots[i + 1..]
                .par_iter()
                .filter(|plot_b| {
                    plot_a.account_id == plot_b.account_id && plot_b.overlaps_with(&plot_a)
                })
                .count()
                > 0
        })
        .count()
        > 0
}
