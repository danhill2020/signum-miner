# Changelog

## 2.1.0

### Fixed

- **Wrong nonces were submitted for every scoop other than 0.** A change in
  May 2025 added `scoop * nonces` to the start nonce of each chunk, so the
  nonce sent to the pool was off by that amount and pools recomputed a
  different deadline. The mapping is now covered by a test that generates
  PoC2 plots independently and compares byte for byte with the real plot in
  `test_data`.
- **Direct I/O read from unaligned offsets** on plots whose nonce count is not
  a multiple of 64, failing with `EINVAL` and silently skipping the plot every
  round. Offsets are now aligned up and the skipped nonces are accounted for.
- **Rounds could never finish** when a plot failed to open (no "drive
  finished" signal), when the number of drives changed after a rescan (the
  expected count was captured once at start-up), or when the small last chunk
  of a drive was hashed before a larger earlier one. In `submit_only_best`
  mode (the default) a round that never finishes never submits anything.
  Every drive now always reports completion and the miner counts hashed
  chunks, so a round completes exactly when every chunk has been hashed.
- **Deadlock between the capacity rescan and new blocks**: the two tasks took
  the reader and state locks in opposite orders. Locks are no longer nested.
- `cargo build --release` failed on Raspberry Pi / aarch64 because the default
  `simd_avx2` feature tried to compile AVX2 code. The build script now selects
  the backend from the target architecture (NEON on aarch64).
- The release profile optimised for *size* (`opt-level = "z"`), which also
  compiled the Shabal hashing C code with `-Oz`. Hashing is now built with
  `-O3`.
- Read buffers were plain `Vec<u8>` with no alignment guarantee, which direct
  I/O requires; they are now explicitly 4 KiB aligned.
- `hdd_wakeup_after` only performed a seek, which never touches the disk and
  cannot keep an external enclosure awake. It now reads one block.
- An invalid generation signature or `additional_headers` value from the
  configuration or pool could panic the miner; both are handled.
- A minimal config (without `cpu_worker_task_count`) exited with "no active
  workers" and status 0. The default is now 4 and configuration errors exit
  with status 1.
- The carriage return used for the progress bar was also written to log files.
- The `opencl` feature did not compile.

### Added (capacity stability)

- `plot_missing_grace` (default 1 h): plots that a rescan cannot find keep
  their place in the capacity and are retried every round until the grace
  period expires. This replaces the previous "debounce" heuristics and stops
  the reported capacity from jumping around on USB resets or late mounts.
- When a new block interrupts an unfinished round, the best deadline found
  so far is submitted instead of being discarded.
- `submit_only_best` keeps the best deadline per account, so a farm plotted
  under several accounts submits for each of them every round (previously
  only one account's best was sent).
- A warning when a round takes longer than 180 s, with pointers to the
  built-in benchmark modes for finding the bottleneck.
- Buffered reads (the fallback for filesystems without direct I/O) advise
  the kernel to read ahead and drop the pages afterwards.

### Changed

- Direct I/O is no longer disabled wholesale for USB drives; instead every
  plot falls back to buffered reads if the filesystem refuses `O_DIRECT`.
- The progress bar is only drawn on an interactive terminal.
- Plot directories that are missing, empty or return errors are rescanned
  every minute instead of waiting for `capacity_check_interval`; the reader
  pool grows when drives are added.
- Device ids and bus types come from `st_dev` and sysfs instead of spawning
  `stat`, `df` and `lsblk` for every plot file.
- Unknown configuration keys are reported; URLs without a host, zero workers
  and unaligned buffer sizes are rejected with a clear message.
- The metrics report now includes submission results and per-drive read
  errors.
- `Ctrl-C` / `SIGINT` logs a shutdown message and exits cleanly.
- New `--check` flag validates the configuration and plot directories.
- `Cargo.lock` is committed for reproducible builds.
- The `async_io` feature is deprecated and has no effect.
- Live-network tests are `#[ignore]`d; CI covers AVX2, SSE2, the portable
  backend, the `opencl` feature check and the aarch64 NEON build under QEMU.

### Added

- `contrib/signum-miner.service` systemd unit and a Raspberry Pi 5 section in
  the README.
