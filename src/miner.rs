use crate::buffer::AlignedBuffer;
use crate::com::api::MiningInfoResponse as MiningInfo;
use crate::config::Cfg;
use crate::cpu_worker::create_cpu_worker_task;
use crate::future::interval::Interval;
#[cfg(feature = "opencl")]
use crate::gpu_worker::create_gpu_worker_task;
#[cfg(feature = "opencl")]
use crate::gpu_worker_async::create_gpu_worker_task_async;
use crate::metrics::{new_shared_disk_health, new_shared_metrics, SharedDiskHealth, SharedMetrics};
#[cfg(feature = "opencl")]
use crate::ocl::GpuBuffer;
#[cfg(feature = "opencl")]
use crate::ocl::GpuContext;
use crate::plot::{parse_plot_name, Plot, SCOOP_SIZE};
use crate::poc_hashing;
use crate::reader::{DriveMap, Reader};
use crate::requests::RequestHandler;
use crate::utils::{
    get_bus_type, get_device_id, lock_or_recover, new_thread_pool, read_or_recover,
    write_or_recover, BusType,
};
use futures_util::stream::StreamExt;
#[cfg(feature = "opencl")]
use ocl_core::Mem;
use std::cmp::{max, min};
use std::collections::HashMap;
use std::fs::read_dir;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// How soon to rescan plot directories when the last scan looked unhealthy
/// (missing directories, read errors, or no plots at all).
const UNHEALTHY_RESCAN_INTERVAL: Duration = Duration::from_secs(60);

pub struct Miner {
    plot_dirs: Vec<PathBuf>,
    hdd_use_direct_io: bool,
    benchmark_cpu: bool,
    capacity_check_interval: Duration,
    reader: Arc<Mutex<Reader>>,
    request_handler: RequestHandler,
    rx_nonce_data: Option<mpsc::Receiver<NonceData>>,
    target_deadline: u64,
    account_id_to_target_deadline: HashMap<u64, u64>,
    state: Arc<Mutex<State>>,
    get_mining_info_interval: u64,
    wakeup_after: Option<Duration>,
    submit_only_best: bool,
    metrics: SharedMetrics,
    disk_health: SharedDiskHealth,
}

pub struct State {
    generation_signature: String,
    generation_signature_bytes: [u8; 32],
    height: u64,
    block: u64,
    account_id_to_best_deadline: HashMap<u64, u64>,
    server_target_deadline: u64,
    base_target: u64,
    round_started: Instant,
    scanning: bool,
    /// Drives that reported their last chunk this round.
    processed_reader_tasks: usize,
    /// Chunks announced by finished drives, and chunks whose result arrived.
    chunks_expected: u64,
    chunks_seen: u64,
    /// Number of drives taking part in the current round.
    reader_task_count: usize,
    /// Bytes read per round (one scoop across all plots) at round start.
    round_bytes: u64,
    scoop: u32,
    first: bool,
    outage: bool,
    pending_capacity_drop: Option<u64>,
}

impl State {
    fn new() -> Self {
        Self {
            generation_signature: "".to_owned(),
            height: 0,
            block: 0,
            scoop: 0,
            account_id_to_best_deadline: HashMap::new(),
            server_target_deadline: u64::MAX,
            base_target: 1,
            processed_reader_tasks: 0,
            chunks_expected: 0,
            chunks_seen: 0,
            reader_task_count: 0,
            round_bytes: 0,
            round_started: Instant::now(),
            generation_signature_bytes: [0; 32],
            scanning: false,
            first: true,
            outage: false,
            pending_capacity_drop: None,
        }
    }

    /// Apply a new block.  Returns `false` (and changes nothing) when the
    /// mining info is unusable.
    fn update_mining_info(&mut self, mining_info: &MiningInfo) -> bool {
        let Some(gensig_bytes) = poc_hashing::decode_gensig(&mining_info.generation_signature)
        else {
            error!(
                "ignoring mining info with invalid generation signature '{}'",
                mining_info.generation_signature
            );
            return false;
        };
        if mining_info.base_target == 0 {
            error!("ignoring mining info with base target 0");
            return false;
        }

        for best_deadlines in self.account_id_to_best_deadline.values_mut() {
            *best_deadlines = u64::MAX;
        }
        self.height = mining_info.height;
        self.block += 1;
        self.base_target = mining_info.base_target;
        self.server_target_deadline = mining_info.target_deadline;
        self.generation_signature_bytes = gensig_bytes;
        self.generation_signature = mining_info.generation_signature.clone();

        let scoop = poc_hashing::calculate_scoop(mining_info.height, &gensig_bytes);
        info!(
            "{: <80}",
            format!("new block: height={}, scoop={}", mining_info.height, scoop)
        );
        self.scoop = scoop;

        self.round_started = Instant::now();
        self.processed_reader_tasks = 0;
        self.chunks_expected = 0;
        self.chunks_seen = 0;
        self.scanning = true;
        true
    }

    /// All drives reported their last chunk and every chunk's result is in.
    fn round_complete(&self) -> bool {
        self.scanning
            && self.processed_reader_tasks >= self.reader_task_count
            && self.chunks_seen >= self.chunks_expected
    }
}

#[derive(Copy, Clone, Debug)]
pub struct NonceData {
    pub height: u64,
    pub block: u64,
    pub base_target: u64,
    pub deadline: u64,
    pub nonce: u64,
    /// This result belongs to the last chunk of a drive for this round.
    pub reader_task_processed: bool,
    /// With `reader_task_processed`: how many chunks that drive produced.
    pub drive_chunks: u64,
    pub account_id: u64,
}

pub trait Buffer {
    fn get_buffer_for_writing(&mut self) -> Arc<Mutex<AlignedBuffer>>;
    #[cfg(feature = "opencl")]
    fn get_gpu_buffers(&self) -> Option<&GpuBuffer>;
    #[cfg(feature = "opencl")]
    fn get_gpu_data(&self) -> Option<Mem>;
    fn unmap(&self);
    #[allow(dead_code)]
    fn get_id(&self) -> usize;
}

pub struct CpuBuffer {
    data: Arc<Mutex<AlignedBuffer>>,
}

impl CpuBuffer {
    pub fn new(buffer_size: usize) -> Self {
        CpuBuffer {
            data: Arc::new(Mutex::new(AlignedBuffer::new(buffer_size))),
        }
    }
}

impl Buffer for CpuBuffer {
    fn get_buffer_for_writing(&mut self) -> Arc<Mutex<AlignedBuffer>> {
        self.data.clone()
    }
    #[cfg(feature = "opencl")]
    fn get_gpu_buffers(&self) -> Option<&GpuBuffer> {
        None
    }
    #[cfg(feature = "opencl")]
    fn get_gpu_data(&self) -> Option<Mem> {
        None
    }
    fn unmap(&self) {}
    fn get_id(&self) -> usize {
        0
    }
}

fn looks_like_plot_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(parse_plot_name)
        .is_some()
}

/// Result of scanning the configured plot directories.
pub struct PlotScan {
    pub drives: DriveMap,
    /// Bytes read per round: nonces * 64.
    pub round_bytes: u64,
    pub had_errors: bool,
    pub had_empty_dir: bool,
    pub plot_count: usize,
}

impl PlotScan {
    pub fn is_healthy(&self) -> bool {
        !self.had_errors && !self.had_empty_dir && self.plot_count > 0
    }
}

fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 && bytes % (1024 * 1024) == 0 {
        format!("{} MiB", bytes / 1024 / 1024)
    } else if bytes >= 1024 && bytes % 1024 == 0 {
        format!("{} KiB", bytes / 1024)
    } else {
        format!("{} B", bytes)
    }
}

fn tib(round_bytes: u64) -> f64 {
    // round_bytes = nonces * 64; a nonce is 256 KiB.
    (round_bytes / SCOOP_SIZE) as f64 / 4.0 / 1024.0 / 1024.0
}

fn modification_time(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn scan_plots(plot_dirs: &[PathBuf], use_direct_io: bool, dummy: bool) -> PlotScan {
    let mut drive_id_to_plots: HashMap<String, Vec<Mutex<Plot>>> = HashMap::new();
    let mut global_capacity: u64 = 0;
    let mut scan_had_errors = false;
    let mut had_empty_plot_dir = false;
    let mut plot_count = 0usize;

    for plot_dir in plot_dirs {
        let dir_str = plot_dir.to_string_lossy();
        let bus_type = get_bus_type(&dir_str);
        let mut num_plots = 0;
        let mut local_capacity: u64 = 0;
        let mut direct_io_plots = 0;
        match read_dir(plot_dir) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(entry) => {
                            let file = entry.path();
                            if !looks_like_plot_file(&file) {
                                debug!("ignoring {}: not a plot file name", file.display());
                                continue;
                            }
                            match Plot::new(&file, use_direct_io, dummy) {
                                Ok(p) => {
                                    let drive_id = get_device_id(&file.to_string_lossy());
                                    let plots = drive_id_to_plots.entry(drive_id).or_default();
                                    local_capacity += p.meta.nonces;
                                    if p.uses_direct_io() {
                                        direct_io_plots += 1;
                                    }
                                    plots.push(Mutex::new(p));
                                    num_plots += 1;
                                }
                                Err(e) => {
                                    scan_had_errors = true;
                                    warn!("failed to load plot {}: {}", file.display(), e);
                                }
                            }
                        }
                        Err(e) => {
                            scan_had_errors = true;
                            warn!("failed to read entry in {}: {}", dir_str, e);
                        }
                    }
                }
            }
            Err(e) => {
                scan_had_errors = true;
                warn!("could not read dir {}: {}", dir_str, e);
                continue;
            }
        }

        let bus_label = match bus_type {
            BusType::Unknown => String::new(),
            other => format!(" ({})", other.label()),
        };
        info!(
            "path={}, files={}, size={:.4} TiB{}{}",
            dir_str,
            num_plots,
            local_capacity as f64 / 4.0 / 1024.0 / 1024.0,
            bus_label,
            if num_plots > 0 && direct_io_plots == 0 && use_direct_io {
                ", buffered I/O"
            } else {
                ""
            }
        );

        global_capacity += local_capacity;
        plot_count += num_plots;
        if num_plots == 0 {
            had_empty_plot_dir = true;
            warn!("no plots in {}", dir_str);
        }
    }

    // Newest plots first so a freshly plotted file is scanned at the start of
    // a round, then wrap everything in Arcs for the reader threads.
    let drives: DriveMap = drive_id_to_plots
        .drain()
        .map(|(drive_id, mut plots)| {
            plots.sort_by_key(|p| std::cmp::Reverse(modification_time(&lock_or_recover(p).path)));
            (drive_id, Arc::new(plots))
        })
        .collect();

    info!(
        "plot files loaded: total drives={}, total capacity={:.4} TiB",
        drives.len(),
        global_capacity as f64 / 4.0 / 1024.0 / 1024.0
    );

    PlotScan {
        drives,
        round_bytes: global_capacity * SCOOP_SIZE,
        had_errors: scan_had_errors,
        had_empty_dir: had_empty_plot_dir,
        plot_count,
    }
}

fn should_apply_capacity_update(
    old_size: u64,
    new_size: u64,
    scan_had_errors: bool,
    had_empty_plot_dir: bool,
) -> bool {
    if new_size >= old_size {
        return true;
    }

    // Protect against temporary drive outages or mount hiccups reducing reported capacity.
    // If the scan had filesystem errors, only accept equal or higher capacity.
    if scan_had_errors {
        return false;
    }

    // A mount hiccup can leave a configured plot directory temporarily empty without an I/O error.
    // Treat large drops (>= 25%) combined with empty plot dirs as suspicious and keep prior capacity.
    let is_large_drop = old_size > 0 && new_size.saturating_mul(100) < old_size.saturating_mul(75);
    !(had_empty_plot_dir && is_large_drop)
}

/// Capacity in GiB as reported to pools/proxies in the `X-Capacity` header.
fn capacity_gib(round_bytes: u64) -> usize {
    (round_bytes * 4 / 1024 / 1024) as usize
}

impl Miner {
    pub fn new(cfg: Cfg, executor: Handle) -> Result<Miner, String> {
        let scan = scan_plots(&cfg.plot_dirs, cfg.hdd_use_direct_io, cfg.benchmark_cpu());
        if scan.plot_count == 0 {
            error!(
                "no plot files found in any configured plot directory; will keep rescanning every {}s",
                UNHEALTHY_RESCAN_INTERVAL.as_secs()
            );
        }

        let cpu_threads = cfg.cpu_threads.max(1);
        let cpu_worker_task_count = cfg.cpu_worker_task_count;
        let cpu_buffer_count = if cpu_worker_task_count > 0 {
            cpu_worker_task_count + cpu_threads
        } else {
            0
        };

        #[cfg(feature = "opencl")]
        let gpu_worker_task_count = cfg.gpu_worker_task_count;
        #[cfg(feature = "opencl")]
        let gpu_threads = cfg.gpu_threads;
        #[cfg(feature = "opencl")]
        let gpu_buffer_count = if gpu_worker_task_count > 0 {
            if cfg.gpu_async {
                gpu_worker_task_count + 2 * gpu_threads
            } else {
                gpu_worker_task_count + gpu_threads
            }
        } else {
            0
        };

        let reader_threads_label = if cfg.hdd_reader_thread_count == 0 {
            format!("{} (one per drive)", scan.drives.len().max(1))
        } else {
            cfg.hdd_reader_thread_count.to_string()
        };

        #[cfg(feature = "opencl")]
        {
            info!(
                "reader-threads={}, CPU-threads={}, GPU-threads={}",
                reader_threads_label, cpu_threads, gpu_threads,
            );
            info!(
                "CPU-buffer={}(+{}), GPU-buffer={}(+{})",
                cpu_worker_task_count,
                if cpu_worker_task_count > 0 {
                    cpu_threads
                } else {
                    0
                },
                gpu_worker_task_count,
                if gpu_worker_task_count > 0 {
                    if cfg.gpu_async {
                        2 * gpu_threads
                    } else {
                        gpu_threads
                    }
                } else {
                    0
                }
            );
            if cpu_threads * cpu_worker_task_count + gpu_threads * gpu_worker_task_count == 0 {
                return Err(
                    "CPU, GPU: no active workers. Check cpu_worker_task_count / gpu_worker_task_count."
                        .to_string(),
                );
            }
        }

        #[cfg(not(feature = "opencl"))]
        {
            info!(
                "reader-threads={}, CPU-threads={}",
                reader_threads_label, cpu_threads
            );
            info!("CPU-buffer={}(+{})", cpu_worker_task_count, cpu_threads);
            if cpu_threads * cpu_worker_task_count == 0 {
                return Err(
                    "CPU: no active workers. cpu_worker_task_count must be greater than 0."
                        .to_string(),
                );
            }
        }

        #[cfg(not(feature = "opencl"))]
        let buffer_count = cpu_buffer_count;
        #[cfg(feature = "opencl")]
        let buffer_count = cpu_buffer_count + gpu_buffer_count;

        let buffer_size_cpu = cfg.io_buffer_size;
        info!(
            "I/O buffers: {} x {} ({} nonces each), direct I/O {}",
            cpu_buffer_count,
            human_size(buffer_size_cpu as u64),
            buffer_size_cpu / SCOOP_SIZE as usize,
            if cfg.hdd_use_direct_io {
                "requested"
            } else {
                "disabled"
            }
        );
        let (tx_empty_buffers, rx_empty_buffers) = crossbeam_channel::bounded(buffer_count);
        let (tx_read_replies_cpu, rx_read_replies_cpu) =
            crossbeam_channel::bounded(cpu_buffer_count.max(1));

        #[cfg(feature = "opencl")]
        let mut tx_read_replies_gpu = Vec::new();
        #[cfg(feature = "opencl")]
        let mut rx_read_replies_gpu = Vec::new();
        #[cfg(feature = "opencl")]
        let mut gpu_contexts = Vec::new();
        #[cfg(feature = "opencl")]
        {
            for _ in 0..gpu_threads {
                let (tx, rx) = crossbeam_channel::unbounded();
                tx_read_replies_gpu.push(tx);
                rx_read_replies_gpu.push(rx);
            }

            for _ in 0..gpu_threads {
                gpu_contexts.push(Arc::new(GpuContext::new(
                    cfg.gpu_platform,
                    cfg.gpu_device,
                    cfg.gpu_nonces_per_cache,
                    if cfg.benchmark_io() {
                        false
                    } else {
                        cfg.gpu_mem_mapping
                    },
                )));
            }
        }

        for _ in 0..cpu_buffer_count {
            let cpu_buffer = CpuBuffer::new(buffer_size_cpu);
            tx_empty_buffers
                .send(Box::new(cpu_buffer) as Box<dyn Buffer + Send>)
                .map_err(|e| format!("failed to seed buffer pool: {}", e))?;
        }

        #[cfg(feature = "opencl")]
        for (i, context) in gpu_contexts.iter().enumerate() {
            for _ in 0..(gpu_buffer_count / gpu_threads
                + if i == 0 {
                    gpu_buffer_count % gpu_threads
                } else {
                    0
                })
            {
                let gpu_buffer = GpuBuffer::new(&context.clone(), i + 1);
                tx_empty_buffers
                    .send(Box::new(gpu_buffer) as Box<dyn Buffer + Send>)
                    .map_err(|e| format!("failed to seed GPU buffer pool: {}", e))?;
            }
        }

        let (tx_nonce_data, rx_nonce_data) = mpsc::channel(buffer_count.max(1));

        thread::Builder::new()
            .name("cpu-worker-dispatch".into())
            .spawn(create_cpu_worker_task(
                cfg.benchmark_io(),
                new_thread_pool(cpu_threads, cfg.cpu_thread_pinning),
                rx_read_replies_cpu,
                tx_empty_buffers.clone(),
                tx_nonce_data.clone(),
            ))
            .map_err(|e| format!("failed to spawn CPU worker thread: {}", e))?;

        #[cfg(feature = "opencl")]
        for i in 0..gpu_threads {
            if cfg.gpu_async {
                thread::spawn(create_gpu_worker_task_async(
                    cfg.benchmark_io(),
                    rx_read_replies_gpu[i].clone(),
                    tx_empty_buffers.clone(),
                    tx_nonce_data.clone(),
                    gpu_contexts[i].clone(),
                    scan.drives.len(),
                ));
            } else {
                thread::spawn(create_gpu_worker_task(
                    cfg.benchmark_io(),
                    rx_read_replies_gpu[i].clone(),
                    tx_empty_buffers.clone(),
                    tx_nonce_data.clone(),
                    gpu_contexts[i].clone(),
                ));
            }
        }

        #[cfg(feature = "opencl")]
        let tx_read_replies_gpu = Some(tx_read_replies_gpu);
        #[cfg(not(feature = "opencl"))]
        let tx_read_replies_gpu = None;

        let metrics = new_shared_metrics();
        let disk_health = new_shared_disk_health();

        let round_bytes = scan.round_bytes;
        let reader = Reader::new(
            scan.drives,
            round_bytes,
            cfg.hdd_reader_thread_count,
            rx_empty_buffers,
            tx_empty_buffers,
            tx_read_replies_cpu,
            tx_read_replies_gpu,
            crate::logger::effective_show_progress(&cfg),
            cfg.show_drive_stats,
            cfg.cpu_thread_pinning,
            cfg.benchmark_cpu(),
            disk_health.clone(),
        );

        let benchmark_cpu = cfg.benchmark_cpu();
        let request_handler = RequestHandler::new(
            cfg.url,
            cfg.account_id_to_secret_phrase,
            cfg.timeout,
            capacity_gib(round_bytes),
            cfg.send_proxy_details,
            cfg.additional_headers,
            metrics.clone(),
            executor,
        )?;

        Ok(Miner {
            plot_dirs: cfg.plot_dirs.clone(),
            hdd_use_direct_io: cfg.hdd_use_direct_io,
            benchmark_cpu,
            capacity_check_interval: Duration::from_secs(cfg.capacity_check_interval.max(1)),
            reader: Arc::new(Mutex::new(reader)),
            rx_nonce_data: Some(rx_nonce_data),
            target_deadline: cfg.target_deadline,
            account_id_to_target_deadline: cfg.account_id_to_target_deadline,
            request_handler,
            state: Arc::new(Mutex::new(State::new())),
            // floor at 1s to protect servers
            get_mining_info_interval: max(1000, cfg.get_mining_info_interval),
            wakeup_after: if cfg.hdd_wakeup_after > 0 {
                Some(Duration::from_secs(cfg.hdd_wakeup_after as u64))
            } else {
                None
            },
            submit_only_best: cfg.submit_only_best,
            metrics,
            disk_health,
        })
    }

    /// Rescan plot directories and apply the result.  Returns the delay until
    /// the next scan should happen.
    pub async fn refresh_capacity(&self) -> Duration {
        let plot_dirs = self.plot_dirs.clone();
        let use_direct_io = self.hdd_use_direct_io;
        let dummy = self.benchmark_cpu;
        let scan =
            match tokio::task::spawn_blocking(move || scan_plots(&plot_dirs, use_direct_io, dummy))
                .await
            {
                Ok(scan) => scan,
                Err(e) => {
                    error!("capacity rescan task failed: {}", e);
                    return UNHEALTHY_RESCAN_INTERVAL;
                }
            };
        let next_delay = if scan.is_healthy() {
            self.capacity_check_interval
        } else {
            min(self.capacity_check_interval, UNHEALTHY_RESCAN_INTERVAL)
        };

        let old_size = lock_or_recover(&self.reader).total_size;
        let new_size = scan.round_bytes;

        // Decide first, with only the state lock held, then apply with only
        // the reader lock held.  Never hold both: the mining-info task takes
        // them in the opposite order.
        let apply = if should_apply_capacity_update(
            old_size,
            new_size,
            scan.had_errors,
            scan.had_empty_dir,
        ) {
            lock_or_recover(&self.state).pending_capacity_drop = None;
            true
        } else if scan.had_errors {
            warn!(
                "capacity rescan had filesystem errors and would reduce capacity ({:.4} -> {:.4} TiB). Keeping previous capacity.",
                tib(old_size),
                tib(new_size)
            );
            false
        } else {
            let mut state = lock_or_recover(&self.state);
            if state.pending_capacity_drop == Some(new_size) {
                warn!(
                    "capacity rescan still shows reduced capacity with empty plot directories ({:.4} -> {:.4} TiB). Applying update after confirmation.",
                    tib(old_size),
                    tib(new_size)
                );
                state.pending_capacity_drop = None;
                true
            } else {
                state.pending_capacity_drop = Some(new_size);
                warn!(
                    "capacity rescan saw a large drop with empty plot directories ({:.4} -> {:.4} TiB). Waiting for a second matching rescan before applying.",
                    tib(old_size),
                    tib(new_size)
                );
                false
            }
        };

        if !apply {
            return next_delay;
        }

        lock_or_recover(&self.reader).update_plots(scan.drives, new_size, self.benchmark_cpu);
        self.request_handler.update_capacity(capacity_gib(new_size));

        if old_size != new_size {
            info!("updated total capacity: {:.4} TiB", tib(new_size));
        }
        next_delay
    }

    pub async fn run(mut self) {
        use tokio::time::sleep;

        let rx_nonce_data = self
            .rx_nonce_data
            .take()
            .expect("nonce receiver is taken exactly once");
        let miner = Arc::new(self);

        // --- mining info polling -------------------------------------------------
        let miner_for_interval = miner.clone();
        tokio::spawn(async move {
            let get_mining_info_interval = miner_for_interval.get_mining_info_interval;
            Interval::new_interval(Duration::from_millis(get_mining_info_interval))
                .for_each(move |_| {
                    let miner = miner_for_interval.clone();
                    async move { miner.poll_mining_info().await }
                })
                .await;
        });

        // --- plot directory rescans ----------------------------------------------
        let miner_refresh = miner.clone();
        tokio::spawn(async move {
            let mut delay = miner_refresh.capacity_check_interval;
            {
                let reader = lock_or_recover(&miner_refresh.reader);
                if reader.drive_count() == 0 {
                    delay = min(delay, UNHEALTHY_RESCAN_INTERVAL);
                }
            }
            loop {
                sleep(delay).await;
                delay = miner_refresh.refresh_capacity().await;
            }
        });

        // --- periodic metrics report ---------------------------------------------
        let miner_metrics = miner.clone();
        tokio::spawn(async move {
            Interval::new_interval(Duration::from_secs(300))
                .for_each(move |_| {
                    let miner = miner_metrics.clone();
                    async move {
                        let summary = read_or_recover(&miner.metrics).summary();
                        info!("\n{}", summary);
                        let disk_health = read_or_recover(&miner.disk_health);
                        if disk_health.has_unhealthy_drives() {
                            warn!("\n{}", disk_health.health_summary());
                        } else {
                            info!("\n{}", disk_health.health_summary());
                        }
                    }
                })
                .await;
        });

        // --- nonce handling ------------------------------------------------------
        let miner_nonces = miner.clone();
        tokio::spawn(async move {
            let mut best_nonce_data = NonceData {
                height: 0,
                block: 0,
                base_target: 0,
                deadline: 0,
                nonce: 0,
                reader_task_processed: false,
                drive_chunks: 0,
                account_id: 0,
            };
            ReceiverStream::new(rx_nonce_data)
                .for_each(move |nonce_data| {
                    let miner = miner_nonces.clone();
                    let submissions = miner.handle_nonce(nonce_data, &mut best_nonce_data);
                    async move {
                        for s in submissions {
                            miner.request_handler.submit_nonce(s);
                        }
                    }
                })
                .await;
        });

        match tokio::signal::ctrl_c().await {
            Ok(()) => info!("shutdown requested, exiting"),
            Err(e) => {
                error!("failed to listen for shutdown signal: {}", e);
                loop {
                    sleep(Duration::from_secs(3600)).await;
                }
            }
        }
        process::exit(0);
    }

    async fn poll_mining_info(&self) {
        let result = self.request_handler.get_mining_info().await;
        match result {
            Ok(mining_info) => {
                let mut state = lock_or_recover(&self.state);
                state.first = false;
                if state.outage {
                    error!("{: <80}", "outage resolved.");
                    state.outage = false;
                }
                if mining_info.generation_signature != state.generation_signature {
                    if !state.update_mining_info(&mining_info) {
                        return;
                    }
                    let mut reader = lock_or_recover(&self.reader);
                    state.reader_task_count = reader.drive_count();
                    state.round_bytes = reader.total_size;
                    if state.reader_task_count == 0 {
                        warn!("no plots loaded, nothing to scan for this block");
                        state.scanning = false;
                        return;
                    }
                    reader.start_reading(
                        mining_info.height,
                        state.block,
                        mining_info.base_target,
                        state.scoop,
                        &Arc::new(state.generation_signature_bytes),
                    );
                } else if let Some(wakeup_after) = self.wakeup_after {
                    if !state.scanning && state.round_started.elapsed() > wakeup_after {
                        info!("HDD, wakeup!");
                        lock_or_recover(&self.reader).wakeup();
                        state.round_started = Instant::now();
                    }
                }
            }
            Err(e) => {
                write_or_recover(&self.metrics).record_network_error();
                let mut state = lock_or_recover(&self.state);
                if state.first {
                    error!(
                        "{: <80}",
                        format!(
                            "error getting mining info, please check server config: {}",
                            e
                        )
                    );
                    state.first = false;
                    state.outage = true;
                } else if !state.outage {
                    error!(
                        "{: <80}",
                        format!("error getting mining info => connection outage: {}", e)
                    );
                    state.outage = true;
                } else {
                    debug!("still no mining info: {}", e);
                }
            }
        }
    }

    /// Process one hashing result.  Returns the submissions to send, if any.
    fn handle_nonce(
        &self,
        nonce_data: NonceData,
        best_nonce_data: &mut NonceData,
    ) -> Vec<Submission> {
        let mut submissions = Vec::new();
        let mut state = lock_or_recover(&self.state);

        if state.height != nonce_data.height {
            // Result from a previous block; nothing to do with it.
            return submissions;
        }

        let deadline = nonce_data.deadline / nonce_data.base_target.max(1);
        let best_deadline = *state
            .account_id_to_best_deadline
            .get(&nonce_data.account_id)
            .unwrap_or(&u64::MAX);
        let limit = min(
            state.server_target_deadline,
            *self
                .account_id_to_target_deadline
                .get(&nonce_data.account_id)
                .unwrap_or(&self.target_deadline),
        );
        if best_deadline > deadline && deadline < limit {
            state
                .account_id_to_best_deadline
                .insert(nonce_data.account_id, deadline);

            if self.submit_only_best {
                *best_nonce_data = nonce_data;
            } else {
                submissions.push(Submission::from_nonce(
                    &nonce_data,
                    deadline,
                    state.generation_signature_bytes,
                ));
            }
        }

        state.chunks_seen += 1;
        if nonce_data.reader_task_processed {
            state.processed_reader_tasks += 1;
            state.chunks_expected += nonce_data.drive_chunks;
        }
        {
            if state.round_complete() {
                let round_time_ms = state.round_started.elapsed().as_millis().max(1) as u64;
                let speed_mibs =
                    state.round_bytes as f64 * 1000.0 / 1024.0 / 1024.0 / round_time_ms as f64;

                info!(
                    "{: <80}",
                    format!(
                        "round finished: roundtime={}ms, speed={:.2}MiB/s",
                        round_time_ms, speed_mibs
                    )
                );

                {
                    let mut metrics = write_or_recover(&self.metrics);
                    metrics.record_round_complete(round_time_ms as i64);
                    metrics.record_bytes_read(state.round_bytes);
                }

                // Submit our best one now, if configured that way.
                if self.submit_only_best && best_nonce_data.height == state.height {
                    let deadline = best_nonce_data.deadline / best_nonce_data.base_target.max(1);
                    submissions.push(Submission::from_nonce(
                        best_nonce_data,
                        deadline,
                        state.generation_signature_bytes,
                    ));
                }

                state.round_started = Instant::now();
                state.scanning = false;
            }
        }

        submissions
    }
}

/// A nonce selected for submission.
#[derive(Clone, Copy, Debug)]
pub struct Submission {
    pub account_id: u64,
    pub nonce: u64,
    pub height: u64,
    pub block: u64,
    pub deadline_unadjusted: u64,
    pub deadline: u64,
    pub gen_sig: [u8; 32],
}

impl Submission {
    fn from_nonce(nonce_data: &NonceData, deadline: u64, gen_sig: [u8; 32]) -> Self {
        Submission {
            account_id: nonce_data.account_id,
            nonce: nonce_data.nonce,
            height: nonce_data.height,
            block: nonce_data.block,
            deadline_unadjusted: nonce_data.deadline,
            deadline,
            gen_sig,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_drop_with_scan_errors_is_rejected() {
        assert!(!should_apply_capacity_update(100, 50, true, false));
    }

    #[test]
    fn capacity_drop_without_scan_errors_is_accepted() {
        assert!(should_apply_capacity_update(100, 50, false, false));
    }

    #[test]
    fn capacity_increase_with_scan_errors_is_accepted() {
        assert!(should_apply_capacity_update(100, 120, true, false));
    }

    #[test]
    fn large_drop_with_empty_plot_dir_is_rejected() {
        assert!(!should_apply_capacity_update(100, 70, false, true));
    }

    #[test]
    fn small_drop_with_empty_plot_dir_is_accepted() {
        assert!(should_apply_capacity_update(100, 90, false, true));
    }

    #[test]
    fn plot_filename_detection_works() {
        assert!(looks_like_plot_file(Path::new("123_0_1000")));
        assert!(!looks_like_plot_file(Path::new("notes.txt")));
        assert!(!looks_like_plot_file(Path::new("123_0_1000.tmp")));
    }

    #[test]
    fn human_size_picks_sensible_units() {
        assert_eq!(human_size(4096), "4 KiB");
        assert_eq!(human_size(16 * 1024 * 1024), "16 MiB");
        assert_eq!(human_size(100), "100 B");
    }

    #[test]
    fn capacity_header_is_in_gib() {
        // 4096 nonces = 1 GiB of plots.
        assert_eq!(capacity_gib(4096 * SCOOP_SIZE), 1);
        assert_eq!(capacity_gib(4096 * 1024 * SCOOP_SIZE), 1024);
    }

    #[test]
    fn scan_plots_loads_test_plot_and_flags_missing_dirs() {
        let test_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test_data");
        let missing = PathBuf::from("/definitely/not/here");
        let scan = scan_plots(std::slice::from_ref(&test_dir), false, false);
        assert_eq!(scan.plot_count, 1);
        assert_eq!(scan.drives.len(), 1);
        assert_eq!(scan.round_bytes, 8 * SCOOP_SIZE);
        assert!(scan.is_healthy());

        let scan = scan_plots(&[test_dir, missing], false, false);
        assert_eq!(scan.plot_count, 1);
        assert!(scan.had_errors);
        assert!(!scan.is_healthy());
    }
}
