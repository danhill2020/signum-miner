use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Lock a mutex, recovering the inner value if a thread panicked while holding it.
///
/// Release builds abort on panic so poisoning cannot happen there; this keeps
/// the test profile (which unwinds) from cascading one failure into many.
pub fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            error!("mutex poisoned, recovering");
            poisoned.into_inner()
        }
    }
}

pub fn read_or_recover<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    match lock.read() {
        Ok(guard) => guard,
        Err(poisoned) => {
            error!("rwlock poisoned, recovering");
            poisoned.into_inner()
        }
    }
}

pub fn write_or_recover<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    match lock.write() {
        Ok(guard) => guard,
        Err(poisoned) => {
            error!("rwlock poisoned, recovering");
            poisoned.into_inner()
        }
    }
}

pub fn new_thread_pool(num_threads: usize, thread_pinning: bool) -> rayon::ThreadPool {
    let core_ids = if thread_pinning {
        match core_affinity::get_core_ids() {
            Some(ids) => ids,
            None => {
                warn!("Failed to get core IDs for thread pinning, disabling pinning");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let has_pinning = thread_pinning && !core_ids.is_empty();

    rayon::ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .start_handler(move |id| {
            if has_pinning {
                #[cfg(not(windows))]
                let core_id = core_ids[id % core_ids.len()];
                #[cfg(not(windows))]
                core_affinity::set_for_current(core_id);
                #[cfg(windows)]
                set_thread_ideal_processor(id % core_ids.len());
            }
        })
        .build()
        .unwrap_or_else(|e| {
            error!("Failed to build thread pool: {}, using default", e);
            rayon::ThreadPoolBuilder::new()
                .num_threads(num_threads)
                .build()
                .expect("Failed to build fallback thread pool")
        })
}

/// Where a plot directory physically lives, as far as we can tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusType {
    Usb,
    Nvme,
    Sata,
    Removable,
    Network,
    Unknown,
}

impl BusType {
    pub fn label(self) -> &'static str {
        match self {
            BusType::Usb => "USB",
            BusType::Nvme => "NVMe",
            BusType::Sata => "SATA",
            BusType::Removable => "removable",
            BusType::Network => "network",
            BusType::Unknown => "",
        }
    }
}

cfg_if! {
    if #[cfg(unix)] {
        use std::os::unix::fs::MetadataExt;
        use std::path::Path;

        /// Identify the device a path lives on.  Plots on the same device are
        /// read by the same reader thread, so this must be stable and must
        /// differ between physical drives.  `st_dev` gives us exactly that
        /// without shelling out to `stat`.
        pub fn get_device_id(path: &str) -> String {
            match std::fs::metadata(path) {
                Ok(m) => format!("{:x}", m.dev()),
                Err(e) => {
                    warn!("failed to stat {}: {}", path, e);
                    "unknown".to_string()
                }
            }
        }

        #[cfg(target_os = "linux")]
        fn split_dev(dev: u64) -> (u64, u64) {
            // glibc's makedev()/major()/minor() encoding.
            let major = ((dev >> 32) & 0xffff_f000) | ((dev >> 8) & 0xfff);
            let minor = ((dev >> 12) & 0xffff_ff00) | (dev & 0xff);
            (major, minor)
        }

        /// Classify the bus a path's block device hangs off, via sysfs.
        /// Purely informational: it is logged next to the plot directory so an
        /// operator can see at a glance which bays are USB.
        pub fn get_bus_type(path: &str) -> BusType {
            #[cfg(target_os = "linux")]
            {
                let dev = match std::fs::metadata(path) {
                    Ok(m) => m.dev(),
                    Err(_) => return BusType::Unknown,
                };
                let (major, minor) = split_dev(dev);
                if major == 0 {
                    // FUSE, NFS, overlay, tmpfs... not a block device.
                    return BusType::Network;
                }
                let sys = Path::new("/sys/dev/block").join(format!("{}:{}", major, minor));
                match std::fs::canonicalize(&sys) {
                    Ok(real) => bus_type_from_sysfs_path(&real.to_string_lossy()),
                    Err(_) => BusType::Unknown,
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = path;
                BusType::Unknown
            }
        }

        #[cfg(target_os = "linux")]
        pub fn bus_type_from_sysfs_path(real: &str) -> BusType {
            if real.contains("/usb") {
                BusType::Usb
            } else if real.contains("/nvme") {
                BusType::Nvme
            } else if real.contains("/ata") {
                BusType::Sata
            } else if real.contains("/mmc") {
                BusType::Removable
            } else {
                BusType::Unknown
            }
        }
    } else {
        use winapi;
        use crate::utils::winapi::um::processthreadsapi::SetThreadIdealProcessor;
        use crate::utils::winapi::um::processthreadsapi::GetCurrentThread;
        use std::os::windows::ffi::OsStrExt;
        use std::ffi::OsStr;
        use std::iter::once;
        use std::ffi::CString;
        use std::path::Path;

        pub fn get_device_id(path: &str) -> String {
            let path_encoded: Vec<u16> = OsStr::new(path).encode_wide().chain(once(0)).collect();
            let mut volume_encoded: Vec<u16> = OsStr::new(path)
                .encode_wide()
                .chain(once(0))
                .collect();

            if unsafe {
                winapi::um::fileapi::GetVolumePathNameW(
                    path_encoded.as_ptr(),
                    volume_encoded.as_mut_ptr(),
                    path.chars().count() as u32
                )
            } == 0  {
                warn!("Failed to get volume path name for {}, using path as-is", path);
                return path.to_string();
            };
            let res = String::from_utf16_lossy(&volume_encoded);
            let v: Vec<&str> = res.split('\u{00}').collect();
            String::from(v[0])
        }

        pub fn get_bus_type(path: &str) -> BusType {
            let path_encoded = Path::new(path);
            let parent_path = if path_encoded.is_dir() {
                path_encoded
            } else {
                match path_encoded.parent() {
                    Some(p) => p,
                    None => return BusType::Unknown,
                }
            };

            let parent_str = match parent_path.to_str() {
                Some(s) => s,
                None => return BusType::Unknown,
            };

            let parent_c = match CString::new(parent_str) {
                Ok(s) => s,
                Err(_) => return BusType::Unknown,
            };

            let drive_type = unsafe { winapi::um::fileapi::GetDriveTypeA(parent_c.as_ptr()) };
            match drive_type {
                winapi::um::winbase::DRIVE_REMOVABLE => BusType::Removable,
                winapi::um::winbase::DRIVE_REMOTE => BusType::Network,
                _ => BusType::Unknown,
            }
        }

        pub fn set_thread_ideal_processor(id: usize){
            unsafe {
                SetThreadIdealProcessor(GetCurrentThread(), id as u32);
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_get_device_id() {
        if cfg!(unix) {
            let id = get_device_id("Cargo.toml");
            assert_ne!("", id);
            assert_ne!("unknown", id);
            // Two files on the same filesystem share a device id.
            assert_eq!(id, get_device_id("build.rs"));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_bus_type_from_sysfs_path() {
        assert_eq!(
            bus_type_from_sysfs_path(
                "/sys/devices/platform/axi/1000120000.pcie/1f00300000.usb/xhci-hcd.1/usb4/4-1/4-1:1.0/host0/target0:0:0/0:0:0:0/block/sda/sda1"
            ),
            BusType::Usb
        );
        assert_eq!(
            bus_type_from_sysfs_path(
                "/sys/devices/pci0000:00/0000:00:1d.0/0000:03:00.0/nvme/nvme0/nvme0n1/nvme0n1p2"
            ),
            BusType::Nvme
        );
        assert_eq!(
            bus_type_from_sysfs_path("/sys/devices/pci0000:00/0000:00:17.0/ata1/host0/target0:0:0/0:0:0:0/block/sda/sda1"),
            BusType::Sata
        );
        assert_eq!(
            bus_type_from_sysfs_path("/sys/devices/virtual/block/dm-0"),
            BusType::Unknown
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_split_dev() {
        // 8:1 (sda1) as encoded by glibc.
        assert_eq!(split_dev(0x801), (8, 1));
        // 259:3 (nvme) uses the extended minor bits.
        assert_eq!(split_dev((259u64 << 8) | 3), (259, 3));
    }
}
