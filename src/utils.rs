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

cfg_if! {
    if #[cfg(unix)] {
        use std::process::Command;

        pub fn get_device_id(path: &str) -> String {
            match Command::new("stat")
                .arg(path)
                .args(["-c", "%D"])
                .output()
            {
                Ok(output) => {
                    String::from_utf8(output.stdout)
                        .unwrap_or_else(|e| {
                            warn!("stat output not UTF-8 for {}: {}", path, e);
                            "unknown".to_string()
                        })
                        .trim_end()
                        .to_owned()
                }
                Err(e) => {
                    warn!("Failed to execute 'stat -c %D' for {}: {}", path, e);
                    "unknown".to_string()
                }
            }
        }

        // On unix, get the device id from 'df' command
        fn get_device_id_unix(path: &str) -> String {
            match Command::new("df").arg(path).output() {
                Ok(output) => {
                    let source = String::from_utf8(output.stdout)
                        .unwrap_or_else(|e| {
                            warn!("df output not UTF-8 for {}: {}", path, e);
                            String::from("unknown")
                        });
                    let lines: Vec<&str> = source.split('\n').collect();
                    if lines.len() > 1 {
                        lines[1]
                            .split_whitespace()
                            .next()
                            .unwrap_or("unknown")
                            .to_string()
                    } else {
                        warn!("df output has unexpected format for {}", path);
                        String::from("unknown")
                    }
                }
                Err(e) => {
                    warn!("Failed to execute 'df' for {}: {}", path, e);
                    String::from("unknown")
                }
            }
        }

        // On macos, use df and 'diskutil info <device>' to get the Device Block Size line
        // and extract the size
        fn get_sector_size_macos(path: &str) -> u64 {
            let source = get_device_id_unix(path);
            match Command::new("diskutil").arg("info").arg(&source).output() {
                Ok(output) => {
                    let source = String::from_utf8(output.stdout)
                        .unwrap_or_else(|e| {
                            warn!("diskutil output not UTF-8 for {}: {}", path, e);
                            String::new()
                        });
                    let mut sector_size: u64 = 0;
                    for line in source.lines() {
                        if line.trim().starts_with("Device Block Size") {
                            // e.g. in reverse: "Bytes 512 Size Block Device"
                            if let Some(size_str) = line.rsplit(' ').nth(1) {
                                sector_size = size_str.parse::<u64>().unwrap_or_else(|e| {
                                    warn!("Failed to parse sector size '{}': {}", size_str, e);
                                    0
                                });
                            }
                        }
                    }
                    if sector_size == 0 {
                        warn!("Unable to determine disk physical sector size from diskutil info for {}. Using default 4096", path);
                        4096
                    } else {
                        sector_size
                    }
                }
                Err(e) => {
                    warn!("Failed to execute 'diskutil info' for {}: {}, using default sector size 4096", path, e);
                    4096
                }
            }
        }

        // On unix, use df and lsblk to extract the device sector size
        fn get_sector_size_unix(path: &str) -> u64 {
            let source = get_device_id_unix(path);
            let output = Command::new("lsblk")
                .arg(&source)
                .arg("-o")
                .arg("PHY-SeC")
                .output()
                .map(|output| output.stdout)
                .unwrap_or_default();

            let sector_size = String::from_utf8(output)
                .unwrap_or_else(|e| {
                    warn!("lsblk output not UTF-8 for {}: {}, defaulting to 4096", path, e);
                    String::from("4096")
                });

            let lines: Vec<&str> = sector_size.split('\n').collect();
            let size_str = if lines.len() > 1 {
                lines[1].trim()
            } else {
                warn!("Failed to determine sector size for {}, defaulting to 4096", path);
                "4096"
            };

            size_str.parse::<u64>().unwrap_or_else(|e| {
                warn!("Failed to parse sector size '{}' for {}: {}, defaulting to 4096", size_str, path, e);
                4096
            })
        }

        pub fn get_sector_size(path: &str) -> u64 {
            if cfg!(target_os = "android") {
                4096
            } else if cfg!(target_os = "macos") {
                get_sector_size_macos(path)
            } else {
                get_sector_size_unix(path)
            }
        }

        pub fn get_bus_type(path: &str) -> String {
            let source = get_device_id_unix(path);
            if cfg!(target_os = "linux") {
                if let Ok(output) = Command::new("lsblk")
                    .arg(&source)
                    .arg("-ndo")
                    .arg("TRAN")
                    .output()
                {
                    if let Ok(bus) = String::from_utf8(output.stdout) {
                        return bus.lines().next().unwrap_or("").trim().to_lowercase();
                    }
                }
            }
            String::from("unknown")
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

        pub fn get_sector_size(path: &str) -> u64 {
            let path_encoded = Path::new(path);
            let parent_path = match path_encoded.parent() {
                Some(p) => match p.to_str() {
                    Some(s) => s,
                    None => {
                        warn!("Failed to convert parent path to string for {}, using default sector size 4096", path);
                        return 4096;
                    }
                },
                None => {
                    warn!("Failed to get parent path for {}, using default sector size 4096", path);
                    return 4096;
                }
            };

            let parent_path_encoded = match CString::new(parent_path) {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to create CString from parent path for {}: {}, using default sector size 4096", path, e);
                    return 4096;
                }
            };

            let mut sectors_per_cluster  = 0u32;
            let mut bytes_per_sector  = 0u32;
            let mut number_of_free_cluster  = 0u32;
            let mut total_number_of_cluster  = 0u32;

            if unsafe {
                winapi::um::fileapi::GetDiskFreeSpaceA(
                    parent_path_encoded.as_ptr(),
                    &mut sectors_per_cluster,
                    &mut bytes_per_sector,
                    &mut number_of_free_cluster,
                    &mut total_number_of_cluster
                )
            } == 0  {
                warn!("Failed to get disk free space for {}, using default sector size 4096", path);
                return 4096;
            };
            u64::from(bytes_per_sector)
        }

        pub fn get_bus_type(path: &str) -> String {
            let path_encoded = Path::new(path);
            let parent_path = if path_encoded.is_dir() {
                path_encoded
            } else {
                match path_encoded.parent() {
                    Some(p) => p,
                    None => {
                        warn!("Failed to get parent path for {}, returning unknown bus type", path);
                        return String::from("unknown");
                    }
                }
            };

            let parent_str = match parent_path.to_str() {
                Some(s) => s,
                None => {
                    warn!("Failed to convert parent path to string for {}, returning unknown bus type", path);
                    return String::from("unknown");
                }
            };

            let parent_c = match CString::new(parent_str) {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to create CString from parent path for {}: {}, returning unknown bus type", path, e);
                    return String::from("unknown");
                }
            };

            let drive_type = unsafe { winapi::um::fileapi::GetDriveTypeA(parent_c.as_ptr()) };
            match drive_type {
                winapi::um::winbase::DRIVE_REMOVABLE => "usb",
                winapi::um::winbase::DRIVE_FIXED => "fixed",
                winapi::um::winbase::DRIVE_REMOTE => "remote",
                winapi::um::winbase::DRIVE_CDROM => "cdrom",
                winapi::um::winbase::DRIVE_RAMDISK => "ramdisk",
                _ => "unknown",
            }
            .to_string()
        }

        pub fn set_thread_ideal_processor(id: usize){
            // Set core affinity for current thread.
        unsafe {
            SetThreadIdealProcessor(
                GetCurrentThread(),
                id as u32
            );

            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    //use std::env;

    #[test]
    fn test_get_device_id() {
        if cfg!(unix) {
            assert_ne!("", get_device_id(&"Cargo.toml".to_string()));
        }
    }

    #[test]
    fn test_get_sector_size() {
        // this should be true for any platform where this test runs
        // but it doesn't exercise all platform variants
        // let cwd = env::current_dir().unwrap();
        // let test_string = cwd.into_os_string().into_string().unwrap();
        // info!("{}", test_string);
        // assert_ne!(0, get_sector_size(&test_string));
    }
}
