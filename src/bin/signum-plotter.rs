//! `signum-plotter`: create PoC2 plot files on the CPU or (with the `opencl`
//! feature) on a GPU.
//!
//! ```text
//! signum-plotter --id 10282355196851764065 --path /mnt/bay1/plots --size-gb 4000
//! signum-plotter --list-devices
//! signum-plotter --id ... --path ... --nonces 1000000 --gpu 0:0
//! ```

#[macro_use]
extern crate cfg_if;

#[path = "../plotter/mod.rs"]
mod plotter;

use clap::{Arg, ArgAction, Command};
use plotter::poc2::NONCE_SIZE;
use plotter::{
    chunk_for_memory, default_chunk_nonces, format_duration, free_space, next_free_nonce,
    set_low_priority, CpuGenerator, NonceGenerator, PlotJob, Progress, NONCES_PER_BLOCK,
};
use std::io::Write;
use std::path::PathBuf;
use std::process;
use std::time::Instant;

fn fail(msg: &str) -> ! {
    eprintln!("error: {}", msg);
    process::exit(1);
}

fn parse_u64(matches: &clap::ArgMatches, name: &str) -> Option<u64> {
    matches.get_one::<String>(name).map(|v| {
        v.replace(['_', ','], "")
            .parse::<u64>()
            .unwrap_or_else(|_| fail(&format!("--{} must be a whole number, got '{}'", name, v)))
    })
}

fn main() {
    let cmd = Command::new("signum-plotter")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Creates PoC2 plot files for Signum mining (CPU, or GPU with the opencl feature)")
        .arg(
            Arg::new("id")
                .short('i')
                .long("id")
                .value_name("ACCOUNT_ID")
                .help("Numeric account id the plot belongs to"),
        )
        .arg(
            Arg::new("path")
                .short('p')
                .long("path")
                .value_name("DIR")
                .help("Directory to write the plot into"),
        )
        .arg(
            Arg::new("start")
                .short('s')
                .long("start-nonce")
                .value_name("NONCE")
                .help(
                    "First nonce (default: continue after existing plots of this account in DIR)",
                ),
        )
        .arg(
            Arg::new("nonces")
                .short('n')
                .long("nonces")
                .value_name("COUNT")
                .help("Number of nonces to plot (256 KiB each)"),
        )
        .arg(
            Arg::new("size")
                .long("size-gb")
                .value_name("GB")
                .help("Plot size in GB (1 GB = 4096 nonces); alternative to --nonces"),
        )
        .arg(
            Arg::new("fill")
                .long("fill")
                .action(ArgAction::SetTrue)
                .help("Plot size = all free space in DIR (minus a small reserve); alternative to --nonces"),
        )
        .arg(
            Arg::new("exact")
                .long("exact")
                .action(ArgAction::SetTrue)
                .help("Keep the nonce count as given instead of rounding it down to a multiple of 64 (which keeps every scoop 4 KiB aligned for direct I/O)"),
        )
        .arg(
            Arg::new("no_direct_io")
                .long("no-direct-io")
                .action(ArgAction::SetTrue)
                .help("Write through the page cache instead of with direct I/O"),
        )
        .arg(
            Arg::new("low_priority")
                .long("low-priority")
                .short('l')
                .action(ArgAction::SetTrue)
                .help("Run with low CPU priority (nice), e.g. while the miner runs on the same machine"),
        )
        .arg(
            Arg::new("threads")
                .short('t')
                .long("threads")
                .value_name("N")
                .help("CPU threads for CPU plotting (default: all cores)"),
        )
        .arg(
            Arg::new("gpu")
                .short('g')
                .long("gpu")
                .value_name("PLATFORM:DEVICE")
                .help("Plot on this OpenCL device (see --list-devices)"),
        )
        .arg(
            Arg::new("batch")
                .short('b')
                .long("batch")
                .value_name("NONCES")
                .help("GPU only: nonces per kernel launch (default: sized from the device memory)"),
        )
        .arg(
            Arg::new("mem")
                .short('m')
                .long("mem")
                .value_name("GB")
                .help("RAM for the two write buffers (default: a quarter of the machine's RAM, at most 4 GB); bigger means larger, faster disk writes"),
        )
        .arg(
            Arg::new("bench")
                .long("bench")
                .action(ArgAction::SetTrue)
                .help("Generate only, write nothing, report the rate"),
        )
        .arg(
            Arg::new("list")
                .long("list-devices")
                .action(ArgAction::SetTrue)
                .help("List OpenCL platforms and devices and exit"),
        )
        .arg(
            Arg::new("quiet")
                .short('q')
                .long("quiet")
                .action(ArgAction::SetTrue)
                .help("Only print the final result"),
        );
    let m = cmd.get_matches();

    if m.get_flag("list") {
        cfg_if! {
            if #[cfg(feature = "opencl")] {
                match plotter::gpu::list_devices() {
                    Ok(devices) if devices.is_empty() => println!("No OpenCL devices found."),
                    Ok(devices) => {
                        for d in devices {
                            println!("{}", d);
                        }
                    }
                    Err(e) => fail(&format!("OpenCL is not available: {}", e)),
                }
            } else {
                println!("This build has no OpenCL support; rebuild with `cargo build --release --features opencl` to list and use GPUs.");
            }
        }
        return;
    }

    let account_id = parse_u64(&m, "id").unwrap_or_else(|| fail("--id is required"));
    let directory = m
        .get_one::<String>("path")
        .map(PathBuf::from)
        .unwrap_or_else(|| fail("--path is required"));
    if !m.get_flag("bench") && !directory.is_dir() {
        fail(&format!("{} is not a directory", directory.display()));
    }
    if m.get_flag("low_priority") && !set_low_priority() {
        eprintln!("warning: could not lower the process priority");
    }
    let mut nonces = match (
        parse_u64(&m, "nonces"),
        parse_u64(&m, "size"),
        m.get_flag("fill"),
    ) {
        (Some(n), _, _) => n,
        (None, Some(gb), _) => gb * 4096,
        (None, None, true) => {
            let free = free_space(&directory)
                .unwrap_or_else(|| fail("cannot determine the free space of the directory"));
            // Leave 64 MiB so the filesystem keeps some slack.
            free.saturating_sub(64 << 20) / NONCE_SIZE as u64
        }
        (None, None, false) => fail("give the plot size with --nonces, --size-gb or --fill"),
    };
    if !m.get_flag("exact") && !m.get_flag("bench") {
        let rounded = nonces / NONCES_PER_BLOCK * NONCES_PER_BLOCK;
        if rounded != nonces {
            eprintln!(
                "rounding {} nonces down to {} (a multiple of {} keeps every scoop region 4 KiB aligned; --exact to keep it)",
                nonces, rounded, NONCES_PER_BLOCK
            );
        }
        nonces = rounded;
    }
    if nonces == 0 {
        fail("the plot needs at least one nonce (64 without --exact)");
    }
    let start_nonce =
        parse_u64(&m, "start").unwrap_or_else(|| next_free_nonce(&directory, account_id));
    if start_nonce.checked_add(nonces).is_none() || nonces.checked_mul(NONCE_SIZE as u64).is_none()
    {
        fail("start nonce + nonces overflows");
    }

    if let Some(threads) = parse_u64(&m, "threads") {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads.max(1) as usize)
            .build_global()
            .unwrap_or_else(|e| fail(&format!("cannot set up {} threads: {}", threads, e)));
    }

    let gpu = m.get_one::<String>("gpu").cloned();
    let mut generator: Box<dyn NonceGenerator> = match gpu {
        None => {
            if let Err(e) = plotter::cpu::check_cpu_support() {
                fail(&e);
            }
            Box::new(CpuGenerator::new())
        }
        Some(spec) => {
            cfg_if! {
                if #[cfg(feature = "opencl")] {
                    let (platform, device) = parse_device(&spec);
                    let batch = parse_u64(&m, "batch").map(|b| b as usize);
                    match plotter::gpu::GpuGenerator::new(platform, device, batch) {
                        Ok(g) => Box::new(g),
                        Err(e) => fail(&format!("cannot use GPU {}: {}", spec, e)),
                    }
                } else {
                    let _ = spec;
                    fail("this build has no OpenCL support; rebuild with `cargo build --release --features opencl`")
                }
            }
        }
    };
    let chunk = match m.get_one::<String>("mem") {
        Some(gb) => {
            let gb: f64 = gb
                .parse()
                .unwrap_or_else(|_| fail(&format!("--mem must be a number of GB, got '{}'", gb)));
            if gb.is_nan() || gb <= 0.0 {
                fail("--mem must be positive");
            }
            chunk_for_memory((gb * 1e9) as u64)
        }
        None => default_chunk_nonces(),
    };

    let quiet = m.get_flag("quiet");
    let job = PlotJob {
        account_id,
        start_nonce,
        nonces,
        directory,
        chunk,
        dry_run: m.get_flag("bench"),
        direct_io: !m.get_flag("no_direct_io"),
    };
    if !job.dry_run && !job.path().exists() {
        let free = free_space(&job.directory);
        let needed = nonces * NONCE_SIZE as u64;
        if let Some(free) = free {
            if free < needed {
                fail(&format!(
                    "not enough free space in {}: need {:.1} GB, have {:.1} GB",
                    job.directory.display(),
                    needed as f64 / 1e9,
                    free as f64 / 1e9
                ));
            }
        }
    }

    if !quiet {
        eprintln!(
        "signum-plotter v{}: {} nonces ({:.2} GB) for account {} starting at nonce {} using {}{}",
        env!("CARGO_PKG_VERSION"),
        nonces,
        nonces as f64 * NONCE_SIZE as f64 / 1e9,
        account_id,
        start_nonce,
        generator.name(),
        if job.dry_run {
            " (benchmark, nothing is written)"
        } else {
            ""
        }
    );
    }
    if !quiet {
        eprintln!(
            "chunks of {} nonces ({:.2} GB of RAM for the two buffers, {} KiB per scoop write)",
            job.chunk_nonces(),
            job.buffer_bytes() as f64 / 1e9,
            job.chunk_nonces() * 64 / 1024
        );
        if !job.dry_run {
            eprintln!("writing {}", job.path().display());
        }
    }

    let started = Instant::now();
    let mut last_report = Instant::now();
    let report = |p: Progress, force: bool, last: &mut Instant| {
        if quiet || (!force && last.elapsed().as_secs_f64() < 2.0) {
            return;
        }
        *last = Instant::now();
        let eta = p.eta().map(format_duration).unwrap_or_else(|| "-".into());
        eprintln!(
            "progress: {:.1}% ({}/{} nonces), {:.0} nonces/min ({:.1} MB/s), elapsed {}, eta {}",
            p.percent(),
            p.done,
            p.total,
            p.nonces_per_minute,
            p.nonces_per_minute * NONCE_SIZE as f64 / 60.0 / 1e6,
            format_duration(p.elapsed),
            eta
        );
        let _ = std::io::stderr().flush();
    };

    let mut announced = false;
    let result = plotter::run(&job, generator.as_mut(), |p| {
        if !announced {
            announced = true;
            if p.done > 0 && !quiet && !job.dry_run {
                // First callback of a resumed plot reports the resume point
                // plus the first batch; say where we picked up.
                eprintln!(
                    "resuming: {} of {} nonces were already on disk",
                    p.done, p.total
                );
            }
        }
        report(p, p.done == p.total, &mut last_report);
    });
    match result {
        Ok(outcome) => {
            if outcome.resumed_from >= nonces && !job.dry_run {
                println!(
                    "done: {} is already complete ({} nonces), nothing to do",
                    outcome.path.display(),
                    nonces
                );
                return;
            }
            let made = nonces - outcome.resumed_from;
            let secs = started.elapsed().as_secs_f64().max(1e-9);
            println!(
                "done: {} nonces in {} ({:.0} nonces/min, {:.1} MB/s{}){}",
                made,
                format_duration(started.elapsed()),
                made as f64 / secs * 60.0,
                made as f64 * NONCE_SIZE as f64 / secs / 1e6,
                if job.dry_run {
                    ""
                } else if outcome.direct_io {
                    ", direct I/O"
                } else {
                    ", buffered I/O"
                },
                if job.dry_run {
                    String::new()
                } else {
                    format!(" -> {}", outcome.path.display())
                }
            );
        }
        Err(e) => {
            if !job.dry_run {
                match plotter::writer::read_resume_marker(&job.path()) {
                    // Nothing written yet: no point keeping the file.
                    Ok(Some(0)) => {
                        let _ = std::fs::remove_file(job.path());
                    }
                    Ok(Some(done)) => eprintln!(
                        "{} nonces are on disk; run the same command again to resume",
                        done
                    ),
                    _ => {}
                }
            }
            fail(&format!("plotting failed: {}", e));
        }
    }
}

#[cfg(feature = "opencl")]
fn parse_device(spec: &str) -> (usize, usize) {
    let mut parts = spec.split(':');
    let platform = parts.next().and_then(|p| p.parse().ok());
    let device = parts.next().and_then(|d| d.parse().ok());
    match (platform, device, parts.next()) {
        (Some(p), Some(d), None) => (p, d),
        (Some(p), None, None) => (p, 0),
        _ => fail(&format!(
            "--gpu expects PLATFORM:DEVICE (e.g. 0:0), got '{}'",
            spec
        )),
    }
}
