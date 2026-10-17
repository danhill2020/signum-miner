use crate::plot::{DIRECT_IO_ALIGNMENT, SCOOP_SIZE};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Serialize)]
pub enum Benchmark {
    IO,
    XPU,
    Disabled,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Cfg {
    #[serde(default = "default_secret_phrase")]
    pub account_id_to_secret_phrase: HashMap<u64, String>,

    #[serde(default)]
    pub plot_dirs: Vec<PathBuf>,

    #[serde(deserialize_with = "deserialize_url")]
    pub url: ::url::Url,

    #[serde(default = "default_hdd_reader_thread_count")]
    pub hdd_reader_thread_count: usize,

    #[serde(default = "default_hdd_use_direct_io")]
    pub hdd_use_direct_io: bool,

    #[serde(default = "default_hdd_wakeup_after")]
    pub hdd_wakeup_after: i64,

    #[serde(default = "default_cpu_threads")]
    pub cpu_threads: usize,

    #[serde(default = "default_cpu_worker_task_count")]
    pub cpu_worker_task_count: usize,

    /// Kept for compatibility with older configs; `io_buffer_size` decides the
    /// chunk size now.
    #[serde(default = "default_cpu_nonces_per_cache")]
    pub cpu_nonces_per_cache: usize,

    #[serde(default = "default_io_buffer_size")]
    pub io_buffer_size: usize,

    #[serde(default = "default_cpu_thread_pinning")]
    pub cpu_thread_pinning: bool,

    #[serde(default = "default_gpu_threads")]
    pub gpu_threads: usize,

    #[serde(default = "default_gpu_platform")]
    pub gpu_platform: usize,

    #[serde(default = "default_gpu_device")]
    pub gpu_device: usize,

    #[serde(default = "default_gpu_worker_task_count")]
    pub gpu_worker_task_count: usize,

    #[serde(default = "default_gpu_nonces_per_cache")]
    pub gpu_nonces_per_cache: usize,

    #[serde(default = "default_gpu_mem_mapping")]
    pub gpu_mem_mapping: bool,

    #[serde(default = "default_gpu_async")]
    pub gpu_async: bool,

    #[serde(default = "default_target_deadline")]
    pub target_deadline: u64,

    #[serde(default = "default_account_id_to_target_deadline")]
    pub account_id_to_target_deadline: HashMap<u64, u64>,

    #[serde(default = "default_get_mining_info_interval")]
    pub get_mining_info_interval: u64,

    #[serde(default = "default_timeout")]
    pub timeout: u64,

    #[serde(default = "default_send_proxy_details")]
    pub send_proxy_details: bool,

    #[serde(default = "default_additional_headers")]
    pub additional_headers: HashMap<String, String>,

    #[serde(default = "default_capacity_check_interval")]
    pub capacity_check_interval: u64,

    /// Seconds a plot that vanished (unplugged bay, USB reset) stays in the
    /// reported capacity and keeps being retried before it is dropped.
    #[serde(default = "default_plot_missing_grace")]
    pub plot_missing_grace: u64,

    #[serde(default = "default_console_log_level")]
    pub console_log_level: String,

    #[serde(default = "default_logfile_log_level")]
    pub logfile_log_level: String,

    #[serde(default = "default_logfile_max_count")]
    pub logfile_max_count: u32,

    #[serde(default = "default_logfile_max_size")]
    pub logfile_max_size: u64,

    #[serde(default = "default_console_log_pattern")]
    pub console_log_pattern: String,

    #[serde(default = "default_logfile_log_pattern")]
    pub logfile_log_pattern: String,

    #[serde(default = "default_show_progress")]
    pub show_progress: bool,

    #[serde(default = "default_show_drive_stats")]
    pub show_drive_stats: bool,

    #[serde(default = "default_submit_only_best")]
    pub submit_only_best: bool,

    pub benchmark_only: Option<Benchmark>,
}

/// Every key the miner understands.  Anything else in the YAML is almost
/// certainly a typo and gets a warning instead of being silently ignored.
pub const KNOWN_KEYS: &[&str] = &[
    "account_id_to_secret_phrase",
    "plot_dirs",
    "url",
    "hdd_reader_thread_count",
    "hdd_use_direct_io",
    "hdd_wakeup_after",
    "cpu_threads",
    "cpu_worker_task_count",
    "cpu_nonces_per_cache",
    "io_buffer_size",
    "cpu_thread_pinning",
    "gpu_threads",
    "gpu_platform",
    "gpu_device",
    "gpu_worker_task_count",
    "gpu_nonces_per_cache",
    "gpu_mem_mapping",
    "gpu_async",
    "target_deadline",
    "account_id_to_target_deadline",
    "get_mining_info_interval",
    "timeout",
    "send_proxy_details",
    "additional_headers",
    "capacity_check_interval",
    "plot_missing_grace",
    "console_log_level",
    "logfile_log_level",
    "logfile_max_count",
    "logfile_max_size",
    "console_log_pattern",
    "logfile_log_pattern",
    "show_progress",
    "show_drive_stats",
    "submit_only_best",
    "benchmark_only",
];

impl<'de> Deserialize<'de> for Benchmark {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str().to_lowercase().as_ref() {
            "i/o" => Benchmark::IO,
            "xpu" => Benchmark::XPU,
            _ => Benchmark::Disabled,
        })
    }
}

fn deserialize_url<'de, D>(deserializer: D) -> Result<::url::Url, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    let trimmed = raw.trim();
    ::url::Url::parse(trimmed)
        .map_err(|e| serde::de::Error::custom(format!("invalid url '{}': {}", trimmed, e)))
}

fn default_secret_phrase() -> HashMap<u64, String> {
    HashMap::new()
}

fn default_hdd_reader_thread_count() -> usize {
    0
}

fn default_hdd_use_direct_io() -> bool {
    true
}

fn default_hdd_wakeup_after() -> i64 {
    240
}

fn default_cpu_threads() -> usize {
    0
}

fn default_cpu_worker_task_count() -> usize {
    4
}

fn default_cpu_nonces_per_cache() -> usize {
    65536
}

fn default_io_buffer_size() -> usize {
    16 * 1024 * 1024
}

fn default_cpu_thread_pinning() -> bool {
    false
}

fn default_gpu_threads() -> usize {
    0
}

fn default_gpu_platform() -> usize {
    0
}

fn default_gpu_device() -> usize {
    0
}

fn default_gpu_worker_task_count() -> usize {
    0
}

fn default_gpu_nonces_per_cache() -> usize {
    1_048_576
}

fn default_gpu_mem_mapping() -> bool {
    false
}

fn default_gpu_async() -> bool {
    false
}

fn default_target_deadline() -> u64 {
    u64::from(u32::MAX)
}

fn default_account_id_to_target_deadline() -> HashMap<u64, u64> {
    HashMap::new()
}

fn default_get_mining_info_interval() -> u64 {
    3000
}

fn default_timeout() -> u64 {
    5000
}

fn default_send_proxy_details() -> bool {
    false
}

fn default_capacity_check_interval() -> u64 {
    21600
}

fn default_plot_missing_grace() -> u64 {
    3600
}

fn default_additional_headers() -> HashMap<String, String> {
    HashMap::new()
}

fn default_console_log_level() -> String {
    "Info".to_owned()
}

fn default_logfile_log_level() -> String {
    "Warn".to_owned()
}

fn default_logfile_max_count() -> u32 {
    10
}

fn default_logfile_max_size() -> u64 {
    20
}

fn default_console_log_pattern() -> String {
    "{d(%H:%M:%S.%3f%z)} [{h({l}):<5}] [{T}] [{t}] - {M}:{m}{n}".to_owned()
}

fn default_logfile_log_pattern() -> String {
    "{d(%Y-%m-%dT%H:%M:%S.%3f%z)} [{h({l}):<5}] [{T}] [{f}:{L}] [{t}] - {M}:{m}{n}".to_owned()
}

fn default_show_progress() -> bool {
    true
}

fn default_show_drive_stats() -> bool {
    false
}

fn default_submit_only_best() -> bool {
    true
}

/// Top-level keys present in the YAML that the miner does not know about.
pub fn unknown_keys(cfg_str: &str) -> Vec<String> {
    let value: serde_yaml::Value = match serde_yaml::from_str(cfg_str) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let Some(map) = value.as_mapping() else {
        return Vec::new();
    };
    map.iter()
        .filter_map(|(k, _)| k.as_str().map(str::to_owned))
        .filter(|k| !KNOWN_KEYS.contains(&k.as_str()))
        .collect()
}

pub fn load_cfg(config: &str) -> Result<Cfg, String> {
    let cfg_str = fs::read_to_string(config).map_err(|e| {
        format!(
            "Failed to open config file '{}': {}. Please check that the file exists and is readable.",
            config, e
        )
    })?;
    parse_cfg(&cfg_str, config)
}

pub fn parse_cfg(cfg_str: &str, origin: &str) -> Result<Cfg, String> {
    let cfg: Cfg = serde_yaml::from_str(cfg_str).map_err(|e| {
        format!(
            "Failed to parse config file '{}': {}. Please check YAML syntax.",
            origin, e
        )
    })?;

    for key in unknown_keys(cfg_str) {
        warn!(
            "config '{}': unknown setting '{}' is ignored (typo?)",
            origin, key
        );
    }

    validate_cfg(cfg)
}

/// Check the loaded values for mistakes a user would otherwise only discover
/// from odd behaviour hours later, and fill in the `0 = auto` values.
pub fn validate_cfg(mut cfg: Cfg) -> Result<Cfg, String> {
    if cfg.url.host_str().is_none() {
        return Err(format!(
            "Configuration error: url '{}' has no host. Expected something like 'https://pool.example.com:8080'",
            cfg.url
        ));
    }

    let cores = num_cpus::get();
    if cfg.cpu_threads == 0 {
        cfg.cpu_threads = cores;
    } else if cfg.cpu_threads > cores {
        warn!(
            "cpu_threads exceeds number of cores ({}), using ({}) threads",
            cores, cores
        );
        cfg.cpu_threads = cores;
    };

    if cfg.io_buffer_size == 0 || cfg.io_buffer_size % SCOOP_SIZE as usize != 0 {
        return Err(format!(
            "Configuration error: io_buffer_size ({}) must be a positive multiple of {} bytes",
            cfg.io_buffer_size, SCOOP_SIZE
        ));
    }
    if cfg.hdd_use_direct_io && cfg.io_buffer_size % DIRECT_IO_ALIGNMENT as usize != 0 {
        return Err(format!(
            "Configuration error: io_buffer_size ({}) must be a multiple of {} bytes when hdd_use_direct_io is enabled",
            cfg.io_buffer_size, DIRECT_IO_ALIGNMENT
        ));
    }
    if cfg.hdd_use_direct_io && cfg.gpu_nonces_per_cache % 64 != 0 {
        return Err(format!(
            "Configuration error: gpu_nonces_per_cache ({}) must be divisible by 64 when hdd_use_direct_io is enabled",
            cfg.gpu_nonces_per_cache
        ));
    }
    if cfg.cpu_worker_task_count == 0 && cfg.gpu_worker_task_count == 0 {
        return Err(
            "Configuration error: cpu_worker_task_count and gpu_worker_task_count are both 0, nothing would hash the plots"
                .to_string(),
        );
    }
    if cfg.get_mining_info_interval < 1000 {
        warn!(
            "get_mining_info_interval {}ms is below the 1000ms floor, using 1000ms",
            cfg.get_mining_info_interval
        );
        cfg.get_mining_info_interval = 1000;
    }
    if cfg.timeout < 100 {
        warn!("timeout {}ms is unreasonably low, using 100ms", cfg.timeout);
        cfg.timeout = 100;
    }
    if cfg.logfile_max_count == 0 {
        cfg.logfile_max_count = 1;
    }
    if cfg.logfile_max_size == 0 {
        cfg.logfile_max_size = 1;
    }
    if cfg.hdd_wakeup_after < 0 {
        warn!("hdd_wakeup_after is negative, disabling wakeup");
        cfg.hdd_wakeup_after = 0;
    }

    let filtered_dirs: Vec<PathBuf> = cfg
        .plot_dirs
        .iter()
        .filter(|plot_dir| {
            if !plot_dir.exists() {
                warn!("path {} does not exist", plot_dir.display());
                false
            } else if !plot_dir.is_dir() {
                warn!("path {} is not a directory", plot_dir.display());
                false
            } else {
                true
            }
        })
        .cloned()
        .collect();
    if cfg.plot_dirs.is_empty() {
        warn!("no plot_dirs configured");
    } else if filtered_dirs.is_empty() {
        warn!("none of the configured plot_dirs exist right now; the miner will keep rescanning");
    }
    cfg.plot_dirs = filtered_dirs;

    Ok(cfg)
}

impl Cfg {
    pub fn benchmark_cpu(&self) -> bool {
        matches!(self.benchmark_only, Some(Benchmark::XPU))
    }

    pub fn benchmark_io(&self) -> bool {
        matches!(self.benchmark_only, Some(Benchmark::IO))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal(extra: &str) -> String {
        format!("plot_dirs: []\nurl: 'http://localhost:8125'\n{}", extra)
    }

    #[test]
    fn test_load_cfg() {
        let config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test_data")
            .join("test_config.yaml");
        let cfg = load_cfg(config_path.to_str().unwrap()).expect("Failed to load config in test");

        assert_eq!(cfg.timeout, 5000);
        let expected_path = PathBuf::from("test_data");
        assert_eq!(cfg.plot_dirs, vec![expected_path]);
    }

    #[test]
    fn test_load_cfg_trims_url_whitespace() {
        let cfg = parse_cfg(
            "plot_dirs: []\nurl: \"  https://pool.signumcoin.ro  \"\n",
            "inline",
        )
        .expect("Failed to load config with whitespace URL");
        assert_eq!(cfg.url.as_str(), "https://pool.signumcoin.ro/");
    }

    #[test]
    fn minimal_config_gets_working_defaults() {
        let cfg = parse_cfg(&minimal(""), "inline").unwrap();
        assert!(cfg.cpu_threads >= 1);
        assert_eq!(cfg.cpu_worker_task_count, 4);
        assert_eq!(cfg.io_buffer_size, 16 * 1024 * 1024);
        assert!(cfg.hdd_use_direct_io);
        assert!(cfg.submit_only_best);
    }

    #[test]
    fn rejects_zero_workers() {
        let err = parse_cfg(&minimal("cpu_worker_task_count: 0\n"), "inline").unwrap_err();
        assert!(err.contains("cpu_worker_task_count"), "{}", err);
    }

    #[test]
    fn rejects_unaligned_buffer_with_direct_io() {
        let err = parse_cfg(&minimal("io_buffer_size: 100000\n"), "inline").unwrap_err();
        assert!(err.contains("io_buffer_size"), "{}", err);
        // Multiples of 64 are fine once direct I/O is off.
        let cfg = parse_cfg(
            &minimal("io_buffer_size: 6400\nhdd_use_direct_io: false\n"),
            "inline",
        )
        .unwrap();
        assert_eq!(cfg.io_buffer_size, 6400);
        let err = parse_cfg(
            &minimal("io_buffer_size: 100\nhdd_use_direct_io: false\n"),
            "inline",
        )
        .unwrap_err();
        assert!(err.contains("multiple of 64"), "{}", err);
    }

    #[test]
    fn rejects_url_without_host() {
        let err = parse_cfg("plot_dirs: []\nurl: 'mailto:someone'\n", "inline").unwrap_err();
        assert!(err.contains("no host"), "{}", err);
    }

    #[test]
    fn floors_polling_interval_and_timeout() {
        let cfg = parse_cfg(
            &minimal("get_mining_info_interval: 10\ntimeout: 1\n"),
            "inline",
        )
        .unwrap();
        assert_eq!(cfg.get_mining_info_interval, 1000);
        assert_eq!(cfg.timeout, 100);
    }

    #[test]
    fn unknown_keys_are_reported() {
        let keys = unknown_keys(&minimal("hdd_use_direct_i0: true\ncpu_treads: 2\n"));
        assert_eq!(
            keys,
            vec!["hdd_use_direct_i0".to_string(), "cpu_treads".to_string()]
        );
        assert!(unknown_keys(&minimal("cpu_threads: 2\n")).is_empty());
    }

    #[test]
    fn sample_config_only_uses_known_keys() {
        let sample =
            std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config.yaml"))
                .unwrap();
        assert!(unknown_keys(&sample).is_empty());
        parse_cfg(&sample, "config.yaml").expect("sample config must load");
    }
}
