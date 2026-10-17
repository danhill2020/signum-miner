//! Writes nonces into an "optimized" (scoop-major) PoC2 plot file.
//!
//! The file layout is `[scoop 0 of every nonce][scoop 1 of every nonce]...`,
//! which is what the miner reads: one contiguous region per scoop.  Nonces
//! are generated nonce-major, so each batch is transposed on the way out:
//! for every scoop, the batch's `count * 64` bytes are written at that
//! scoop's region plus the batch offset.  With batches of a few thousand
//! nonces every write is a few hundred KiB, which keeps drives busy.

use super::poc2::{NONCE_SIZE, SCOOPS_PER_NONCE, SCOOP_SIZE};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

pub struct PlotWriter {
    file: File,
    total_nonces: u64,
    gather: Vec<u8>,
}

cfg_if::cfg_if! {
    if #[cfg(unix)] {
        use std::os::unix::fs::FileExt;
        fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
            file.write_all_at(buf, offset)
        }
    } else {
        use std::os::windows::fs::FileExt;
        fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
            while !buf.is_empty() {
                let n = file.seek_write(buf, offset)?;
                if n == 0 {
                    return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0"));
                }
                buf = &buf[n..];
                offset += n as u64;
            }
            Ok(())
        }
    }
}

impl PlotWriter {
    /// Create (or truncate) the plot file and reserve its full size up front,
    /// so running out of disk space fails now rather than hours later.
    pub fn create(path: &Path, total_nonces: u64, max_batch: usize) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(total_nonces * NONCE_SIZE as u64)?;
        Ok(Self {
            file,
            total_nonces,
            gather: vec![0u8; max_batch * SCOOP_SIZE],
        })
    }

    /// Write `batch` (nonce-major, `count * NONCE_SIZE` bytes) as nonces
    /// `first..first + count` of the plot.  `stride` is the distance between
    /// consecutive nonces in `batch` (`NONCE_SIZE`, or `GEN_SIZE` for raw GPU
    /// output that still carries the 16-byte seed).
    pub fn write_batch(
        &mut self,
        first: u64,
        batch: &[u8],
        count: usize,
        stride: usize,
    ) -> io::Result<()> {
        assert!(
            first + count as u64 <= self.total_nonces,
            "batch past end of plot"
        );
        assert!(batch.len() >= (count - 1) * stride + NONCE_SIZE);
        assert!(count * SCOOP_SIZE <= self.gather.len());
        let gather = &mut self.gather[..count * SCOOP_SIZE];
        for scoop in 0..SCOOPS_PER_NONCE {
            for n in 0..count {
                let src = n * stride + scoop * SCOOP_SIZE;
                gather[n * SCOOP_SIZE..(n + 1) * SCOOP_SIZE]
                    .copy_from_slice(&batch[src..src + SCOOP_SIZE]);
            }
            let offset = (scoop as u64 * self.total_nonces + first) * SCOOP_SIZE as u64;
            write_all_at(&self.file, gather, offset)?;
        }
        Ok(())
    }

    pub fn finish(self) -> io::Result<()> {
        self.file.sync_all()
    }
}

/// Byte offset of `scoop` of nonce index `n` in a plot with `total` nonces.
#[allow(dead_code)]
pub fn scoop_offset(total: u64, n: u64, scoop: usize) -> u64 {
    (scoop as u64 * total + n) * SCOOP_SIZE as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plotter::poc2::generate_batch;
    use std::path::PathBuf;

    #[test]
    fn writer_produces_scoop_major_layout_identical_to_reference_plot() {
        let dir = std::env::temp_dir().join(format!("signum-plotter-w-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("10282355196851764065_0_8");
        let mut w = PlotWriter::create(&path, 8, 3).unwrap();
        // Three batches of uneven size to exercise the offsets.
        for (first, count) in [(0u64, 3usize), (3, 3), (6, 2)] {
            let mut batch = vec![0u8; count * NONCE_SIZE];
            generate_batch(10282355196851764065, first, &mut batch);
            w.write_batch(first, &batch, count, NONCE_SIZE).unwrap();
        }
        w.finish().unwrap();

        let ours = std::fs::read(&path).unwrap();
        let reference = std::fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("test_data")
                .join("10282355196851764065_0_8"),
        )
        .unwrap();
        assert_eq!(ours.len(), reference.len());
        assert!(ours[..ours.len() - 8] == reference[..reference.len() - 8]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scoop_offsets() {
        assert_eq!(scoop_offset(100, 0, 0), 0);
        assert_eq!(scoop_offset(100, 5, 0), 320);
        assert_eq!(scoop_offset(100, 0, 1), 6400);
    }
}
