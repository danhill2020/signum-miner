//! The plotter: generates PoC2 plot files for an account on the CPU or, with
//! the `opencl` feature, on a GPU.
//!
//! Pipeline: the generator fills a scoop-major *chunk* of `chunk` nonces
//! (one or more generator batches) while the writer thread writes the
//! previous chunk, so hashing and disk I/O overlap.  Two chunk buffers are
//! allocated: memory use is `2 * chunk * 256 KiB`.

pub mod cpu;
pub mod poc2;
pub mod writer;

#[cfg(feature = "opencl")]
pub mod gpu;

pub use cpu::CpuGenerator;
use poc2::NONCE_SIZE;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use writer::PlotWriter;

/// Something that produces consecutive nonces straight into a scoop-major
/// chunk: scoop `s` of nonce index `n` lives at `(s * chunk_nonces + n) * 64`.
pub trait NonceGenerator: Send {
    fn name(&self) -> String;
    /// Most nonces a single `generate` call can produce; 0 means any number.
    fn max_batch(&self) -> usize;
    /// Generate `count` nonces starting at `start_nonce` as nonce indexes
    /// `offset..offset + count` of `chunk` (which holds `chunk_nonces`).
    fn generate(
        &mut self,
        account_id: u64,
        start_nonce: u64,
        count: usize,
        chunk: &mut [u8],
        chunk_nonces: usize,
        offset: usize,
    ) -> io::Result<()>;
}

/// What to plot.
#[derive(Clone, Debug)]
pub struct PlotJob {
    pub account_id: u64,
    pub start_nonce: u64,
    pub nonces: u64,
    pub directory: PathBuf,
    /// Nonces per chunk (write unit); two chunk buffers are kept in memory.
    pub chunk: usize,
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
    pub fn chunk_nonces(&self) -> usize {
        (self.chunk.max(1) as u64).min(self.nonces.max(1)) as usize
    }
    /// Bytes of RAM the two chunk buffers take.
    pub fn buffer_bytes(&self) -> u64 {
        2 * self.chunk_nonces() as u64 * NONCE_SIZE as u64
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

/// Run a plot job.  `on_progress` is called after every generator batch with
/// the number of nonces generated so far.
pub fn run(
    job: &PlotJob,
    generator: &mut dyn NonceGenerator,
    mut on_progress: impl FnMut(Progress),
) -> io::Result<PathBuf> {
    let chunk_nonces = job.chunk_nonces();
    let max_batch = generator.max_batch();
    let batch = if max_batch == 0 {
        chunk_nonces
    } else {
        max_batch.min(chunk_nonces)
    };
    let path = job.path();
    let started = Instant::now();

    let mut writer = if job.dry_run {
        None
    } else {
        Some(PlotWriter::create(&path, job.nonces)?)
    };

    // Writer thread: receives (first, count, chunk), writes, hands the buffer back.
    let (tx_full, rx_full) = mpsc::sync_channel::<(u64, usize, Vec<u8>)>(1);
    let (tx_empty, rx_empty) = mpsc::channel::<Vec<u8>>();
    for _ in 0..2 {
        tx_empty
            .send(vec![0u8; chunk_nonces * NONCE_SIZE])
            .expect("channel open");
    }
    let writer_thread = thread::spawn(move || -> io::Result<()> {
        for (first, count, buf) in rx_full {
            if let Some(w) = writer.as_mut() {
                w.write_chunk(first, &buf, count)?;
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
    'chunks: while done < job.nonces {
        let count = chunk_nonces.min((job.nonces - done) as usize);
        let Ok(mut buf) = rx_empty.recv() else {
            break; // writer died; its error is collected below
        };
        let mut offset = 0usize;
        while offset < count {
            let n = batch.min(count - offset);
            if let Err(e) = generator.generate(
                job.account_id,
                job.start_nonce + done + offset as u64,
                n,
                &mut buf,
                count,
                offset,
            ) {
                result = Err(e);
                break 'chunks;
            }
            offset += n;
            let elapsed = started.elapsed();
            let generated = done + offset as u64;
            on_progress(Progress {
                done: generated,
                total: job.nonces,
                nonces_per_minute: generated as f64 / elapsed.as_secs_f64().max(1e-9) * 60.0,
                elapsed,
            });
        }
        if tx_full.send((done, count, buf)).is_err() {
            break;
        }
        done += count as u64;
    }
    drop(tx_full);
    let writer_result = writer_thread
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("writer thread panicked")));
    result?;
    writer_result?;
    Ok(path)
}

/// Chunk size (nonces) for a memory budget of `bytes` across both buffers.
pub fn chunk_for_memory(bytes: u64) -> usize {
    (bytes / 2 / NONCE_SIZE as u64).clamp(16, 1 << 20) as usize
}

/// Default chunk: a quarter of the machine's RAM for the two buffers, at
/// most 8192 nonces (2 GiB per buffer) and at least 256 (64 MiB per buffer).
pub fn default_chunk_nonces() -> usize {
    let ram = total_memory().unwrap_or(4 << 30);
    chunk_for_memory(ram / 4).clamp(256, 8192)
}

#[cfg(unix)]
fn total_memory() -> Option<u64> {
    // SAFETY: plain sysconf queries.
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if pages <= 0 || page <= 0 {
        return None;
    }
    Some(pages as u64 * page as u64)
}

#[cfg(not(unix))]
fn total_memory() -> Option<u64> {
    None
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

    /// Chunk of 3 with a generator limited to 2 per batch: exercises
    /// sub-batching, a partial last chunk and the scoop-major writes.
    #[test]
    fn run_pipeline_writes_a_correct_plot() {
        struct Limited(CpuGenerator);
        impl NonceGenerator for Limited {
            fn name(&self) -> String {
                self.0.name()
            }
            fn max_batch(&self) -> usize {
                2
            }
            fn generate(
                &mut self,
                a: u64,
                s: u64,
                c: usize,
                chunk: &mut [u8],
                cn: usize,
                o: usize,
            ) -> io::Result<()> {
                self.0.generate(a, s, c, chunk, cn, o)
            }
        }
        let dir = std::env::temp_dir().join(format!("signum-plotter-r-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let job = PlotJob {
            account_id: 10282355196851764065,
            start_nonce: 0,
            nonces: 8,
            directory: dir.clone(),
            chunk: 3,
            dry_run: false,
        };
        let mut progress = Vec::new();
        let path = run(&job, &mut Limited(CpuGenerator::new()), |p| {
            progress.push(p.done)
        })
        .unwrap();
        assert_eq!(progress, vec![2, 3, 5, 6, 8]);
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
    fn progress_math_and_chunk_sizing() {
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
        assert_eq!(chunk_for_memory(2 << 30), 4096);
        assert_eq!(chunk_for_memory(1), 16);
        let d = default_chunk_nonces();
        assert!((256..=8192).contains(&d));
    }
}
