//! The plotter: generates PoC2 plot files for an account on the CPU or, with
//! the `opencl` feature, on a GPU.

pub mod poc2;
pub mod writer;

#[cfg(feature = "opencl")]
pub mod gpu;

use poc2::{GEN_SIZE, NONCE_SIZE};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use writer::PlotWriter;

/// Something that fills a nonce-major buffer with consecutive nonces.
pub trait NonceGenerator: Send {
    fn name(&self) -> String;
    /// Distance in bytes between consecutive nonces in the buffers it fills.
    fn stride(&self) -> usize;
    /// Generate `count` nonces starting at `start_nonce` into `out`
    /// (`count * stride()` bytes).
    fn generate(
        &mut self,
        account_id: u64,
        start_nonce: u64,
        count: usize,
        out: &mut [u8],
    ) -> io::Result<()>;
}

pub struct CpuGenerator;

impl NonceGenerator for CpuGenerator {
    fn name(&self) -> String {
        format!("CPU ({} threads)", rayon::current_num_threads())
    }
    fn stride(&self) -> usize {
        NONCE_SIZE
    }
    fn generate(
        &mut self,
        account_id: u64,
        start_nonce: u64,
        count: usize,
        out: &mut [u8],
    ) -> io::Result<()> {
        poc2::generate_batch(account_id, start_nonce, &mut out[..count * NONCE_SIZE]);
        Ok(())
    }
}

/// What to plot.
#[derive(Clone, Debug)]
pub struct PlotJob {
    pub account_id: u64,
    pub start_nonce: u64,
    pub nonces: u64,
    pub directory: PathBuf,
    /// Nonces generated per batch.
    pub batch: usize,
    /// Generate only, do not write (benchmark).
    pub dry_run: bool,
}

impl PlotJob {
    pub fn file_name(&self) -> String {
        format!("{}_{}_{}", self.account_id, self.start_nonce, self.nonces)
    }
    pub fn path(&self) -> PathBuf {
        self.directory.join(self.file_name())
    }
}

/// Progress callback payload.
#[derive(Clone, Copy, Debug)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
    pub nonces_per_minute: f64,
    pub elapsed: Duration,
}

impl Progress {
    pub fn percent(&self) -> f64 {
        if self.total == 0 {
            100.0
        } else {
            self.done as f64 * 100.0 / self.total as f64
        }
    }
    pub fn eta(&self) -> Option<Duration> {
        if self.nonces_per_minute <= 0.0 || self.done >= self.total {
            return None;
        }
        let remaining = (self.total - self.done) as f64;
        Some(Duration::from_secs_f64(
            remaining / self.nonces_per_minute * 60.0,
        ))
    }
}

/// Run a plot job: generation and writing overlap through a two-buffer
/// pipeline (generate batch N+1 while batch N is written).
pub fn run(
    job: &PlotJob,
    generator: &mut dyn NonceGenerator,
    mut on_progress: impl FnMut(Progress),
) -> io::Result<PathBuf> {
    let stride = generator.stride();
    let batch = job.batch.max(1);
    let buffer_len = batch * stride.max(GEN_SIZE);
    let path = job.path();
    let started = Instant::now();

    let mut writer = if job.dry_run {
        None
    } else {
        Some(PlotWriter::create(&path, job.nonces, batch)?)
    };

    // Writer thread: receives (first, count, buffer), writes, hands the buffer back.
    let (tx_full, rx_full) = mpsc::sync_channel::<(u64, usize, Vec<u8>)>(1);
    let (tx_empty, rx_empty) = mpsc::channel::<Vec<u8>>();
    for _ in 0..2 {
        tx_empty.send(vec![0u8; buffer_len]).expect("channel open");
    }
    let writer_thread = thread::spawn(move || -> io::Result<()> {
        for (first, count, buf) in rx_full {
            if let Some(w) = writer.as_mut() {
                w.write_batch(first, &buf, count, stride)?;
            }
            if tx_empty.send(buf).is_err() {
                break;
            }
        }
        if let Some(w) = writer {
            w.finish()?;
        }
        Ok(())
    });

    let mut done = 0u64;
    let mut result = Ok(());
    while done < job.nonces {
        let count = batch.min((job.nonces - done) as usize);
        let Ok(mut buf) = rx_empty.recv() else {
            break; // writer died; its error is collected below
        };
        if let Err(e) = generator.generate(job.account_id, job.start_nonce + done, count, &mut buf)
        {
            result = Err(e);
            break;
        }
        if tx_full
            .send((job.start_nonce + done - job.start_nonce, count, buf))
            .is_err()
        {
            break;
        }
        done += count as u64;
        let elapsed = started.elapsed();
        on_progress(Progress {
            done,
            total: job.nonces,
            nonces_per_minute: done as f64 / elapsed.as_secs_f64().max(1e-9) * 60.0,
            elapsed,
        });
    }
    drop(tx_full);
    let writer_result = writer_thread
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("writer thread panicked")));
    result?;
    writer_result?;
    Ok(path)
}

/// Highest end nonce of existing plots for `account_id` in `dir`, so a new
/// plot can continue where the last one stopped without overlapping.
pub fn next_free_nonce(dir: &Path, account_id: u64) -> u64 {
    let mut next = 0u64;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let parts: Vec<&str> = name.split('_').collect();
            if parts.len() != 3 {
                continue;
            }
            let (Ok(id), Ok(start), Ok(count)) = (
                parts[0].parse::<u64>(),
                parts[1].parse::<u64>(),
                parts[2].parse::<u64>(),
            ) else {
                continue;
            };
            if id == account_id {
                next = next.max(start.saturating_add(count));
            }
        }
    }
    next
}

pub fn format_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}s", s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_free_nonce_skips_other_accounts_and_junk() {
        let dir = std::env::temp_dir().join(format!("signum-plotter-n-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["5_0_100", "5_100_50", "6_0_1000", "notes.txt", "5_x_1"] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        assert_eq!(next_free_nonce(&dir, 5), 150);
        assert_eq!(next_free_nonce(&dir, 6), 1000);
        assert_eq!(next_free_nonce(&dir, 7), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn run_pipeline_writes_a_correct_plot() {
        let dir = std::env::temp_dir().join(format!("signum-plotter-r-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let job = PlotJob {
            account_id: 10282355196851764065,
            start_nonce: 0,
            nonces: 8,
            directory: dir.clone(),
            batch: 3,
            dry_run: false,
        };
        let mut progress = Vec::new();
        let path = run(&job, &mut CpuGenerator, |p| progress.push(p.done)).unwrap();
        assert_eq!(progress, vec![3, 6, 8]);
        let ours = std::fs::read(&path).unwrap();
        let reference = std::fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("test_data")
                .join("10282355196851764065_0_8"),
        )
        .unwrap();
        assert!(ours[..ours.len() - 8] == reference[..reference.len() - 8]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn progress_math() {
        let p = Progress {
            done: 50,
            total: 200,
            nonces_per_minute: 100.0,
            elapsed: Duration::from_secs(30),
        };
        assert_eq!(p.percent(), 25.0);
        assert_eq!(p.eta().unwrap().as_secs(), 90);
        assert_eq!(format_duration(Duration::from_secs(3725)), "1h 02m");
        assert_eq!(format_duration(Duration::from_secs(65)), "1m 05s");
    }
}
