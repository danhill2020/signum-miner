# Signum Miner

[![Build Status](https://github.com/signum-network/signum-miner/actions/workflows/release.yml/badge.svg)](https://github.com/signum-network/signum-miner/actions)
[![License: GPLv3](https://img.shields.io/badge/License-GPLv3-blue.svg)](./LICENSE)
[![Telegram](https://img.shields.io/badge/chat-telegram-blue.svg)](https://t.me/signumnetwork)
[![Get Support](https://img.shields.io/badge/join-discord-blue.svg)](https://discord.gg/9rH2bbCNpe) 
</br>
[![Website](https://img.shields.io/badge/Website-signum.network-green?logo=Firefox&logoColor=white)](https://signum.network)
[![Docs](https://img.shields.io/badge/Docs-Mining%20Guide-blue?logo=Book&logoColor=white)](https://docs.signum.network/signum/starting-mining-signa)

---

## ⚡ **Why Signum Miner?**

Signum is the world's first truly sustainable blockchain — and the Signum Miner is your gateway to mining it efficiently and eco-consciously. Whether you're using modern AVX512-capable CPUs, an ARM board, or legacy hardware, the Signum Miner is optimized to deliver fast results with minimal energy.

> Join the green revolution of decentralized computing — mine with purpose. Mine with Signum.

---

## 🔧 **Features**

- Ultra-low energy-power mining algorithm (**Proof of Commitment (PoC+)**)
- AVX512, AVX2, AVX, SSE2 and NEON **SIMD optimizations**
- Multi-threaded & high performance I/O
- Integrated plot reader, CPU miner & buffer pipeline
- Full async + **Tokio** + **crossbeam** architecture

## 🧰 **Feature Overview**

| Architecture     | Feature(s)               | Description                            |
|------------------|--------------------------|----------------------------------------|
| x86 (Intel/AMD)  | sse2, avx, avx2, avx512f | SIMD extensions for Desktop/Server CPUs |
| ARM (e.g. Pi)    | neon                    | SIMD for ARM CPUs                      |




| Variant  | Description                                              | CPU Availability                            |
|----------|----------------------------------------------------------|---------------------------------------------|
| sse2     | Older, widely supported SIMD extension                   | Almost all x86 CPUs since 2001              |
| avx      | Advanced Vector Extensions – larger 256-bit registers    | Intel: Sandy Bridge (2011), AMD: Bulldozer  |
| avx2     | Improved AVX version with integer support                | Intel: Haswell (2013), AMD: Excavator       |
| avx512f  | Even wider 512-bit SIMD registers – very powerful        | Intel: Skylake-X (rare in consumer CPUs)    |
| neon     | SIMD extension for ARM architecture                      | ARMv7 (32-bit) and ARMv8 (64-bit, e.g. Raspberry Pi 4) |

---



## 📦 **Installation**

### 🖥️ Precompiled Binaries

➡️ [Go to Releases](https://github.com/signum-network/signum-miner/releases)

Download the binary matching your system:

For Linux distributions:
- `signum-miner-avx`
- `signum-miner-avx2`
- `signum-miner-avx512f` (modern CPUs)
- `signum-miner-sse2` (legacy CPUs)
- `signum-miner-aarch64-neon` (ARM 64-bit: Raspberry Pi 4/5, most SBCs)

For Windows distributions one of the `.exe` versions:
- `signum-miner-avx`
- `signum-miner-avx2`
- `signum-miner-avx512f` (modern CPUs)
- `signum-miner-sse2` (legacy CPUs)

The miner checks at start-up that the CPU supports the SIMD instructions it
was built with and refuses to run otherwise, instead of crashing with an
"Illegal instruction" halfway through the first round.

## ⚙️ **Running the binaries**

### Config

The miner needs a **config.yaml** file; start from the one in this repository
or in the release. A minimal config is just:

```yaml
plot_dirs:
  - '/mnt/bay1/plots'
  - '/mnt/bay2/plots'
url: 'https://pool.example.com:8080'
```

Every other setting has a working default and is documented in
[`config.yaml`](./config.yaml). Unknown keys are reported as warnings at
start-up so typos do not silently fall back to defaults. Run
`./signum-miner --check` to validate the configuration and scan the plot
directories without starting to mine.

Settings worth knowing:

- `hdd_use_direct_io` (default `true`): reads bypass the page cache, which keeps
  memory free and CPU usage low. Plots on filesystems that do not support it
  (for example NTFS via `ntfs-3g`) automatically fall back to buffered reads;
  you will see a warning per plot file.
- `io_buffer_size` (default 16 MiB): bytes per read request. 8–32 MiB works well
  for USB and SATA drives; it must be a multiple of 4096.
- `hdd_wakeup_after` (default 240 s): when no new block has arrived for this
  long the miner reads one block from every drive so external enclosures do not
  spin down. Set to `0` to disable.
- `capacity_check_interval` (default 6 h): how often plot directories are
  rescanned. While a directory is missing, empty or returned errors the miner
  rescans every minute instead, so a bay that is mounted late or re-plugged is
  picked up quickly without a restart.
- `submit_only_best` (default `true`): submit the best deadline once the round
  is complete rather than every improvement as it is found.
- `show_progress`: the progress bar is only drawn on an interactive terminal; under
  systemd or with output redirected to a file it is switched off automatically.

### Running

Be sure to have the config file in the same folder as your binary, or pass
`--config /path/to/config.yaml`.

For Windows, double click on the executable file. If it refuses to run, start
the executable from a command prompt to check for error messages.

For Linux:
```shell
./signum-miner
```

The miner exits with a non-zero status on configuration errors and aborts on
internal errors, so a supervisor such as systemd can restart it.

### 🍓 Raspberry Pi 5 with external drive bays

This is a first-class target. A plain `cargo build --release` on the Pi picks
the NEON hashing backend automatically; no feature flags are needed.

```bash
sudo apt-get install -y build-essential git curl
curl https://sh.rustup.rs -sSf | sh -s -- -y && source "$HOME/.cargo/env"
git clone https://github.com/signum-network/signum-miner
cd signum-miner
cargo build --release
./target/release/signum-miner --config config.yaml --check
```

Recommended settings for a Pi 5 (4 cores) feeding several USB 3 bays:

```yaml
plot_dirs:
  - '/mnt/bay1/plots'
  - '/mnt/bay2/plots'
  - '/mnt/bay3/plots'
  - '/mnt/bay4/plots'
url: 'https://pool.example.com:8080'
cpu_threads: 0              # all 4 cores hash
cpu_worker_task_count: 4
io_buffer_size: 16777216    # 16 MiB per read keeps USB queues full
hdd_use_direct_io: true     # no page-cache churn on 4-8 GB of RAM
hdd_wakeup_after: 240       # keep the bays spinning between blocks
show_progress: false        # headless
logfile_log_level: off      # journald captures stdout
```

Notes:

- One reader thread is created per physical drive (the miner groups plots by
  the device they live on), so each bay streams independently.
- Format drives as ext4 or exFAT. NTFS via `ntfs-3g` works but only with
  buffered reads, which the miner falls back to automatically.
- Mount the bays through `/etc/fstab` with stable identifiers (`UUID=` or
  `LABEL=`) so `/mnt/bayN` always points at the same disk. The included
  [`contrib/signum-miner.service`](./contrib/signum-miner.service) shows how to
  make systemd wait for those mounts and restart the miner if it ever stops.
- Many enclosures have their own idle spin-down timer; `hdd_wakeup_after`
  issues a real read (not just a seek) so they stay awake. If an enclosure still
  sleeps, lower the value or disable its timer with `hdparm -B 255` /
  `sdparm --clear STANDBY`.
- Run `./signum-miner --check` after plugging a new bay: it lists every plot
  directory with the bus it is attached to (USB, SATA, NVMe) and the capacity.

### Running as a service (systemd)

```bash
sudo mkdir -p /opt/signum-miner
sudo cp target/release/signum-miner config.yaml /opt/signum-miner/
sudo useradd --system --home /opt/signum-miner --shell /usr/sbin/nologin signum
sudo chown -R signum:signum /opt/signum-miner
sudo cp contrib/signum-miner.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now signum-miner
journalctl -u signum-miner -f
```

Every five minutes the miner logs a metrics summary (rounds, round time,
submissions, data read, network errors) and a per-drive health report
(read errors and consecutive failures), which is what to watch on a headless
box.

### GUI helper

For an optional graphical interface, run `miner_ui.py` with Python 3. The UI
uses the [CustomTkinter](https://github.com/TomSchimansky/CustomTkinter)
framework to provide a modern dark theme with blue accents. Install the required
packages first:
```bash
pip install -r requirements.txt
```
You can then edit `config.yaml`, start or stop the miner and monitor its log
output in a more user friendly window.

```bash
python3 miner_ui.py
```

The CustomTkinter based UI works on Linux, macOS and Windows as long as Python 3
is available and the `signum-miner` binary is in the same directory.
Make sure the compiled miner binary is named **exactly** `signum-miner`,
otherwise the UI will fail to launch it.

---

## 💻 Build from Source
 - First you need to install a Rust stable toolchain (1.80 or newer), check https://www.rust-lang.org/tools/install.
 - A C compiler is needed for the Shabal hashing code (`build-essential` on Debian/Ubuntu/Raspberry Pi OS).
 - Binaries are in **target/debug** or **target/release** depending on optimization.

```bash
# Install Rust
curl https://sh.rustup.rs -sSf | sh

# Clone the repository
git clone https://github.com/signum-network/signum-miner
cd signum-miner
# Or run the automated setup which installs requirements and builds
python3 setup.py

# Default build: AVX2 on x86-64, NEON on aarch64 (Raspberry Pi), portable
# Rust elsewhere. The build script picks the backend that matches the target.
cargo build --release

# Pick a different x86 backend (choose exactly one)
cargo build --release --no-default-features --features simd_sse2
cargo build --release --no-default-features --features simd_avx
cargo build --release --no-default-features --features simd_avx512f

# Portable build without SIMD (slow, for unusual CPUs)
cargo build --release --no-default-features

# OpenCL GPU support
cargo build --release --features opencl

# Cross-compile for a Raspberry Pi from an x86-64 Linux box
rustup target add aarch64-unknown-linux-gnu
sudo apt-get install -y gcc-aarch64-linux-gnu
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
  cargo build --release --target aarch64-unknown-linux-gnu
```

The `async_io` feature of earlier versions is deprecated and has no effect;
plot reads always use positional blocking I/O on dedicated reader threads.

### Tests

```bash
cargo test --release
```

The test suite includes an independent PoC2 plot generator and deadline
oracle (built on the reference C Shabal implementation) that verifies the
miner maps every byte on disk to the right nonce and that the compiled SIMD
backend agrees with the reference on real plot data. CI runs it for AVX2,
SSE2, the portable backend and, under QEMU, for the aarch64 NEON build.

## 📜 License

This project is licensed under the GPLv3 — see the LICENSE file for details.</br>
Made with ❤️ for the future of decentralized sustainability — Signum.

### Forked from
This is a code fork from https://github.com/PoC-Consortium/scavenger