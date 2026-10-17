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
    pub meta: Meta,
    pub path: PathBuf,
    fh: File,
    /// Byte offset of the current scoop's region inside the file.
    scoop_base: u64,
    /// Bytes of the current scoop already handed out (including the alignment skip).
    read_offset: u64,
    use_direct_io: bool,
    dummy: bool,
}

cfg_if! {
    if #[cfg(unix)] {
        use std::os::unix::fs::{FileExt, OpenOptionsExt};

        pub fn open_using_direct_io<P: AsRef<Path>>(path: P) -> io::Result<File> {
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECT)
                .open(path)
        }

        pub fn open<P: AsRef<Path>>(path: P) -> io::Result<File> {
            OpenOptions::new().read(true).open(path)
        }

        fn read_exact_at(fh: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
            fh.read_exact_at(buf, offset)
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
            fh,
            path: path.to_path_buf(),
            scoop_base: 0,
            read_offset: 0,
            use_direct_io,
            dummy,
        })
    }

    pub fn uses_direct_io(&self) -> bool {
        self.use_direct_io
    }

    /// Size in bytes of one scoop across all nonces of this plot.
    fn scoop_bytes(&self) -> u64 {
        self.meta.nonces * SCOOP_SIZE
    }

    fn reopen(&mut self) -> io::Result<()> {
        if self.use_direct_io {
            match open_using_direct_io(&self.path) {
                Ok(fh) => {
                    self.fh = fh;
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
        self.fh = open(&self.path)?;
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
        self.scoop_base = u64::from(scoop) * self.scoop_bytes();
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
            if let Err(e) = read_exact_at(&self.fh, &mut bs[..bytes_to_read], offset) {
                if self.use_direct_io && e.kind() == io::ErrorKind::InvalidInput {
                    // The filesystem accepted O_DIRECT on open but rejects the
                    // transfer (FUSE, some network filesystems).  Retry buffered.
                    warn!(
                        "direct I/O read rejected for {} ({}), switching to buffered reads",
                        self.meta.name, e
                    );
                    self.use_direct_io = false;
                    self.fh = open(&self.path)?;
                    read_exact_at(&self.fh, &mut bs[..bytes_to_read], offset)?;
                } else {
                    return Err(e);
                }
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
        read_exact_at(&self.fh, &mut buf[..len], addr)
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
    fn wakeup_reads_without_error() {
        let mut plot = Plot::new(&test_plot_path(), false, false).expect("open plot");
        plot.wakeup().expect("wakeup read");
    }
}
