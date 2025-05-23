use core_affinity;
use rayon;
use std::path::PathBuf;

pub fn new_thread_pool(num_threads: usize, thread_pinning: bool) -> rayon::ThreadPool {
    let core_ids = if thread_pinning {
        core_affinity::get_core_ids().unwrap()
    } else {
        Vec::new()
    };
    rayon::ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .start_handler(move |id| {
            if thread_pinning {
                #[cfg(not(windows))]
                let core_id = core_ids[id % core_ids.len()];
                #[cfg(not(windows))]
                core_affinity::set_for_current(core_id);
                #[cfg(windows)]
                set_thread_ideal_processor(id % core_ids.len());
            }
        })
        .build()
        .unwrap()
}

cfg_if! {
    if #[cfg(unix)] {
        use std::process::Command;

        pub fn get_device_id(path: &str) -> String {
            let output = Command::new("stat")
                .arg(path)
                .arg("-c %D")
                .output()
                .expect("failed to execute 'stat -c %D'");
            String::from_utf8(output.stdout).expect("not utf8").trim_end().to_owned()
        }

        // On unix, get the device id from 'df' command
        fn get_device_id_unix(path: &str) -> String {
            let output = Command::new("df")
                 .arg(path)
                 .output()
                 .expect("failed to execute 'df'");
             let source = String::from_utf8(output.stdout).expect("not utf8");
             source.split('\n').collect::<Vec<&str>>()[1].split(' ').collect::<Vec<&str>>()[0].to_string()
         }

        // On macos, use df and 'diskutil info <device>' to get the Device Block Size line
        // and extract the size
        fn get_sector_size_macos(path: &str) -> u64 {
            let source = get_device_id_unix(path);
            let output = Command::new("diskutil")
                .arg("info")
                .arg(source)
                .output()
                .expect("failed to execute 'diskutil info'");
            let source = String::from_utf8(output.stdout).expect("not utf8");
            let mut sector_size: u64 = 0;
            for line in source.split('\n').collect::<Vec<&str>>() {
                if line.trim().starts_with("Device Block Size") {
                    // e.g. in reverse: "Bytes 512 Size Block Device"
                    let source = line.rsplit(' ').collect::<Vec<&str>>()[1];

                    sector_size = source.parse::<u64>().unwrap();
                }
            }
            if sector_size == 0 {
                sector_size = 4096;
                warn!("Abort: Unable to determine disk physical sector size from diskutil info. Using default 4096")
            }
            sector_size
        }

        // On unix, use df and lsblk to extract the device sector size
        fn get_sector_size_unix(path: &str) -> u64 {
            let source = get_device_id_unix(path);
            let output = Command::new("lsblk")
                .arg(source)
                .arg("-o")
                .arg("PHY-SeC")
                .output()
                .map(|output| output.stdout)
                .unwrap_or_default();

            let sector_size = String::from_utf8(output).expect("not utf8");
            let sector_size = sector_size.split('\n').collect::<Vec<&str>>().get(1).unwrap_or_else(|| {
                warn!("failed to determine sector size, defaulting to 4096.");
                &"4096"
            }).trim();

            sector_size.parse::<u64>().unwrap()
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

        pub fn get_mount_points() -> Vec<PathBuf> {
            let mut points = Vec::new();
            if let Ok(content) = std::fs::read_to_string("/proc/mounts") {
                for line in content.lines() {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 2 {
                        let mp = parts[1];
                        if mp != "/"
                            && !mp.starts_with("/proc")
                            && !mp.starts_with("/sys")
                            && !mp.starts_with("/dev")
                            && !mp.starts_with("/run")
                            && !mp.starts_with("/snap")
                        {
                            points.push(PathBuf::from(mp));
                        }
                    }
                }
            }
            points
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
                panic!("get volume path name");
            };
            let res = String::from_utf16_lossy(&volume_encoded);
            let v: Vec<&str> = res.split('\u{00}').collect();
            String::from(v[0])
        }

        pub fn get_sector_size(path: &str) -> u64 {
            let path_encoded = Path::new(path);
            let parent_path = path_encoded.parent().unwrap().to_str().unwrap();
            let parent_path_encoded = CString::new(parent_path).unwrap();
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
                panic!("get sector size, filename={}",path);
            };
            u64::from(bytes_per_sector)
        }

        pub fn get_bus_type(path: &str) -> String {
            let path_encoded = Path::new(path);
            let parent_path = if path_encoded.is_dir() {
                path_encoded
            } else {
                path_encoded.parent().unwrap()
            };
            let parent_c = CString::new(parent_path.to_str().unwrap()).unwrap();
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

        pub fn get_mount_points() -> Vec<PathBuf> {
            let mut points = Vec::new();
            unsafe {
                let mask = winapi::um::fileapi::GetLogicalDrives();
                for i in 0..26 {
                    if mask & (1 << i) != 0 {
                        let letter = (b'A' + i as u8) as char;
                        let path = format!("{}:\\", letter);
                        points.push(PathBuf::from(path));
                    }
                }
            }
            points
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
