use crate::com::api::MiningInfoResponse as MiningInfo;
use crate::config::Cfg;
use crate::cpu_worker::create_cpu_worker_task;
use crate::future::interval::Interval;
#[cfg(feature = "opencl")]
use crate::gpu_worker::create_gpu_worker_task;
#[cfg(feature = "opencl")]
use crate::gpu_worker_async::create_gpu_worker_task_async;
#[cfg(feature = "opencl")]
use crate::ocl::GpuBuffer;
#[cfg(feature = "opencl")]
use crate::ocl::GpuContext;
use crate::plot::{Plot, SCOOP_SIZE};
use crate::poc_hashing;
use crate::reader::Reader;
use crate::requests::RequestHandler;
use crate::utils::{get_bus_type, get_device_id, new_thread_pool};
use crossbeam_channel;
use filetime::FileTime;
use futures_util::{stream::StreamExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
#[cfg(feature = "opencl")]
use ocl_core::Mem;
use std::cmp::{max, min};
use std::collections::HashMap;
use std::fs::read_dir;
use std::path::PathBuf;
use std::process;
use std::sync::Arc;
#[cfg(feature = "async_io")]
use tokio::sync::Mutex;
#[cfg(not(feature = "async_io"))]
use std::sync::Mutex;
//use std::sync::Arc;
//use tokio::sync::Mutex;
use std::thread;
use std::u64;
use stopwatch::Stopwatch;
use tokio::runtime::Handle;



pub struct Miner {
    plot_dirs: Vec<PathBuf>,
    hdd_use_direct_io: bool,
    benchmark_cpu: bool,
    capacity_check_interval: u64,
    reader: Arc<Mutex<Reader>>,
    request_handler: Arc<Mutex<RequestHandler>>,
    rx_nonce_data: mpsc::Receiver<NonceData>,
    target_deadline: u64,
    account_id_to_target_deadline: HashMap<u64, u64>,
    state: Arc<Mutex<State>>,
    reader_task_count: usize,
    get_mining_info_interval: u64,
    executor: Handle,
    wakeup_after: i64,
    submit_only_best: bool,
}

pub struct State {
    generation_signature: String,
    generation_signature_bytes: [u8; 32],
    height: u64,
    block: u64,
    account_id_to_best_deadline: HashMap<u64, u64>,
    server_target_deadline: u64,
    base_target: u64,
    sw: Stopwatch,
    scanning: bool,
    processed_reader_tasks: usize,
    scoop: u32,
    first: bool,
    outage: bool,
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
            sw: Stopwatch::new(),
            generation_signature_bytes: [0; 32],
            scanning: false,
            first: true,
            outage: false,
        }
    }

    fn update_mining_info(&mut self, mining_info: &MiningInfo) {
        for best_deadlines in self.account_id_to_best_deadline.values_mut() {
            *best_deadlines = u64::MAX;
        }
        self.height = mining_info.height;
        self.block += 1;
        self.base_target = mining_info.base_target;
        self.server_target_deadline = mining_info.target_deadline;

        self.generation_signature_bytes =
            poc_hashing::decode_gensig(&mining_info.generation_signature);
        self.generation_signature = mining_info.generation_signature.clone();

        let scoop =
            poc_hashing::calculate_scoop(mining_info.height, &self.generation_signature_bytes);
        info!(
            "{: <80}",
            format!("new block: height={}, scoop={}", mining_info.height, scoop)
        );
        self.scoop = scoop;

        self.sw.restart();
        self.processed_reader_tasks = 0;
        self.scanning = true;
    }
}

#[derive(Copy, Clone)]
pub struct NonceData {
    pub height: u64,
    pub block: u64,
    pub base_target: u64,
    pub deadline: u64,
    pub nonce: u64,
    pub reader_task_processed: bool,
    pub account_id: u64,
}

pub trait Buffer {
    fn get_buffer(&mut self) -> Arc<Mutex<Vec<u8>>>;
    fn get_buffer_for_writing(&mut self) -> Arc<Mutex<Vec<u8>>>;
    #[cfg(feature = "opencl")]
    fn get_gpu_buffers(&self) -> Option<&GpuBuffer>;
    #[cfg(feature = "opencl")]
    fn get_gpu_data(&self) -> Option<Mem>;
    fn unmap(&self);
    fn get_id(&self) -> usize;
}

pub struct CpuBuffer {
    data: Arc<Mutex<Vec<u8>>>,
}

impl CpuBuffer {
    pub fn new(buffer_size: usize) -> Self {
        let data = vec![0u8; buffer_size];

        CpuBuffer {
            data: Arc::new(Mutex::new(data)),
        }
    }
}

impl Buffer for CpuBuffer {
    fn get_buffer(&mut self) -> Arc<Mutex<Vec<u8>>> {
        self.data.clone()
    }
    fn get_buffer_for_writing(&mut self) -> Arc<Mutex<Vec<u8>>> {
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

fn scan_plots(
    plot_dirs: &[PathBuf],
    use_direct_io: bool,
    dummy: bool,
) -> (HashMap<String, Arc<Vec<Mutex<Plot>>>>, u64) {
    let mut drive_id_to_plots: HashMap<String, Vec<Mutex<Plot>>> = HashMap::new();
    let mut global_capacity: u64 = 0;

    for plot_dir in plot_dirs {
        let bus_type = get_bus_type(plot_dir.to_str().unwrap());
        let is_usb = bus_type.to_lowercase() == "usb" || bus_type.to_lowercase() == "removable";
        let mut num_plots = 0;
        let mut local_capacity: u64 = 0;
        for file in read_dir(plot_dir).unwrap() {
            let file = &file.unwrap().path();

            if let Ok(p) = Plot::new(file, use_direct_io && !is_usb, dummy) {
                let drive_id = get_device_id(&file.to_str().unwrap().to_string());
                let plots = drive_id_to_plots.entry(drive_id).or_insert(Vec::new());

                local_capacity += p.meta.nonces as u64;
                plots.push(Mutex::new(p));
                num_plots += 1;
            }
        }

        info!(
            "path={}, files={}, size={:.4} TiB{}",
            plot_dir.to_str().unwrap(),
            num_plots,
            local_capacity as f64 / 4.0 / 1024.0 / 1024.0,
            if is_usb { " (USB)" } else { "" }
        );

        global_capacity += local_capacity;
        if num_plots == 0 {
            warn!("no plots in {}", plot_dir.to_str().unwrap());
        }
    }

    // sort plots by filetime and get them into an arc
    let drive_id_to_plots: HashMap<String, Arc<Vec<Mutex<Plot>>>> = drive_id_to_plots
        .drain()
        .map(|(drive_id, mut plots)| {
            plots.sort_by_key(|p| {
                #[cfg(feature = "async_io")]
                let p = p.blocking_lock();
                #[cfg(not(feature = "async_io"))]
                let p = p.lock().unwrap();
                let m = std::fs::metadata(&p.path).unwrap();
                -FileTime::from_last_modification_time(&m).unix_seconds()
            });
            (drive_id, Arc::new(plots))
        })
        .collect();

    info!(
        "plot files loaded: total drives={}, total capacity={:.4} TiB",
        drive_id_to_plots.len(),
        global_capacity as f64 / 4.0 / 1024.0 / 1024.0
    );

    (drive_id_to_plots, global_capacity * 64)
}

impl Miner {
    pub fn new(cfg: Cfg, executor: Handle) -> Miner {
        let (drive_id_to_plots, total_size) =
            scan_plots(&cfg.plot_dirs, cfg.hdd_use_direct_io, cfg.benchmark_cpu());

        let cpu_threads = cfg.cpu_threads.max(1);
        info!("🖥️  Using {} CPU thread(s)", cpu_threads);
        let cpu_worker_task_count = cfg.cpu_worker_task_count;

        let cpu_buffer_count = cpu_worker_task_count
            + if cpu_worker_task_count > 0 {
                cpu_threads
            } else {
                0
            };

        let reader_thread_count = if cfg.hdd_reader_thread_count == 0 {
            drive_id_to_plots.len()
        } else {
            cfg.hdd_reader_thread_count
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
        #[cfg(feature = "opencl")]
        {
            info!(
                "reader-threads={}, CPU-threads={}, GPU-threads={}",
                reader_thread_count, cpu_threads, gpu_threads,
            );
            info!("→ Starting now");
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

            {
                if cpu_threads * cpu_worker_task_count + gpu_threads * gpu_worker_task_count == 0 {
                    error!("CPU, GPU: no active workers. Check thread and task configuration. Shutting down...");
                    process::exit(0);
                }
            }
        }

        #[cfg(not(feature = "opencl"))]
        {
            info!(
                "reader-threads={} CPU-threads={}",
                reader_thread_count, cpu_threads
            );
            info!("CPU-buffer={}(+{})", cpu_worker_task_count, cpu_threads);
            {
                if cpu_threads * cpu_worker_task_count == 0 {
                    error!(
                    "CPU: no active workers. Check thread and task configuration. Shutting down..."
                );
                    process::exit(0);
                }
            }
        }

        #[cfg(not(feature = "opencl"))]
        let buffer_count = cpu_buffer_count;
        #[cfg(feature = "opencl")]
        let buffer_count = cpu_buffer_count + gpu_buffer_count;

        let cpu_nonces_per_cache = cfg.io_buffer_size / SCOOP_SIZE as usize;
        let buffer_size_cpu = cpu_nonces_per_cache * SCOOP_SIZE as usize;
        let (tx_empty_buffers, rx_empty_buffers) =
            crossbeam_channel::bounded(buffer_count as usize);
        let (tx_read_replies_cpu, rx_read_replies_cpu) =
            crossbeam_channel::bounded(cpu_buffer_count);

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
                .unwrap();
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
                    .unwrap();
            }
        }

        let (tx_nonce_data, rx_nonce_data) = mpsc::channel(buffer_count);

        thread::spawn({
            create_cpu_worker_task(
                cfg.benchmark_io(),
                new_thread_pool(cpu_threads, cfg.cpu_thread_pinning),
                rx_read_replies_cpu.clone(),
                tx_empty_buffers.clone(),
                tx_nonce_data.clone(),
            )
        });

        #[cfg(feature = "opencl")]
        for i in 0..gpu_threads {
            if cfg.gpu_async {
                thread::spawn({
                    create_gpu_worker_task_async(
                        cfg.benchmark_io(),
                        rx_read_replies_gpu[i].clone(),
                        tx_empty_buffers.clone(),
                        tx_nonce_data.clone(),
                        gpu_contexts[i].clone(),
                        drive_id_to_plots.len(),
                    )
                });
            } else {
                #[cfg(feature = "opencl")]
                thread::spawn({
                    create_gpu_worker_task(
                        cfg.benchmark_io(),
                        rx_read_replies_gpu[i].clone(),
                        tx_empty_buffers.clone(),
                        tx_nonce_data.clone(),
                        gpu_contexts[i].clone(),
                    )
                });
            }
        }

        #[cfg(feature = "opencl")]
        let tx_read_replies_gpu = Some(tx_read_replies_gpu);
        #[cfg(not(feature = "opencl"))]
        let tx_read_replies_gpu = None;

        Miner {
            plot_dirs: cfg.plot_dirs.clone(),
            hdd_use_direct_io: cfg.hdd_use_direct_io,
            benchmark_cpu: cfg.benchmark_cpu(),
            capacity_check_interval: cfg.capacity_check_interval,
            reader_task_count: drive_id_to_plots.len(),
            reader: Arc::new(Mutex::new(Reader::new(
                drive_id_to_plots,
                total_size,
                reader_thread_count,
                rx_empty_buffers,
                tx_empty_buffers,
                tx_read_replies_cpu,
                tx_read_replies_gpu,
                cfg.show_progress,
                cfg.show_drive_stats,
                cfg.cpu_thread_pinning,
                cfg.benchmark_cpu(),
            ))), // three closing parens
            rx_nonce_data,
            target_deadline: cfg.target_deadline,
            account_id_to_target_deadline: cfg.account_id_to_target_deadline,
            request_handler: Arc::new(Mutex::new(RequestHandler::new(
                cfg.url,
                cfg.account_id_to_secret_phrase,
                cfg.timeout,
                (total_size * 4 / 1024 / 1024) as usize,
                cfg.send_proxy_details,
                cfg.additional_headers,
                executor.clone(),
            ))), // three closing parens
            state: Arc::new(Mutex::new(State::new())),
            // floor at 1s to protect servers
            get_mining_info_interval: max(1000, cfg.get_mining_info_interval),
            executor,
            wakeup_after: cfg.hdd_wakeup_after * 1000, // ms -> s
            submit_only_best : cfg.submit_only_best,
        }
    }

    pub async fn refresh_capacity(&self) {
        let (drive_id_to_plots, total_size) =
            scan_plots(&self.plot_dirs, self.hdd_use_direct_io, self.benchmark_cpu);

        #[cfg(feature = "async_io")]
        let mut reader = self.reader.lock().await;
        #[cfg(not(feature = "async_io"))]
        let mut reader = self.reader.lock().unwrap();
        let old_size = reader.total_size;
        reader.update_plots(drive_id_to_plots, total_size, self.benchmark_cpu);
        drop(reader);

        let total_size_gb = (total_size * 4 / 1024 / 1024) as usize;
        #[cfg(feature = "async_io")]
        {
            let mut rh = self.request_handler.lock().await;
            rh.update_capacity(total_size_gb).await;
        }
        #[cfg(not(feature = "async_io"))]
        {
            let mut rh = self.request_handler.lock().unwrap();
            rh.update_capacity(total_size_gb);
        }

        if old_size != total_size {
            info!(
                "updated total capacity: {:.4} TiB",
                (total_size / 64) as f64 / 4.0 / 1024.0 / 1024.0
            );
        }
    }

    pub async fn run(self) {
        use tokio::time::{sleep, Duration};
        let mut miner = Arc::new(self);

        // Take ownership of the nonce receiver before cloning the miner
        let rx_nonce_data = {
            // use a dummy channel to leave a valid receiver inside the miner
            let (_tx, dummy_rx) = mpsc::channel(1);
            let inner = Arc::get_mut(&mut miner).expect("unique reference");
            std::mem::replace(&mut inner.rx_nonce_data, dummy_rx)
        };

        let request_handler = miner.request_handler.clone();
        #[cfg(feature = "async_io")]
        let total_size = { miner.reader.lock().await.total_size };
        #[cfg(not(feature = "async_io"))]
        let total_size = { miner.reader.lock().unwrap().total_size };

        let reader = miner.reader.clone();


        let state = miner.state.clone();
        // there might be a way to solve this without two nested moves
        let get_mining_info_interval = miner.get_mining_info_interval;
        let wakeup_after = miner.wakeup_after;
        tokio::spawn(async move {
            info!("→ Interval task started");
            Interval::new_interval(Duration::from_millis(get_mining_info_interval))
                .for_each(move |_| {
                    let state = state.clone();
                    let reader = reader.clone();
                    let request_handler = request_handler.clone();
                    async move {
                        #[cfg(feature = "async_io")]
                        let mining_info_fut = {
                            let rh = request_handler.lock().await.clone();
                            async move { rh.get_mining_info().await }
                        };
                        #[cfg(not(feature = "async_io"))]
                        let mining_info_fut = {
                            let rh = request_handler.lock().unwrap().clone();
                            async move { rh.get_mining_info().await }
                        };
                        match mining_info_fut.await {
                            Ok(mining_info) => {
                                #[cfg(feature = "async_io")]
                                let mut state = state.lock().await;
                                #[cfg(not(feature = "async_io"))]
                                let mut state = state.lock().unwrap();
                                state.first = false;
                                if state.outage {
                                    error!("{: <80}", "outage resolved.");
                                    state.outage = false;
                                }
                                if mining_info.generation_signature != state.generation_signature {
                                    state.update_mining_info(&mining_info);
                                    #[cfg(feature = "async_io")]
                                    reader.lock().await.start_reading(
                                        mining_info.height,
                                        state.block,
                                        mining_info.base_target,
                                        state.scoop,
                                        &Arc::new(state.generation_signature_bytes),
                                    );
                                    #[cfg(not(feature = "async_io"))]
                                    reader.lock().unwrap().start_reading(
                                        mining_info.height,
                                        state.block,
                                        mining_info.base_target,
                                        state.scoop,
                                        &Arc::new(state.generation_signature_bytes),
                                    );
                                    drop(state);
                                } else if !state.scanning
                                    && wakeup_after != 0
                                    && state.sw.elapsed_ms() > wakeup_after
                                {
                                    info!("HDD, wakeup!");
                                    #[cfg(feature = "async_io")]
                                    reader.lock().await.wakeup();
                                    #[cfg(not(feature = "async_io"))]
                                    reader.lock().unwrap().wakeup();
                                    state.sw.restart();
                                }
                            }
                            _ => {
                                #[cfg(feature = "async_io")]
                                let mut state = state.lock().await;
                                #[cfg(not(feature = "async_io"))]
                                let mut state = state.lock().unwrap();
                                if state.first {
                                    error!(
                                        "{: <80}",
                                        "error getting mining info, please check server config"
                                    );
                                    state.first = false;
                                    state.outage = true;
                                } else if !state.outage {
                                    error!(
                                        "{: <80}",
                                        "error getting mining info => connection outage..."
                                    );
                                    state.outage = true;
                                }
                            }
                        }
                    }
                })
                .await;
        });

        let miner_refresh = miner.clone();
        tokio::spawn(async move {
            Interval::new_interval(Duration::from_secs(miner_refresh.capacity_check_interval))
                .for_each(move |_| {
                    let miner_refresh = miner_refresh.clone();
                    async move {
                        miner_refresh.refresh_capacity().await;
                    }
                })
                .await;
        });

        // only start submitting nonces after a while
        let mut best_nonce_data = NonceData {
            height: 0,
            block: 0,
            base_target: 0,
            deadline: 0,
            nonce: 0,
            reader_task_processed: false,
            account_id: 0,
        };

        let target_deadline = miner.target_deadline;
        let account_id_to_target_deadline = miner.account_id_to_target_deadline.clone();
        let request_handler = miner.request_handler.clone();
        let state = miner.state.clone();
        let reader_task_count = miner.reader_task_count;
        let inner_submit_only_best = miner.submit_only_best;
        miner.executor.clone().spawn(
            ReceiverStream::new(rx_nonce_data)
                .for_each(move |nonce_data| {
                    let state = state.clone();
                    let request_handler = request_handler.clone();
                    let account_id_to_target_deadline = account_id_to_target_deadline.clone();
                    async move {
                        #[cfg(feature = "async_io")]
                        let mut state = state.lock().await;
                        #[cfg(not(feature = "async_io"))]
                        let mut state = state.lock().unwrap();

                        let deadline = nonce_data.deadline / nonce_data.base_target;
                        if state.height == nonce_data.height {
                            let best_deadline = *state
                                .account_id_to_best_deadline
                                .get(&nonce_data.account_id)
                                .unwrap_or(&u64::MAX);
                            if best_deadline > deadline
                                && deadline
                                    < min(
                                        state.server_target_deadline,
                                        *(account_id_to_target_deadline
                                            .get(&nonce_data.account_id)
                                            .unwrap_or(&target_deadline)),
                                    )
                            {
                                state
                                    .account_id_to_best_deadline
                                    .insert(nonce_data.account_id, deadline);

                                if inner_submit_only_best {
                                    best_nonce_data = nonce_data.clone();
                                } else {
                                    #[cfg(feature = "async_io")]
                                    request_handler.lock().await.submit_nonce(
                                        nonce_data.account_id,
                                        nonce_data.nonce,
                                        nonce_data.height,
                                        nonce_data.block,
                                        nonce_data.deadline,
                                        deadline,
                                        state.generation_signature_bytes,
                                    );
                                    #[cfg(not(feature = "async_io"))]
                                    request_handler.lock().unwrap().submit_nonce(
                                        nonce_data.account_id,
                                        nonce_data.nonce,
                                        nonce_data.height,
                                        nonce_data.block,
                                        nonce_data.deadline,
                                        deadline,
                                        state.generation_signature_bytes,
                                    );
                                }
                            }

                            if nonce_data.reader_task_processed {
                                state.processed_reader_tasks += 1;
                                if state.processed_reader_tasks == reader_task_count {
                                    info!(
                                        "{: <80}",
                                        format!(
                                            "round finished: roundtime={}ms, speed={:.2}MiB/s",
                                            state.sw.elapsed_ms(),
                                            total_size as f64 * 1000.0
                                                / 1024.0
                                                / 1024.0
                                                / state.sw.elapsed_ms() as f64
                                        )
                                    );

                                    // Submit now our best one, if configured that way
                                    if best_nonce_data.height == state.height {
                                        let deadline =
                                            best_nonce_data.deadline / best_nonce_data.base_target;
                                        #[cfg(feature = "async_io")]
                                        request_handler.lock().await.submit_nonce(
                                            best_nonce_data.account_id,
                                            best_nonce_data.nonce,
                                            best_nonce_data.height,
                                            best_nonce_data.block,
                                            best_nonce_data.deadline,
                                            deadline,
                                            state.generation_signature_bytes,
                                        );
                                        #[cfg(not(feature = "async_io"))]
                                        request_handler.lock().unwrap().submit_nonce(
                                            best_nonce_data.account_id,
                                            best_nonce_data.nonce,
                                            best_nonce_data.height,
                                            best_nonce_data.block,
                                            best_nonce_data.deadline,
                                            deadline,
                                            state.generation_signature_bytes,
                                        );
                                    }

                                    state.sw.restart();
                                    state.scanning = false;
                                }
                            }
                        }
                    }
                }),
        );
        loop {
        sleep(Duration::from_secs(60)).await;
        }
    }

}
