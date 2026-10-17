//! Writes scoop-major chunks into an "optimized" (scoop-major) PoC2 plot file.
//!
//! The file layout is `[scoop 0 of every nonce][scoop 1 of every nonce]...`,
//! which is what the miner reads: one contiguous region per scoop.  The
//! generators already produce each chunk in that layout for the chunk's
//! nonces, so writing a chunk of `count` nonces is 4096 writes of
//! `count * 64` bytes, one per scoop region.  The bigger the chunk, the
//! larger and more sequential those writes are: with 4096 nonces per chunk
//! every write is 256 KiB, which keeps a hard drive close to its sequential
//! speed.
//!
//! The file is really preallocated (`fallocate`), not just sized: a sparse
//! file filled by 4096 interleaved write streams would fragment badly, and
//! the miner reads every scoop region sequentially for years afterwards.

use super::poc2::{NONCE_SIZE, SCOOPS_PER_NONCE, SCOOP_SIZE};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

pub struct PlotWriter {
    file: File,
    total_nonces: u64,
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

/// Reserve `len` bytes of real disk space for `file`.  Falls back to plain
/// `set_len` on filesystems without preallocation (FUSE, ntfs-3g, ...).
fn preallocate(file: &File, len: u64) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        // SAFETY: valid descriptor; mode 0 allocates and extends the file.
        let rc = unsafe { libc::fallocate(file.as_raw_fd(), 0, 0, len as libc::off_t) };
        if rc == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ENOSPC) {
            return Err(err);
        }
        // EOPNOTSUPP / ENOSYS and friends: the filesystem cannot preallocate.
    }
    file.set_len(len)
}

impl PlotWriter {
    /// Create (or truncate) the plot file and reserve its full size up front,
    /// so running out of disk space fails now rather than hours later.
    pub fn create(path: &Path, total_nonces: u64) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        preallocate(&file, total_nonces * NONCE_SIZE as u64)?;
        Ok(Self { file, total_nonces })
    }

    /// Write a scoop-major chunk of `count` nonces (`count * NONCE_SIZE`
    /// bytes, scoop `s` of nonce `n` at `(s * count + n) * 64`) as nonces
    /// `first..first + count` of the plot.
    pub fn write_chunk(&mut self, first: u64, chunk: &[u8], count: usize) -> io::Result<()> {
        assert!(
            first + count as u64 <= self.total_nonces,
            "chunk past end of plot"
        );
        assert!(chunk.len() >= count * NONCE_SIZE);
        let row = count * SCOOP_SIZE;
        for scoop in 0..SCOOPS_PER_NONCE {
            let offset = (scoop as u64 * self.total_nonces + first) * SCOOP_SIZE as u64;
            write_all_at(&self.file, &chunk[scoop * row..(scoop + 1) * row], offset)?;
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
    use crate::plotter::poc2::scoop_major_reference;
    use std::path::PathBuf;

    #[test]
    fn writer_produces_scoop_major_layout_identical_to_reference_plot() {
        let dir = std::env::temp_dir().join(format!("signum-plotter-w-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("10282355196851764065_0_8");
        let mut w = PlotWriter::create(&path, 8).unwrap();
        // Three chunks of uneven size to exercise the offsets.
        for (first, count) in [(0u64, 3usize), (3, 3), (6, 2)] {
            let chunk = scoop_major_reference(10282355196851764065, first, count, count, 0);
            w.write_chunk(first, &chunk, count).unwrap();
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
