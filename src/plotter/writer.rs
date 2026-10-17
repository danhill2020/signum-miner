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
//!
//! Resume marker: like engraver and the official signum-plotter, the last 8
//! bytes of the file hold the number of nonces written so far as a 32-bit
//! little-endian integer followed by `AF FE AF FE`.  It is updated after
//! every chunk, so an interrupted run (power cut, Ctrl-C) continues where it
//! stopped when started again with the same parameters, with either plotter.
//! A finished plot keeps the marker with the full nonce count.  (Those 8
//! bytes belong to the last scoop of the last nonce; every plotter sacrifices
//! them, and the miner reads them as ordinary data.)
//!
//! Direct I/O: on Linux the data is written with `O_DIRECT` so a terabyte
//! plot does not go through the page cache and squeeze out everything else
//! (the miner's buffers included).  That needs 4 KiB aligned buffers, offsets
//! and sizes, which is why nonce counts are rounded to multiples of 64 and
//! the chunk buffers are page aligned.  Filesystems that refuse `O_DIRECT`
//! get buffered writes automatically.

use super::poc2::{NONCE_SIZE, SCOOPS_PER_NONCE, SCOOP_SIZE};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Trailer that marks a resumable plot and ends every plot we write.
pub const RESUME_MAGIC: [u8; 4] = [0xAF, 0xFE, 0xAF, 0xFE];
/// Direct I/O alignment of buffers, offsets and sizes.
pub const DIRECT_IO_ALIGNMENT: usize = 4096;
/// Nonces per `DIRECT_IO_ALIGNMENT` bytes of one scoop region.
pub const NONCES_PER_BLOCK: u64 = (DIRECT_IO_ALIGNMENT / SCOOP_SIZE) as u64;

/// What `PlotWriter::open` found on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Existing {
    /// No file: a fresh plot.
    New,
    /// An unfinished plot with this many nonces already written.
    Resume(u64),
    /// The file is complete.
    Finished,
}

pub struct PlotWriter {
    /// Data writes (direct I/O when available).
    file: File,
    /// Buffered handle for the 8-byte trailer.
    trailer: File,
    total_nonces: u64,
    direct_io: bool,
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

/// Open `path` for data writes with direct I/O; `None` when the filesystem
/// does not support it.
#[cfg(target_os = "linux")]
fn open_direct(path: &Path) -> Option<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)
        .ok()
}

#[cfg(not(target_os = "linux"))]
fn open_direct(_path: &Path) -> Option<File> {
    None
}

/// Read the resume trailer of an existing file: nonces written so far, or
/// `None` when the file carries no marker.
pub fn read_resume_marker(path: &Path) -> io::Result<Option<u64>> {
    let mut f = File::open(path)?;
    if f.metadata()?.len() < 8 {
        return Ok(None);
    }
    f.seek(SeekFrom::End(-8))?;
    let mut trailer = [0u8; 8];
    f.read_exact(&mut trailer)?;
    if trailer[4..] == RESUME_MAGIC {
        Ok(Some(
            u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]) as u64,
        ))
    } else {
        Ok(None)
    }
}

impl PlotWriter {
    /// Create the plot file (reserving its full size up front, so running
    /// out of disk space fails now rather than hours later), or open an
    /// unfinished one to continue it.
    pub fn open(path: &Path, total_nonces: u64, direct_io: bool) -> io::Result<(Self, Existing)> {
        let len = total_nonces * NONCE_SIZE as u64;
        let existing = if path.is_file() {
            let size = std::fs::metadata(path)?.len();
            if size != len {
                return Err(io::Error::other(format!(
                    "{} exists with size {} but a plot of {} nonces is {} bytes; delete it or pick another start nonce",
                    path.display(),
                    size,
                    total_nonces,
                    len
                )));
            }
            match read_resume_marker(path)? {
                Some(done) if done >= total_nonces => Existing::Finished,
                // Direct I/O needs an aligned restart point; the official
                // plotter's progress is a multiple of its buffer size, ours of
                // the chunk size, so this normally changes nothing.
                Some(done) => Existing::Resume(done / NONCES_PER_BLOCK * NONCES_PER_BLOCK),
                None => {
                    return Err(io::Error::other(format!(
                        "{} exists but has no resume marker (a finished plot from another plotter, or not a plot); delete it to plot it again",
                        path.display()
                    )))
                }
            }
        } else {
            let f = OpenOptions::new().write(true).create_new(true).open(path)?;
            if let Err(e) = preallocate(&f, len) {
                drop(f);
                let _ = std::fs::remove_file(path);
                return Err(e);
            }
            Existing::New
        };
        let trailer = OpenOptions::new().write(true).open(path)?;
        let (file, direct_io) = match (direct_io, open_direct(path)) {
            (true, Some(f)) => (f, true),
            _ => (OpenOptions::new().write(true).open(path)?, false),
        };
        let mut writer = Self {
            file,
            trailer,
            total_nonces,
            direct_io,
        };
        if existing == Existing::New {
            writer.set_progress(0)?;
        }
        Ok((writer, existing))
    }

    pub fn uses_direct_io(&self) -> bool {
        self.direct_io
    }

    /// Write a scoop-major chunk of `count` nonces (`count * NONCE_SIZE`
    /// bytes, scoop `s` of nonce `n` at `(s * count + n) * 64`) as nonces
    /// `first..first + count` of the plot, then record the progress.
    pub fn write_chunk(&mut self, first: u64, chunk: &[u8], count: usize) -> io::Result<()> {
        assert!(
            first + count as u64 <= self.total_nonces,
            "chunk past end of plot"
        );
        assert!(chunk.len() >= count * NONCE_SIZE);
        let row = count * SCOOP_SIZE;
        for scoop in 0..SCOOPS_PER_NONCE {
            let offset = (scoop as u64 * self.total_nonces + first) * SCOOP_SIZE as u64;
            let data = &chunk[scoop * row..(scoop + 1) * row];
            if let Err(e) = write_all_at(&self.file, data, offset) {
                if self.direct_io && e.raw_os_error() == Some(libc_einval()) {
                    // Alignment the filesystem will not accept: continue buffered.
                    self.direct_io = false;
                    self.file = OpenOptions::new().write(true).open_path(&self.trailer)?;
                    write_all_at(&self.file, data, offset)?;
                } else {
                    return Err(e);
                }
            }
        }
        self.set_progress(first + count as u64)
    }

    /// Record `done` nonces in the trailer (flushed, so a crash right after
    /// a chunk still resumes correctly).
    fn set_progress(&mut self, done: u64) -> io::Result<()> {
        let mut trailer = [0u8; 8];
        trailer[..4].copy_from_slice(&(done.min(u32::MAX as u64) as u32).to_le_bytes());
        trailer[4..].copy_from_slice(&RESUME_MAGIC);
        self.trailer.seek(SeekFrom::End(-8))?;
        self.trailer.write_all(&trailer)?;
        self.trailer.flush()?;
        self.trailer.sync_data()
    }

    pub fn finish(self) -> io::Result<()> {
        self.file.sync_all()
    }
}

#[cfg(unix)]
fn libc_einval() -> i32 {
    libc::EINVAL
}

#[cfg(not(unix))]
fn libc_einval() -> i32 {
    -1
}

/// Reopen the same file as `handle`, buffered.
trait OpenPath {
    fn open_path(&mut self, same_as: &File) -> io::Result<File>;
}

impl OpenPath for OpenOptions {
    fn open_path(&mut self, same_as: &File) -> io::Result<File> {
        // `try_clone` duplicates the descriptor with its O_DIRECT flag, so
        // the path is needed; it is recovered through /proc on Linux.
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::io::AsRawFd;
            let link = format!("/proc/self/fd/{}", same_as.as_raw_fd());
            if let Ok(path) = std::fs::read_link(link) {
                return self.open(path);
            }
        }
        same_as.try_clone()
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

    fn reference() -> Vec<u8> {
        std::fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("test_data")
                .join("10282355196851764065_0_8"),
        )
        .unwrap()
    }

    #[test]
    fn writer_produces_scoop_major_layout_identical_to_reference_plot() {
        let dir = std::env::temp_dir().join(format!("signum-plotter-w-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("10282355196851764065_0_8");
        let (mut w, existing) = PlotWriter::open(&path, 8, true).unwrap();
        assert_eq!(existing, Existing::New);
        assert_eq!(read_resume_marker(&path).unwrap(), Some(0));
        // Three chunks of uneven size to exercise the offsets.
        for (first, count) in [(0u64, 3usize), (3, 3), (6, 2)] {
            let chunk = scoop_major_reference(10282355196851764065, first, count, count, 0);
            w.write_chunk(first, &chunk, count).unwrap();
        }
        w.finish().unwrap();

        let ours = std::fs::read(&path).unwrap();
        let reference = reference();
        assert_eq!(ours.len(), reference.len());
        assert!(ours[..ours.len() - 8] == reference[..reference.len() - 8]);
        assert_eq!(
            &ours[ours.len() - 8..],
            &[8, 0, 0, 0, 0xAF, 0xFE, 0xAF, 0xFE]
        );
        assert_eq!(read_resume_marker(&path).unwrap(), Some(8));
        // Opening it again reports a finished plot.
        let (_, existing) = PlotWriter::open(&path, 8, false).unwrap();
        assert_eq!(existing, Existing::Finished);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Stop after the first chunk, "restart", and get the same bytes.
    #[test]
    fn interrupted_plot_resumes_from_the_marker() {
        let dir = std::env::temp_dir().join(format!("signum-plotter-rs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("10282355196851764065_0_8");
        {
            let (mut w, _) = PlotWriter::open(&path, 8, false).unwrap();
            let chunk = scoop_major_reference(10282355196851764065, 0, 5, 5, 0);
            w.write_chunk(0, &chunk, 5).unwrap();
            // dropped without finish(): simulates a crash
        }
        assert_eq!(read_resume_marker(&path).unwrap(), Some(5));
        let (mut w, existing) = PlotWriter::open(&path, 8, false).unwrap();
        // 5 is rounded down to the direct I/O block (64 nonces): restart at 0.
        assert_eq!(existing, Existing::Resume(0));
        let chunk = scoop_major_reference(10282355196851764065, 0, 8, 8, 0);
        w.write_chunk(0, &chunk, 8).unwrap();
        w.finish().unwrap();
        let ours = std::fs::read(&path).unwrap();
        let reference = reference();
        assert!(ours[..ours.len() - 8] == reference[..reference.len() - 8]);

        // A file of the right size without a marker is refused rather than
        // overwritten.
        let other = dir.join("10282355196851764065_8_8");
        std::fs::write(&other, vec![0u8; 8 * NONCE_SIZE]).unwrap();
        assert!(PlotWriter::open(&other, 8, false).is_err());
        // A file of the wrong size too.
        let wrong = dir.join("10282355196851764065_16_8");
        std::fs::write(&wrong, vec![0u8; 100]).unwrap();
        assert!(PlotWriter::open(&wrong, 8, false).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scoop_offsets() {
        assert_eq!(scoop_offset(100, 0, 0), 0);
        assert_eq!(scoop_offset(100, 5, 0), 320);
        assert_eq!(scoop_offset(100, 0, 1), 6400);
    }
}
