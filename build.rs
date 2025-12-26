// Import the `cc` crate for compiling C files
extern crate cc;
use std::env;
use std::process::Command;

#[cfg(target_env = "gnu")]
fn compile_windows_icon() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("windows") {
        let out_dir = std::env::var("OUT_DIR").unwrap();

        let rc_path = "src/windows/app.rc";
        let res_path = format!("{}/icon.res", out_dir);

        // Compile .rc file into .res
        let windres_cmd = if Command::new("x86_64-w64-mingw32-windres").arg("--version").status().is_ok() {
            "x86_64-w64-mingw32-windres"  
        } else if Command::new("i686-w64-mingw32-windres").arg("--version").status().is_ok() {
            "i686-w64-mingw32-windres"   
        } else if Command::new("windres").arg("--version").status().is_ok() {
            "windres"  
        } else {
            println!("cargo:warning=Windres not found, skipping icon resource compilation.");
            return;
        };
        let status = std::process::Command::new(windres_cmd)
            .args(["--input", rc_path, "--output", &res_path, "--output-format=coff"])
            .status()
            .expect("failed to run windres");

        if !status.success() {
            panic!("windres failed with status {}", status);
        }

        // Link .res file
        println!("cargo:rustc-link-arg-bin=signum-miner={}", res_path);
    }
}

fn main() {
    let mut shared_config = cc::Build::new();

    #[cfg(target_env = "gnu")]
    compile_windows_icon();

    // Apply optimization flags depending on compiler environment
    if cfg!(target_env = "msvc") {
        // Visual Studio (MSVC) specific optimization flags
        shared_config
            .flag("/O2")
            .flag("/Oi")
            .flag("/Ot")
            .flag("/Oy")
            .flag("/GT")
            .flag("/GL");
    } else {
        // Non-MSVC: use C99 standard and tune for native CPU when not cross-compiling
        shared_config.flag("-std=c99");
        if env::var("HOST").ok() == env::var("TARGET").ok() {
            shared_config.flag("-mtune=native");
        }
    }

    // **Always compile the baseline (non-SIMD) Shabal implementation**
    let mut base_config = shared_config.clone();
    base_config
        .file("src/c/sph_shabal.c")
        .file("src/c/shabal.c")
        .file("src/c/common.c")
        .compile("shabal");

    // **Detect which SIMD features are enabled**
    let simd_sse2    = env::var("CARGO_FEATURE_SIMD_SSE2").is_ok();
    let simd_avx     = env::var("CARGO_FEATURE_SIMD_AVX").is_ok();
    let simd_avx2    = env::var("CARGO_FEATURE_SIMD_AVX2").is_ok();
    let simd_avx512f = env::var("CARGO_FEATURE_SIMD_AVX512F").is_ok();

    // **Ensure that at most one SIMD variant is active** – if more than one is enabled, stop with an error.
    let simd_count = [simd_sse2, simd_avx, simd_avx2, simd_avx512f]
        .iter()
        .filter(|&&enabled| enabled)
        .count();
    if simd_count > 1 {
        panic!(
            "Multiple SIMD features enabled ({:?}). Please activate only one of 'simd_sse2', 'simd_avx', 'simd_avx2', or 'simd_avx512f'.",
            [
                (simd_sse2, "simd_sse2"),
                (simd_avx, "simd_avx"),
                (simd_avx2, "simd_avx2"),
                (simd_avx512f, "simd_avx512f")
            ]
            .iter()
            .filter_map(|&(enabled, name)| if enabled { Some(name) } else { None })
            .collect::<Vec<&str>>()
        );
    }

    // **Compile the selected SIMD variant (if exactly one is enabled)**
    if simd_count == 1 {
        let mut config = base_config.clone();
        if simd_sse2 {
            // Enable SSE2 intrinsics
            if !cfg!(target_env = "msvc") {
                config.flag("-msse2");
            }
            config
                .file("src/c/mshabal_128_sse2.c")
                .file("src/c/shabal_sse2.c")
                .compile("shabal_sse2");
        } else if simd_avx {
            // Enable AVX intrinsics
            if cfg!(target_env = "msvc") {
                config.flag("/arch:AVX");
            } else {
                config.flag("-mavx");
            }
            config
                .file("src/c/mshabal_128_avx.c")
                .file("src/c/shabal_avx.c")
                .compile("shabal_avx");
        } else if simd_avx2 {
            // Enable AVX2 intrinsics
            if cfg!(target_env = "msvc") {
                config.flag("/arch:AVX2");
            } else {
                config.flag("-mavx2");
            }
            config
                .file("src/c/mshabal_256_avx2.c")
                .file("src/c/shabal_avx2.c")
                .compile("shabal_avx2");
        } else if simd_avx512f {
            // Enable AVX-512F intrinsics
            if cfg!(target_env = "msvc") {
                config.flag("/arch:AVX512");
            } else {
                config.flag("-mavx512f");
            }
            config
                .file("src/c/mshabal_512_avx512f.c")
                .file("src/c/shabal_avx512f.c")
                .compile("shabal_avx512f");
        }
    }
    // If the umbrella "simd" feature is enabled *without* any specific subfeature, 
    // no SIMD variant will be compiled (simd_count == 0, so nothing to do here).

    // **Compile Neon variant (independently selectable)**
    if env::var("CARGO_FEATURE_NEON").is_ok() {
        // Only attempt Neon build for ARM targets:
        let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
        if target_arch == "arm" || target_arch == "aarch64" {
            let mut config = base_config.clone();
            // On 32-bit ARM (not AArch64 and not MSVC), use Neon FPU flag
            if target_arch == "arm" && !cfg!(target_env = "msvc") {
                config.flag("-mfpu=neon");
            }
            config
                .file("src/c/mshabal_128_neon.c")
                .file("src/c/shabal_neon.c")
                .compile("shabal_neon");
        } else {
            println!(
                "cargo:warning=Feature 'neon' was enabled for target arch '{}', but Neon is only supported on ARM. Skipping Neon build.",
                target_arch
            );
        }
    }
}
