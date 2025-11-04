use crate::utils::get_sector_size;
use rand::prelude::*;
use std::cmp::{max, min};
use std::error::Error;
use std::fs;
use std::fs::{File, OpenOptions};
#[cfg(feature = "async_io")]
use tokio::fs::File as TokioFile;
#[cfg(not(feature = "async_io"))]
use std::fs::File as TokioFile;
#[cfg(feature = "async_io")]
use tokio::io::{AsyncReadExt, AsyncSeekExt};
#[cfg(feature = "async_io")]
use std::io::Seek;
use std::io;
use std::io::{SeekFrom};
#[cfg(not(feature = "async_io"))]
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

const SCOOPS_IN_NONCE: u64 = 4096;
const SHABAL256_HASH_SIZE: u64 = 32;
pub const SCOOP_SIZE: u64 = SHABAL256_HASH_SIZE * 2;
const NONCE_SIZE: u64 = SCOOP_SIZE * SCOOPS_IN_NONCE;

#[derive(Clone)]
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

pub struct Plot {
    pub meta: Meta,
    pub path: String,
    pub fh: TokioFile,
    read_offset: u64,
    align_offset: u64,
    seek_base: u64,
    use_direct_io: bool,
    sector_size: u64,
    dummy: bool,
}

cfg_if! {
    if #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;

        const O_DIRECT: i32 = 0o0_040_000;

        pub fn open_using_direct_io<P: AsRef<Path>>(path: P) -> io::Result<File> {
            OpenOptions::new()
                .read(true)
                .custom_flags(O_DIRECT)
                .open(path)
        }

        pub fn open<P: AsRef<Path>>(path: P) -> io::Result<File> {
            OpenOptions::new()
                .read(true)
                .open(path)
        }

    } else {
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
        const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
        const FILE_FLAG_RANDOM_ACCESS: u32 = 0x1000_0000;

        pub fn open_using_direct_io<P: AsRef<Path>>(path: P) -> io::Result<File> {
            OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_NO_BUFFERING)
                .open(path)
        }

        pub fn open<P: AsRef<Path>>(path: P) -> io::Result<File> {
            OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN | FILE_FLAG_RANDOM_ACCESS)
                .open(path)
        }
    }
}

impl Plot {
    pub fn new(path: &PathBuf, mut use_direct_io: bool, dummy: bool) -> Result<Plot, Box<dyn Error>> {
        if !path.is_file() {
            return Err(From::from(format!(
                "{} is not a file",
                path.to_str().unwrap_or("<non-UTF8 path>")
            )));
        }

        let plot_file = path.file_name()
            .and_then(|f| f.to_str())
            .ok_or_else(|| {
                warn!("Plot file path contains invalid UTF-8: {:?}", path);
                "Plot filename contains invalid UTF-8 characters"
            })?;
        let parts: Vec<&str> = plot_file.split('_').collect();
        if parts.len() != 3 {
            return Err(From::from("plot file has wrong format"));
        }

        let account_id = parts[0].parse::<u64>()?;
        let start_nonce = parts[1].parse::<u64>()?;
        let nonces = parts[2].parse::<u64>()?;

        let size = fs::metadata(path)?.len();
        let exp_size = nonces * NONCE_SIZE;
        if size != exp_size as u64 {
            return Err(From::from(format!(
                "expected plot size {} but got {}",
                exp_size, size
            )));
        }

        let fh_std = if use_direct_io {
            open_using_direct_io(path)?
        } else {
            open(path)?
        };
        let fh = {
            #[cfg(feature = "async_io")]
            { TokioFile::from_std(fh_std) }
            #[cfg(not(feature = "async_io"))]
            { fh_std }
        };

        let plot_file_name = plot_file.to_string();
        let sector_size = get_sector_size(&path.to_str()
            .unwrap_or_else(|| {
                warn!("Plot path contains invalid UTF-8, using lossy conversion: {:?}", path);
                ""
            }).to_owned());
        if use_direct_io && sector_size / 64 > nonces {
            warn!(
                "not enough nonces for using direct io: plot={}",
                plot_file_name
            );
            use_direct_io = false;
        }

        let file_path = path.clone().into_os_string().into_string()
            .unwrap_or_else(|os_string| {
                warn!("Plot file path contains invalid UTF-8, using lossy conversion: {:?}", os_string);
                os_string.to_string_lossy().into_owned()
            });
        Ok(Plot {
            meta: Meta {
                account_id,
                start_nonce,
                nonces,
                name: plot_file_name,
            },
            fh,
            path: file_path,
            read_offset: 0,
            align_offset: 0,
            seek_base: 0,
            use_direct_io,
            sector_size,
            dummy,
        })
    }

#[cfg(not(feature = "async_io"))]
pub fn prepare(&mut self, scoop: u32) -> io::Result<u64> {
        self.read_offset = 0;
        self.align_offset = 0;
        let nonces = self.meta.nonces;
        let mut seek_addr = u64::from(scoop) * nonces as u64 * SCOOP_SIZE;

        // reopening file handles
        if !self.use_direct_io {
            self.fh = open(&self.path)?;
        } else {
            self.fh = open_using_direct_io(&self.path)?;
        };

        if self.use_direct_io {
            self.align_offset = self.round_seek_addr(&mut seek_addr);
        }
        self.seek_base = seek_addr;

        self.fh.seek(SeekFrom::Start(seek_addr))
    }

    #[cfg(feature = "async_io")]
    pub async fn prepare_async(&mut self, scoop: u32) -> io::Result<u64> {
        self.read_offset = 0;
        self.align_offset = 0;
        let nonces = self.meta.nonces;
        let mut seek_addr = u64::from(scoop) * nonces as u64 * SCOOP_SIZE;

        if !self.use_direct_io {
            let f = open(&self.path)?;
            self.fh = TokioFile::from_std(f);
        } else {
            let f = open_using_direct_io(&self.path)?;
            self.fh = TokioFile::from_std(f);
        };

        if self.use_direct_io {
            self.align_offset = self.round_seek_addr(&mut seek_addr);
        }
        self.seek_base = seek_addr;

        self.fh.seek(SeekFrom::Start(seek_addr)).await
    }

#[cfg(not(feature = "async_io"))]
    pub fn read(&mut self, bs: &mut Vec<u8>, scoop: u32) -> Result<(usize, u64, bool), io::Error> {
        let read_offset = self.read_offset;
        let buffer_cap = bs.capacity();
        let start_nonce = self.meta.start_nonce
            + u64::from(scoop) * self.meta.nonces
            + self.read_offset / 64;

        let (bytes_to_read, finished) =
            if read_offset as usize + buffer_cap >= (SCOOP_SIZE * self.meta.nonces) as usize {
                let mut bytes_to_read =
                    (SCOOP_SIZE * self.meta.nonces) as usize - self.read_offset as usize;
                if self.use_direct_io {
                    let r = bytes_to_read % self.sector_size as usize;
                    if r != 0 {
                        bytes_to_read -= r;
                    }
                }

                (bytes_to_read, true)
            } else {
                (buffer_cap as usize, false)
            };

        let offset = self.read_offset;
        let seek_addr = SeekFrom::Start(self.seek_base + self.align_offset + offset);
        if !self.dummy {
            self.fh.seek(seek_addr)?;
            self.fh.read_exact(&mut bs[0..bytes_to_read])?;
            // interrupt avoider (not implemented)
            // let read_chunk_size_in_nonces = 65536;
            // for i in (0..bytes_to_read).step_by(read_chunk_size_in_nonces) {
            //     self.fh.read_exact(
            //         &mut bs[i..(i + min(read_chunk_size_in_nonces, bytes_to_read - i))],
            //     )?;
            // }
        }
        self.read_offset += bytes_to_read as u64;

        Ok((bytes_to_read, start_nonce, finished))
    }

    #[cfg(feature = "async_io")]
    pub async fn read_async(
        &mut self,
        bs: &mut Vec<u8>,
        scoop: u32,
    ) -> Result<(usize, u64, bool), io::Error> {
        let read_offset = self.read_offset;
        let buffer_cap = bs.capacity();
        let start_nonce = self.meta.start_nonce
            + u64::from(scoop) * self.meta.nonces
            + self.read_offset / 64;

        let (bytes_to_read, finished) = if read_offset as usize + buffer_cap
            >= (SCOOP_SIZE * self.meta.nonces) as usize
        {
            let mut bytes_to_read = (SCOOP_SIZE * self.meta.nonces) as usize
                - self.read_offset as usize;
            if self.use_direct_io {
                let r = bytes_to_read % self.sector_size as usize;
                if r != 0 {
                    bytes_to_read -= r;
                }
            }
            (bytes_to_read, true)
        } else {
            (buffer_cap as usize, false)
        };

        let offset = self.read_offset;
        let seek_addr = SeekFrom::Start(self.seek_base + self.align_offset + offset);
        if !self.dummy {
            self.fh.seek(seek_addr).await?;
            self.fh.read_exact(&mut bs[0..bytes_to_read]).await?;
        }
        self.read_offset += bytes_to_read as u64;

        Ok((bytes_to_read, start_nonce, finished))
    }

#[cfg(not(feature = "async_io"))]
    pub fn seek_random(&mut self) -> io::Result<u64> {
        let mut rng = thread_rng();
        let rand_scoop = rng.gen_range(0, SCOOPS_IN_NONCE);

        let mut seek_addr = rand_scoop as u64 * self.meta.nonces as u64 * SCOOP_SIZE;
        if self.use_direct_io {
            self.round_seek_addr(&mut seek_addr);
        }

        self.fh.seek(SeekFrom::Start(seek_addr))
    }

    #[cfg(feature = "async_io")]
    pub fn seek_random(&mut self) -> io::Result<u64> {
        let mut rng = thread_rng();
        let rand_scoop = rng.gen_range(0, SCOOPS_IN_NONCE);

        let mut seek_addr = rand_scoop as u64 * self.meta.nonces as u64 * SCOOP_SIZE;
        if self.use_direct_io {
            self.round_seek_addr(&mut seek_addr);
        }

        let mut f = if self.use_direct_io {
            open_using_direct_io(&self.path)?
        } else {
            open(&self.path)?
        };

        f.seek(SeekFrom::Start(seek_addr))
    }

    fn round_seek_addr(&mut self, seek_addr: &mut u64) -> u64 {
        // Align file offset to the underlying sector size without skipping
        // the beginning of the scoop.  Older logic aligned upwards which
        // resulted in bytes at the start of a scoop being silently ignored
        // when using direct I/O.  This caused nonce calculation to be off by
        // the alignment delta when `async_io` was enabled.  Aligning downwards
        // preserves all bytes while still satisfying the O_DIRECT requirement.

        let r = *seek_addr % self.sector_size;
        if r != 0 {
            *seek_addr -= r;
        }
        r
    }
}
