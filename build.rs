//! Build script: compiles the C Shabal implementations and picks the SIMD
//! backend that matches the *target* architecture.
//!
//! Cargo features cannot depend on the target, so `--features simd_avx2`
//! (the default) would otherwise try to compile AVX2 code on a Raspberry Pi.
//! Instead the script resolves the requested features against the target and
//! tells the Rust side which backend exists via `cfg(simd_backend = "...")`.

use std::env;
use std::process::Command;

const BACKENDS: &[&str] = &["none", "sse2", "avx", "avx2", "avx512f", "neon"];

#[cfg(target_env = "gnu")]
fn compile_windows_icon() {
    let target = env::var("TARGET").unwrap_or_default();
    if target.contains("windows") {
        let out_dir = env::var("OUT_DIR").unwrap();

        let rc_path = "src/windows/app.rc";
        let res_path = format!("{}/icon.res", out_dir);

        let windres_cmd = if Command::new("x86_64-w64-mingw32-windres")
            .arg("--version")
            .status()
            .is_ok()
        {
            "x86_64-w64-mingw32-windres"
        } else if Command::new("i686-w64-mingw32-windres")
            .arg("--version")
            .status()
            .is_ok()
        {
            "i686-w64-mingw32-windres"
        } else if Command::new("windres").arg("--version").status().is_ok() {
            "windres"
        } else {
            println!("cargo:warning=Windres not found, skipping icon resource compilation.");
            return;
        };
        let status = Command::new(windres_cmd)
            .args([
                "--input",
                rc_path,
                "--output",
                &res_path,
                "--output-format=coff",
            ])
            .status()
            .expect("failed to run windres");

        if !status.success() {
            panic!("windres failed with status {}", status);
        }

        println!("cargo:rustc-link-arg-bin=signum-miner={}", res_path);
    }
}

fn feature(name: &str) -> bool {
    env::var(format!("CARGO_FEATURE_{}", name.to_uppercase())).is_ok()
}

/// Decide which backend to build for `target_arch` given the enabled features.
fn select_backend(target_arch: &str, requested_x86: &[&str], neon_requested: bool) -> String {
    match target_arch {
        "x86" | "x86_64" => {
            if neon_requested && requested_x86.is_empty() {
                println!(
                    "cargo:warning=feature 'neon' has no effect on {}; building without SIMD",
                    target_arch
                );
            }
            match requested_x86 {
                [] => "none".to_string(),
                [one] => one.to_string(),
                many => panic!(
                    "Multiple SIMD features enabled ({:?}). Please activate only one of 'simd_sse2', 'simd_avx', 'simd_avx2', or 'simd_avx512f'.",
                    many
                ),
            }
        }
        "aarch64" => {
            // NEON is mandatory on AArch64, so it is always safe to use.  The
            // default feature set requests AVX2; honour the intent (fast
            // hashing) rather than the letter.
            if !neon_requested && !requested_x86.is_empty() {
                println!(
                    "cargo:warning=x86 SIMD feature(s) {:?} requested for aarch64; using NEON instead",
                    requested_x86
                );
            }
            "neon".to_string()
        }
        "arm" => {
            // 32-bit ARM: NEON is optional, only use it when asked for.
            if neon_requested {
                "neon".to_string()
            } else {
                if !requested_x86.is_empty() {
                    println!(
                        "cargo:warning=x86 SIMD feature(s) {:?} requested for arm; building without SIMD (use --features neon on NEON capable boards)",
                        requested_x86
                    );
                }
                "none".to_string()
            }
        }
        other => {
            if neon_requested || !requested_x86.is_empty() {
                println!(
                    "cargo:warning=no SIMD backend available for target arch '{}'; building without SIMD",
                    other
                );
            }
            "none".to_string()
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/c");
    println!("cargo:rerun-if-changed=src/windows/app.rc");
    println!(
        "cargo:rustc-check-cfg=cfg(simd_backend, values({}))",
        BACKENDS
            .iter()
            .map(|b| format!("\"{}\"", b))
            .collect::<Vec<_>>()
            .join(", ")
    );

    if feature("async_io") {
        println!(
            "cargo:warning=feature 'async_io' is deprecated and has no effect; plot reads always use positional blocking I/O on dedicated reader threads"
        );
    }

    #[cfg(target_env = "gnu")]
    compile_windows_icon();

    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let msvc = target_env == "msvc";

    let mut shared_config = cc::Build::new();
    // The hashing loop is the hot path of the whole miner: always optimise it
    // for speed, whatever the Rust profile says.
    shared_config.opt_level(3);
    if msvc {
        shared_config
            .flag("/Oi")
            .flag("/Ot")
            .flag("/Oy")
            .flag("/GT")
            .flag("/GL");
    } else {
        shared_config.flag("-std=c99");
        if env::var("HOST").ok() == env::var("TARGET").ok() {
            shared_config.flag("-mtune=native");
        }
    }

    // Always compile the baseline (non-SIMD) Shabal implementation.
    let mut base_config = shared_config.clone();
    base_config
        .file("src/c/sph_shabal.c")
        .file("src/c/shabal.c")
        .file("src/c/common.c")
        .compile("shabal");

    let mut requested_x86 = Vec::new();
    for (name, flag) in [
        ("simd_sse2", "sse2"),
        ("simd_avx", "avx"),
        ("simd_avx2", "avx2"),
        ("simd_avx512f", "avx512f"),
    ] {
        if feature(name) {
            requested_x86.push(flag);
        }
    }
    let backend = select_backend(&target_arch, &requested_x86, feature("neon"));
    println!("cargo:rustc-cfg=simd_backend=\"{}\"", backend);

    let mut config = base_config.clone();
    match backend.as_str() {
        "sse2" => {
            if !msvc {
                config.flag("-msse2");
            }
            config
                .file("src/c/mshabal_128_sse2.c")
                .file("src/c/shabal_sse2.c")
                .compile("shabal_sse2");
        }
        "avx" => {
            if msvc {
                config.flag("/arch:AVX");
            } else {
                config.flag("-mavx");
            }
            config
                .file("src/c/mshabal_128_avx.c")
                .file("src/c/shabal_avx.c")
                .compile("shabal_avx");
        }
        "avx2" => {
            if msvc {
                config.flag("/arch:AVX2");
            } else {
                config.flag("-mavx2");
            }
            config
                .file("src/c/mshabal_256_avx2.c")
                .file("src/c/shabal_avx2.c")
                .compile("shabal_avx2");
        }
        "avx512f" => {
            if msvc {
                config.flag("/arch:AVX512");
            } else {
                config.flag("-mavx512f");
            }
            config
                .file("src/c/mshabal_512_avx512f.c")
                .file("src/c/shabal_avx512f.c")
                .compile("shabal_avx512f");
        }
        "neon" => {
            if target_arch == "arm" && !msvc {
                config.flag("-mfpu=neon");
            }
            config
                .file("src/c/mshabal_128_neon.c")
                .file("src/c/shabal_neon.c")
                .compile("shabal_neon");
        }
        _ => {}
    }
}
