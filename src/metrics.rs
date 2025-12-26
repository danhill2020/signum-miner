use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
#[cfg(feature = "async_io")]
use tokio::sync::RwLock;
#[cfg(not(feature = "async_io"))]
use std::sync::RwLock;

/// Comprehensive metrics tracking for the miner
/// Some fields and methods are intentionally kept for future monitoring/debugging use
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct MinerMetrics {
    /// Time when miner started
    start_time: Instant,
    /// Total number of nonce submissions attempted
    pub total_submissions: u64,
    /// Total number of successful submissions
    pub successful_submissions: u64,
    /// Total number of failed submissions
    pub failed_submissions: u64,
    /// Best deadline ever achieved (per account)
    pub best_deadlines: HashMap<u64, u64>,
    /// Total rounds completed
    pub rounds_completed: u64,
    /// Total rounds failed
    pub rounds_failed: u64,
    /// I/O errors per drive
    pub io_errors_by_drive: HashMap<String, u64>,
    /// Total I/O errors
    pub total_io_errors: u64,
    /// Config loading errors
    pub config_errors: u64,
    /// Network errors
    pub network_errors: u64,
    /// Last submission time
    pub last_submission: Option<Instant>,
    /// Average round time in milliseconds
    pub avg_round_time_ms: f64,
    /// Total bytes read
    pub total_bytes_read: u64,
}

#[allow(dead_code)]
impl MinerMetrics {
    pub fn new() -> Self {
        Self {
            start_time: Instant::now(),
            total_submissions: 0,
            successful_submissions: 0,
            failed_submissions: 0,
            best_deadlines: HashMap::new(),
            rounds_completed: 0,
            rounds_failed: 0,
            io_errors_by_drive: HashMap::new(),
            total_io_errors: 0,
            config_errors: 0,
            network_errors: 0,
            last_submission: None,
            avg_round_time_ms: 0.0,
            total_bytes_read: 0,
        }
    }

    /// Get uptime in seconds
    pub fn uptime_secs(&self) -> u64 {
        self.start_time.elapsed().as_secs()
    }

    /// Get uptime as formatted string
    pub fn uptime_formatted(&self) -> String {
        let secs = self.uptime_secs();
        let hours = secs / 3600;
        let minutes = (secs % 3600) / 60;
        let seconds = secs % 60;
        format!("{}h {}m {}s", hours, minutes, seconds)
    }

    /// Record a successful submission
    pub fn record_submission_success(&mut self, account_id: u64, deadline: u64) {
        self.total_submissions += 1;
        self.successful_submissions += 1;
        self.last_submission = Some(Instant::now());

        // Update best deadline for account
        let current_best = self.best_deadlines.entry(account_id).or_insert(u64::MAX);
        if deadline < *current_best {
            *current_best = deadline;
        }
    }

    /// Record a failed submission
    pub fn record_submission_failure(&mut self) {
        self.total_submissions += 1;
        self.failed_submissions += 1;
    }

    /// Record a completed round
    pub fn record_round_complete(&mut self, duration_ms: i64) {
        self.rounds_completed += 1;

        // Update average round time using exponential moving average
        let alpha = 0.1; // Smoothing factor
        self.avg_round_time_ms = alpha * duration_ms as f64 + (1.0 - alpha) * self.avg_round_time_ms;
    }

    /// Record a failed round
    pub fn record_round_failure(&mut self) {
        self.rounds_failed += 1;
    }

    /// Record an I/O error for a specific drive
    pub fn record_io_error(&mut self, drive_id: &str) {
        self.total_io_errors += 1;
        *self.io_errors_by_drive.entry(drive_id.to_string()).or_insert(0) += 1;
    }

    /// Record a network error
    pub fn record_network_error(&mut self) {
        self.network_errors += 1;
    }

    /// Record a config error
    pub fn record_config_error(&mut self) {
        self.config_errors += 1;
    }

    /// Record bytes read
    pub fn record_bytes_read(&mut self, bytes: u64) {
        self.total_bytes_read += bytes;
    }

    /// Get submission success rate
    pub fn submission_success_rate(&self) -> f64 {
        if self.total_submissions == 0 {
            0.0
        } else {
            (self.successful_submissions as f64 / self.total_submissions as f64) * 100.0
        }
    }

    /// Get round success rate
    pub fn round_success_rate(&self) -> f64 {
        let total_rounds = self.rounds_completed + self.rounds_failed;
        if total_rounds == 0 {
            0.0
        } else {
            (self.rounds_completed as f64 / total_rounds as f64) * 100.0
        }
    }

    /// Get average read speed in MiB/s
    pub fn avg_read_speed_mibs(&self) -> f64 {
        let uptime_ms = self.start_time.elapsed().as_millis() as f64;
        if uptime_ms == 0.0 {
            0.0
        } else {
            (self.total_bytes_read as f64 / 1024.0 / 1024.0) * 1000.0 / uptime_ms
        }
    }

    /// Get formatted metrics summary
    pub fn summary(&self) -> String {
        let mut summary = String::new();
        summary.push_str("=== MINER METRICS ===\n");
        summary.push_str(&format!("Uptime: {}\n", self.uptime_formatted()));
        summary.push_str(&format!("Rounds: {} completed, {} failed ({:.1}% success)\n",
            self.rounds_completed, self.rounds_failed, self.round_success_rate()));
        summary.push_str(&format!("Avg Round Time: {:.0}ms\n", self.avg_round_time_ms));
        summary.push_str(&format!("Submissions: {} total, {} successful, {} failed ({:.1}% success)\n",
            self.total_submissions, self.successful_submissions, self.failed_submissions,
            self.submission_success_rate()));
        summary.push_str(&format!("Data Read: {:.2} TiB (avg {:.2} MiB/s)\n",
            self.total_bytes_read as f64 / 1024.0 / 1024.0 / 1024.0 / 1024.0,
            self.avg_read_speed_mibs()));
        summary.push_str(&format!("I/O Errors: {} total\n", self.total_io_errors));
        summary.push_str(&format!("Network Errors: {}\n", self.network_errors));

        if !self.best_deadlines.is_empty() {
            summary.push_str("Best Deadlines:\n");
            for (account_id, deadline) in &self.best_deadlines {
                summary.push_str(&format!("  Account {}: {} seconds\n", account_id, deadline));
            }
        }

        summary
    }

    /// Get health status
    pub fn health_status(&self) -> HealthStatus {
        let io_error_rate = if self.total_bytes_read == 0 {
            0.0
        } else {
            (self.total_io_errors as f64 / (self.total_bytes_read / 1024 / 1024) as f64) * 100.0
        };

        let round_failure_rate = 100.0 - self.round_success_rate();

        if io_error_rate > 5.0 || round_failure_rate > 20.0 || self.network_errors > 100 {
            HealthStatus::Critical
        } else if io_error_rate > 1.0 || round_failure_rate > 10.0 || self.network_errors > 50 {
            HealthStatus::Warning
        } else {
            HealthStatus::Healthy
        }
    }
}

impl Default for MinerMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Health status of the miner
#[derive(Debug, Clone, PartialEq)]
pub enum HealthStatus {
    Healthy,
    Warning,
    Critical,
}

/// Thread-safe metrics wrapper
pub type SharedMetrics = Arc<RwLock<MinerMetrics>>;

/// Create a new shared metrics instance
pub fn new_shared_metrics() -> SharedMetrics {
    Arc::new(RwLock::new(MinerMetrics::new()))
}

/// Disk health monitor
/// Some fields and methods are intentionally kept for future monitoring/debugging use
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DiskHealthInfo {
    pub drive_id: String,
    pub total_reads: u64,
    pub failed_reads: u64,
    pub last_error: Option<Instant>,
    pub consecutive_errors: u32,
}

#[allow(dead_code)]
impl DiskHealthInfo {
    pub fn new(drive_id: String) -> Self {
        Self {
            drive_id,
            total_reads: 0,
            failed_reads: 0,
            last_error: None,
            consecutive_errors: 0,
        }
    }

    /// Record a successful read
    pub fn record_success(&mut self) {
        self.total_reads += 1;
        self.consecutive_errors = 0;
    }

    /// Record a failed read
    pub fn record_failure(&mut self) {
        self.total_reads += 1;
        self.failed_reads += 1;
        self.last_error = Some(Instant::now());
        self.consecutive_errors += 1;
    }

    /// Get error rate as percentage
    pub fn error_rate(&self) -> f64 {
        if self.total_reads == 0 {
            0.0
        } else {
            (self.failed_reads as f64 / self.total_reads as f64) * 100.0
        }
    }

    /// Check if disk is healthy
    pub fn is_healthy(&self) -> bool {
        self.error_rate() < 1.0 && self.consecutive_errors < 5
    }

    /// Get health status
    pub fn health_status(&self) -> HealthStatus {
        if self.consecutive_errors >= 10 || self.error_rate() > 5.0 {
            HealthStatus::Critical
        } else if self.consecutive_errors >= 5 || self.error_rate() > 1.0 {
            HealthStatus::Warning
        } else {
            HealthStatus::Healthy
        }
    }
}

/// Disk health monitor for all drives
#[allow(dead_code)]
pub struct DiskHealthMonitor {
    drives: HashMap<String, DiskHealthInfo>,
}

#[allow(dead_code)]
impl DiskHealthMonitor {
    pub fn new() -> Self {
        Self {
            drives: HashMap::new(),
        }
    }

    /// Get or create disk health info
    pub fn get_or_create(&mut self, drive_id: &str) -> &mut DiskHealthInfo {
        self.drives
            .entry(drive_id.to_string())
            .or_insert_with(|| DiskHealthInfo::new(drive_id.to_string()))
    }

    /// Get health summary for all drives
    pub fn health_summary(&self) -> String {
        let mut summary = String::new();
        summary.push_str("=== DISK HEALTH ===\n");

        for (drive_id, info) in &self.drives {
            let status = match info.health_status() {
                HealthStatus::Healthy => "✓ HEALTHY",
                HealthStatus::Warning => "⚠ WARNING",
                HealthStatus::Critical => "✗ CRITICAL",
            };

            summary.push_str(&format!(
                "Drive {}: {} (errors: {}/{}, rate: {:.2}%, consecutive: {})\n",
                drive_id, status, info.failed_reads, info.total_reads,
                info.error_rate(), info.consecutive_errors
            ));
        }

        summary
    }

    /// Check if any drive is unhealthy
    pub fn has_unhealthy_drives(&self) -> bool {
        self.drives.values().any(|info| !info.is_healthy())
    }
}

impl Default for DiskHealthMonitor {
    fn default() -> Self {
        Self::new()
    }
}

pub type SharedDiskHealth = Arc<RwLock<DiskHealthMonitor>>;

pub fn new_shared_disk_health() -> SharedDiskHealth {
    Arc::new(RwLock::new(DiskHealthMonitor::new()))
}
