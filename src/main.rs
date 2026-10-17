#![warn(unused_extern_crates)]

#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate cfg_if;
#[macro_use]
extern crate log;

mod buffer;
mod com;
mod config;
mod cpu_worker;
mod future;
mod logger;
mod metrics;
mod miner;
mod plot;
mod poc_hashing;
mod reader;
mod requests;
mod shabal256;
#[cfg(test)]
mod test_support;
mod utils;

#[cfg(feature = "opencl")]
mod gpu_worker;
#[cfg(feature = "opencl")]
mod gpu_worker_async;
#[cfg(feature = "opencl")]
mod ocl;

use crate::config::load_cfg;
use crate::miner::Miner;
use clap::{Arg, Command};
use std::process;

/// Refuse to run a binary whose SIMD backend this CPU cannot execute.  The
/// alternative is an "Illegal instruction" crash in the middle of the first
/// round, which is much harder to diagnose.
fn check_cpu_support() -> Result<(), String> {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        let required: Option<(&str, bool)> = {
            cfg_if! {
                if #[cfg(simd_backend = "avx512f")] {
                    Some(("avx512f", std::is_x86_feature_detected!("avx512f")))
                } else if #[cfg(simd_backend = "avx2")] {
                    Some(("avx2", std::is_x86_feature_detected!("avx2")))
                } else if #[cfg(simd_backend = "avx")] {
                    Some(("avx", std::is_x86_feature_detected!("avx")))
                } else if #[cfg(simd_backend = "sse2")] {
                    Some(("sse2", std::is_x86_feature_detected!("sse2")))
                } else {
                    None
                }
            }
        };
        if let Some((name, supported)) = required {
            if !supported {
                return Err(format!(
                    "this build uses {} instructions which this CPU does not support; \
                     download or build a variant for your CPU (supported here: {})",
                    name.to_uppercase(),
                    supported_x86_features().join(", ")
                ));
            }
        }
    }
    Ok(())
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn supported_x86_features() -> Vec<&'static str> {
    let mut v = Vec::new();
    if std::is_x86_feature_detected!("avx512f") {
        v.push("avx512f");
    }
    if std::is_x86_feature_detected!("avx2") {
        v.push("avx2");
    }
    if std::is_x86_feature_detected!("avx") {
        v.push("avx");
    }
    if std::is_x86_feature_detected!("sse2") {
        v.push("sse2");
    }
    v
}

fn fail(msg: &str) -> ! {
    eprintln!("error: {}", msg);
    process::exit(1);
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let cmd = Command::new(env!("CARGO_PKG_NAME"))
        .version(env!("CARGO_PKG_VERSION"))
        .author(env!("CARGO_PKG_AUTHORS"))
        .about(env!("CARGO_PKG_DESCRIPTION"))
        .arg(
            Arg::new("config")
                .short('c')
                .long("config")
                .value_name("FILE")
                .help("Location of the config file")
                .default_value("config.yaml")
                .required(false),
        )
        .arg(
            Arg::new("check")
                .long("check")
                .help("Validate the configuration and plot directories, then exit")
                .action(clap::ArgAction::SetTrue),
        );

    #[cfg(feature = "opencl")]
    let cmd = cmd.arg(
        Arg::new("opencl")
            .short('o')
            .long("opencl")
            .help("Display OpenCL platforms and devices")
            .action(clap::ArgAction::SetTrue),
    );

    let matches = cmd.get_matches();
    let config = matches
        .get_one::<String>("config")
        .map(|s| s.as_str())
        .unwrap_or("config.yaml");

    if let Err(e) = check_cpu_support() {
        fail(&e);
    }

    let cfg_loaded = match load_cfg(config) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Configuration error: {}", e);
            eprintln!(
                "\nPlease create a valid config.yaml file or specify a different config file with --config"
            );
            process::exit(1);
        }
    };
    if let Err(e) = logger::init_logger(&cfg_loaded) {
        fail(&e);
    }

    info!("{} v{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
    info!(
        "hashing backend: {}, {} logical CPU(s)",
        cpu_worker::backend_name(),
        num_cpus::get()
    );
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    info!("CPU SIMD support: {}", supported_x86_features().join(", "));

    #[cfg(feature = "opencl")]
    info!("GPU extensions: OpenCL");

    #[cfg(feature = "opencl")]
    if matches.get_flag("opencl") {
        ocl::platform_info();
        process::exit(0);
    }

    cpu_worker::init_backend();

    #[cfg(feature = "opencl")]
    ocl::gpu_info(&cfg_loaded);

    let handle = tokio::runtime::Handle::current();
    let miner = match Miner::new(cfg_loaded, handle) {
        Ok(miner) => miner,
        Err(e) => {
            error!("{}", e);
            process::exit(1);
        }
    };

    if matches.get_flag("check") {
        info!("configuration check passed");
        process::exit(0);
    }

    miner.run().await;
}
