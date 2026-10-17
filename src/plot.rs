use std::cmp::{max, min};
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::buffer::{AlignedBuffer, BUFFER_ALIGNMENT};

const SCOOPS_IN_NONCE: u64 = 4096;
const SHABAL256_HASH_SIZE: u64 = 32;
pub const SCOOP_SIZE: u64 = SHABAL256_HASH_SIZE * 2;
pub const NONCE_SIZE: u64 = SCOOP_SIZE * SCOOPS_IN_NONCE;

/// Alignment required for direct I/O offsets and lengths.  See `buffer.rs`.
pub const DIRECT_IO_ALIGNMENT: u64 = BUFFER_ALIGNMENT as u64;

#[derive(Clone, Debug)]
pub struct Meta {
    pub account_id: u64,
    pub start_nonce: u64,
    pub nonces: u64,
    pub name: String,
}

impl Meta {
    pub fn overlaps_with(&self, other: &Meta) -> bool {
        if self.start_nonce < other.start_nonce + other.nonces
            && other.start_nonce < self.start_nonce + self.nonces
        {
            let overlap = min(
                other.start_nonce + other.nonces,
                self.start_nonce + self.nonces,
            ) - max(self.start_nonce, other.start_nonce);
            warn!(
                "overlap: {} and {} share {} nonces!",
                self.name, other.name, overlap
            );
            true
        } else {
            false
        }
    }
}

/// Parse a plot file name of the form `<account>_<start_nonce>_<nonces>`.
pub fn parse_plot_name(name: &str) -> Option<(u64, u64, u64)> {
    let parts: Vec<&str> = name.split('_').collect();
    if parts.len() != 3 {
        return None;
    }
    let account_id = parts[0].parse::<u64>().ok()?;
    let start_nonce = parts[1].parse::<u64>().ok()?;
    let nonces = parts[2].parse::<u64>().ok()?;
    if nonces == 0 {
        return None;
    }
    start_nonce.checked_add(nonces)?;
    nonces.checked_mul(NONCE_SIZE)?;
    Some((account_id, start_nonce, nonces))
}

pub struct Plot {
    /// `meta.nonces` is the number of nonces that are *mined*: the whole
    /// file, or the finished part of an unfinished plot.
    pub meta: Meta,
    pub path: PathBuf,
    /// Nonces the file is laid out for (its name); scoop regions are this long.
    file_nonces: u64,
    /// `None` for a plot that is currently unreadable (drive went away); it is
    /// reopened at the start of every round and simply skipped until it is back.
    fh: Option<File>,
    /// Byte offset of the current scoop's region inside the file.
    scoop_base: u64,
    /// Bytes of the current scoop already handed out (including the alignment skip).
    read_offset: u64,
    use_direct_io: bool,
    dummy: bool,
}

/// Trailer that plotters (engraver, signum-plotter, ours) keep in the last 8
/// bytes of a plot: nonces written so far (u32 LE) followed by this magic.
const RESUME_MAGIC: [u8; 4] = [0xAF, 0xFE, 0xAF, 0xFE];

/// `Some(written)` when the file is an unfinished plot, `None` when it is
/// complete (or carries no marker).
///
/// The official plotter does not update the marker after its last buffer,
/// so a marker short of the full count is only trusted when the nonce it
/// points at is still unwritten (preallocated space reads as zeros) and the
/// last nonce is too.
fn unfinished_nonces(path: &Path, nonces: u64) -> io::Result<Option<u64>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = File::open(path)?;
    f.seek(SeekFrom::End(-8))?;
    let mut trailer = [0u8; 8];
    f.read_exact(&mut trailer)?;
    if trailer[4..] != RESUME_MAGIC {
        return Ok(None);
    }
    let written = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]) as u64;
    if written >= nonces {
        return Ok(None);
    }
    let mut scoop = [0u8; SCOOP_SIZE as usize];
    let mut is_blank = |nonce: u64| -> io::Result<bool> {
        f.seek(SeekFrom::Start(nonce * SCOOP_SIZE))?;
        f.read_exact(&mut scoop)?;
        Ok(scoop.iter().all(|&b| b == 0))
    };
    if is_blank(written)? && is_blank(nonces - 1)? {
        Ok(Some(written))
    } else {
        // Written past the marker: a finished plot whose plotter stopped
        // updating it (or one still being written; the next rescan sees more).
        Ok(None)
    }
}

cfg_if! {
    if #[cfg(unix)] {
        use std::os::unix::fs::{FileExt, OpenOptionsExt};
        use std::os::unix::io::AsRawFd;

        pub fn open_using_direct_io<P: AsRef<Path>>(path: P) -> io::Result<File> {
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECT)
                .open(path)
        }

        pub fn open<P: AsRef<Path>>(path: P) -> io::Result<File> {
            let fh = OpenOptions::new().read(true).open(path)?;
            // Buffered fallback: tell the kernel the access is sequential so
            // it reads ahead aggressively (USB drives need a full queue).
            unsafe {
                libc::posix_fadvise(fh.as_raw_fd(), 0, 0, libc::POSIX_FADV_SEQUENTIAL);
            }
            Ok(fh)
        }

        fn read_exact_at(fh: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
            fh.read_exact_at(buf, offset)
        }

        /// After a buffered read, drop the pages again: scoop data is never
        /// re-read, and on a Pi the page cache is better spent elsewhere.
        fn drop_cache(fh: &File, offset: u64, len: usize) {
            unsafe {
                libc::posix_fadvise(
                    fh.as_raw_fd(),
                    offset as libc::off_t,
                    len as libc::off_t,
                    libc::POSIX_FADV_DONTNEED,
                );
            }
        }
    } else {
        use std::os::windows::fs::{FileExt, OpenOptionsExt};

        const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
        const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;

        pub fn open_using_direct_io<P: AsRef<Path>>(path: P) -> io::Result<File> {
            OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_NO_BUFFERING)
                .open(path)
        }

        pub fn open<P: AsRef<Path>>(path: P) -> io::Result<File> {
            OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
                .open(path)
        }

        fn read_exact_at(fh: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
            while !buf.is_empty() {
                match fh.seek_read(buf, offset) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "failed to fill whole buffer",
                        ))
                    }
                    Ok(n) => {
                        buf = &mut buf[n..];
                        offset += n as u64;
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }

        fn drop_cache(_fh: &File, _offset: u64, _len: usize) {}
    }
}

impl Plot {
    pub fn new(path: &Path, use_direct_io: bool, dummy: bool) -> Result<Plot, Box<dyn Error>> {
        if !path.is_file() {
            return Err(From::from(format!("{} is not a file", path.display())));
        }

        let plot_file = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("plot file name is not valid UTF-8")?;
        let (account_id, start_nonce, nonces) =
            parse_plot_name(plot_file).ok_or("plot file has wrong format")?;

        let size = fs::metadata(path)?.len();
        let exp_size = nonces * NONCE_SIZE;
        if size != exp_size {
            return Err(From::from(format!(
                "expected plot size {} but got {}",
                exp_size, size
            )));
        }

        let file_nonces = nonces;
        let nonces = match unfinished_nonces(path, nonces)? {
            Some(0) => {
                return Err(From::from(format!(
                "{} is an unfinished plot with nothing written yet; finish it with signum-plotter",
                plot_file
            )))
            }
            Some(done) => {
                warn!(
                    "{} is an unfinished plot: {} of {} nonces are written; mining that part, resume it with signum-plotter (same account, start nonce and size)",
                    plot_file, done, nonces
                );
                done
            }
            None => nonces,
        };

        let mut use_direct_io = use_direct_io;
        if use_direct_io && nonces * SCOOP_SIZE < DIRECT_IO_ALIGNMENT {
            warn!(
                "not enough nonces for using direct io, falling back to buffered reads: plot={}",
                plot_file
            );
            use_direct_io = false;
        }

        let fh = if use_direct_io {
            match open_using_direct_io(path) {
                Ok(fh) => fh,
                Err(e) => {
                    warn!(
                        "direct I/O not available for {} ({}), falling back to buffered reads",
                        plot_file, e
                    );
                    use_direct_io = false;
                    open(path)?
                }
            }
        } else {
            open(path)?
        };

        Ok(Plot {
            meta: Meta {
                account_id,
                start_nonce,
                nonces,
                name: plot_file.to_string(),
            },
            path: path.to_path_buf(),
            file_nonces,
            fh: Some(fh),
            scoop_base: 0,
            read_offset: 0,
            use_direct_io,
            dummy,
        })
    }

    /// A plot that was seen before but cannot be opened right now.  It keeps
    /// its place (and its capacity) and is retried every round.
    pub fn placeholder(meta: Meta, path: PathBuf, use_direct_io: bool, dummy: bool) -> Plot {
        Plot {
            file_nonces: meta.nonces,
            meta,
            path,
            fh: None,
            scoop_base: 0,
            read_offset: 0,
            use_direct_io,
            dummy,
        }
    }

    #[cfg(test)]
    pub fn is_open(&self) -> bool {
        self.fh.is_some()
    }

    pub fn uses_direct_io(&self) -> bool {
        self.use_direct_io
    }

    /// Size in bytes of one scoop across the mined nonces of this plot.
    fn scoop_bytes(&self) -> u64 {
        self.meta.nonces * SCOOP_SIZE
    }

    /// Distance in bytes between the starts of two scoop regions (the file's
    /// layout, which an unfinished plot already has in full).
    fn scoop_stride(&self) -> u64 {
        self.file_nonces * SCOOP_SIZE
    }

    fn reopen(&mut self) -> io::Result<()> {
        if self.use_direct_io {
            match open_using_direct_io(&self.path) {
                Ok(fh) => {
                    self.fh = Some(fh);
                    return Ok(());
                }
                Err(e) => {
                    warn!(
                        "direct I/O open failed for {} ({}), switching to buffered reads",
                        self.meta.name, e
                    );
                    self.use_direct_io = false;
                }
            }
        }
        self.fh = Some(open(&self.path)?);
        Ok(())
    }

    /// Position the plot at the start of `scoop` for a new round.
    ///
    /// The file is reopened every round so a drive that went away and came
    /// back (USB bays do that) is picked up again without a restart.
    pub fn prepare(&mut self, scoop: u32) -> io::Result<()> {
        self.reopen()?;
        self.position(scoop);
        Ok(())
    }

    /// Offset bookkeeping for a new scoop, separated from the reopen so the
    /// alignment logic can be tested without a filesystem that supports
    /// direct I/O.
    fn position(&mut self, scoop: u32) {
        self.scoop_base = u64::from(scoop) * self.scoop_stride();
        self.read_offset = 0;
        if self.use_direct_io {
            // Direct I/O needs an aligned file offset.  Skip forward to the
            // next aligned position; the (at most 63) nonces skipped are
            // accounted for in `read_offset`, so start nonces stay correct.
            let r = self.scoop_base % DIRECT_IO_ALIGNMENT;
            if r != 0 {
                self.read_offset = DIRECT_IO_ALIGNMENT - r;
            }
        }
    }

    /// Read the next chunk of the current scoop into `bs`.
    ///
    /// Returns `(bytes_read, start_nonce, finished)` where `start_nonce` is the
    /// nonce number of the first 64-byte scoop in `bs`.
    pub fn read(&mut self, bs: &mut AlignedBuffer) -> io::Result<(usize, u64, bool)> {
        let scoop_bytes = self.scoop_bytes();
        let remaining = scoop_bytes.saturating_sub(self.read_offset);
        let buffer_cap = bs.len() as u64;
        let start_nonce = self.meta.start_nonce + self.read_offset / SCOOP_SIZE;

        let (mut bytes_to_read, finished) = if buffer_cap >= remaining {
            (remaining, true)
        } else {
            (buffer_cap, false)
        };
        if self.use_direct_io {
            // Direct I/O also needs an aligned length; the tail of the scoop
            // (at most 63 nonces) is dropped, as it always has been.
            bytes_to_read -= bytes_to_read % DIRECT_IO_ALIGNMENT;
        }
        let bytes_to_read = bytes_to_read as usize;

        if !self.dummy && bytes_to_read > 0 {
            let offset = self.scoop_base + self.read_offset;
            let fh = self
                .fh
                .as_ref()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "plot file is not open"))?;
            if let Err(e) = read_exact_at(fh, &mut bs[..bytes_to_read], offset) {
                if self.use_direct_io && e.kind() == io::ErrorKind::InvalidInput {
                    // The filesystem accepted O_DIRECT on open but rejects the
                    // transfer (FUSE, some network filesystems).  Retry buffered.
                    warn!(
                        "direct I/O read rejected for {} ({}), switching to buffered reads",
                        self.meta.name, e
                    );
                    self.use_direct_io = false;
                    let fh = open(&self.path)?;
                    read_exact_at(&fh, &mut bs[..bytes_to_read], offset)?;
                    drop_cache(&fh, offset, bytes_to_read);
                    self.fh = Some(fh);
                } else {
                    return Err(e);
                }
            } else if !self.use_direct_io {
                drop_cache(fh, offset, bytes_to_read);
            }
        }
        self.read_offset += bytes_to_read as u64;

        Ok((bytes_to_read, start_nonce, finished))
    }

    /// Touch the drive by reading one block from a random scoop.
    ///
    /// A bare seek never reaches the device, so it cannot keep an external
    /// enclosure from spinning down; an actual read can.
    pub fn wakeup(&mut self) -> io::Result<()> {
        let rand_scoop = pseudo_random() % SCOOPS_IN_NONCE;
        let mut addr = rand_scoop * self.scoop_bytes();
        addr -= addr % DIRECT_IO_ALIGNMENT;
        let len = min(DIRECT_IO_ALIGNMENT, self.meta.nonces * NONCE_SIZE - addr) as usize;
        let mut buf = AlignedBuffer::new(DIRECT_IO_ALIGNMENT as usize);
        self.reopen()?;
        let fh = self.fh.as_ref().expect("reopen sets the file handle");
        read_exact_at(fh, &mut buf[..len], addr)
    }
}

/// A cheap, dependency-free random number good enough for picking a scoop.
fn pseudo_random() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut x = nanos ^ 0x9E37_79B9_7F4A_7C15;
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{generate_nonce, scoop_of};

    const TEST_ACCOUNT: u64 = 10282355196851764065;

    fn test_plot_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test_data")
            .join(format!("{}_0_8", TEST_ACCOUNT))
    }

    /// Reads every chunk of `scoop` with a buffer holding `nonces_per_chunk`
    /// nonces and checks that each reported start nonce matches the bytes on
    /// disk for that nonce.
    fn assert_chunks_map_to_nonces(plot: &mut Plot, scoop: u32, nonces_per_chunk: usize) {
        let nonce_images: Vec<Vec<u8>> = (0..8).map(|n| generate_nonce(TEST_ACCOUNT, n)).collect();

        plot.prepare(scoop).expect("prepare");
        let mut buf = AlignedBuffer::new(nonces_per_chunk * SCOOP_SIZE as usize);
        let mut seen = 0u64;
        loop {
            let (bytes, start_nonce, finished) = plot.read(&mut buf).expect("read");
            assert_eq!(bytes % SCOOP_SIZE as usize, 0);
            assert_eq!(start_nonce, seen, "chunk start nonce for scoop {}", scoop);
            for k in 0..bytes / SCOOP_SIZE as usize {
                let nonce = start_nonce + k as u64;
                let expected = scoop_of(&nonce_images[nonce as usize], scoop);
                let got = &buf[k * 64..k * 64 + 64];
                assert_eq!(got, expected, "scoop {} nonce {}", scoop, nonce);
            }
            seen += (bytes / 64) as u64;
            if finished {
                break;
            }
        }
        assert_eq!(seen, 8, "all nonces of scoop {} read", scoop);
    }

    #[test]
    fn read_maps_bytes_to_correct_nonces_for_several_scoops() {
        for scoop in [0u32, 1, 7, 2048, 4094] {
            let mut plot = Plot::new(&test_plot_path(), false, false).expect("open plot");
            assert_chunks_map_to_nonces(&mut plot, scoop, 3);
            assert_chunks_map_to_nonces(&mut plot, scoop, 8);
            assert_chunks_map_to_nonces(&mut plot, scoop, 64);
        }
    }

    #[test]
    fn parse_plot_name_accepts_valid_and_rejects_junk() {
        assert_eq!(parse_plot_name("123_0_1000"), Some((123, 0, 1000)));
        assert_eq!(parse_plot_name("123_5_0"), None);
        assert_eq!(parse_plot_name("123_0"), None);
        assert_eq!(parse_plot_name("a_b_c"), None);
        assert_eq!(parse_plot_name("123_18446744073709551615_2"), None);
    }

    #[test]
    fn tiny_plot_never_uses_direct_io() {
        // 8 nonces * 64 bytes is below the 4 KiB alignment, so direct I/O
        // must be refused regardless of the request.
        let plot = Plot::new(&test_plot_path(), true, false).expect("open plot");
        assert!(!plot.uses_direct_io());
    }

    #[test]
    fn direct_io_alignment_math_keeps_start_nonce_correct() {
        // Simulate a 100-nonce plot (6400 bytes per scoop, not 4 KiB aligned).
        // Scoop 1 starts at byte 6400; the aligned start is 8192, i.e. 1792
        // bytes = 28 nonces later.  Those nonces are skipped and the start
        // nonce reported for the first chunk must reflect that.
        let dir = std::env::temp_dir().join(format!("signum-miner-align-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("1_0_100");
        std::fs::write(&path, vec![0u8; 100 * NONCE_SIZE as usize]).unwrap();

        let mut plot = Plot::new(&path, true, true).expect("open plot");
        // `dummy` mode skips the actual read so this works even where O_DIRECT
        // is unsupported; the offset bookkeeping is what we are testing.
        plot.use_direct_io = true;
        plot.position(1);
        let mut buf = AlignedBuffer::new(4096);
        let (bytes, start_nonce, finished) = plot.read(&mut buf).unwrap();
        assert_eq!(start_nonce, 28);
        assert_eq!(bytes, 4096);
        assert!(!finished);
        let (bytes, start_nonce, finished) = plot.read(&mut buf).unwrap();
        assert_eq!(start_nonce, 28 + 64);
        // 6400 - 1792 - 4096 = 512 bytes remain, below one alignment unit.
        assert_eq!(bytes, 0);
        assert!(finished);

        plot.position(0);
        let (bytes, start_nonce, _) = plot.read(&mut buf).unwrap();
        assert_eq!((bytes, start_nonce), (4096, 0));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn placeholder_plot_fails_prepare_until_file_exists() {
        let dir = std::env::temp_dir().join(format!("signum-miner-ph-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("7_0_8");
        let meta = Meta {
            account_id: 7,
            start_nonce: 0,
            nonces: 8,
            name: "7_0_8".into(),
        };
        let mut plot = Plot::placeholder(meta, path.clone(), false, false);
        assert!(!plot.is_open());
        assert!(plot.prepare(3).is_err());
        let mut buf = AlignedBuffer::new(512);
        assert!(plot.read(&mut buf).is_err());

        std::fs::copy(test_plot_path(), &path).unwrap();
        plot.prepare(3).expect("prepare once the file is back");
        assert!(plot.is_open());
        let (bytes, start_nonce, finished) = plot.read(&mut buf).unwrap();
        assert_eq!((bytes, start_nonce, finished), (512, 0, true));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wakeup_reads_without_error() {
        let mut plot = Plot::new(&test_plot_path(), false, false).expect("open plot");
        plot.wakeup().expect("wakeup read");
    }
}
