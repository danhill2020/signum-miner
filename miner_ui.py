#!/usr/bin/env python3
"""Signum Miner desktop UI.

A small control panel around the ``signum-miner`` binary:

* **Dashboard** – live status parsed from the miner's log output (block,
  scoop, round time, read speed, capacity, accepted deadlines, pool state,
  unreadable plots) plus the periodic metrics and disk-health reports.
* **Config** – a typed editor for ``config.yaml`` that keeps the comments and
  order of the file intact, with a *Validate* button that runs
  ``signum-miner --check``.
* **Logs** – the raw miner output with level colouring and a level filter.

The pure-logic parts (``ConfigDocument``, ``LogStats``, ``find_binary`` ...)
have no GUI dependency so they can be unit tested; see ``tests/ui``.
"""

from __future__ import annotations

import argparse
import os
import queue
import re
import shutil
import subprocess
import sys
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Optional

try:  # the GUI toolkit is optional for importing the logic in tests
    import tkinter as tk
    from tkinter import filedialog, messagebox
    import customtkinter as ctk
except ImportError:  # pragma: no cover - exercised only without Tk
    tk = None  # type: ignore[assignment]
    ctk = None  # type: ignore[assignment]

try:
    from PIL import Image
except ImportError:  # pragma: no cover
    Image = None  # type: ignore[assignment]

import yaml

APP_NAME = "Signum Miner"
HERE = Path(__file__).resolve().parent
LOGO_PATH = HERE / "src" / "windows" / "miner_logo.ico"

# ---------------------------------------------------------------------------
# Signum palette.  The green is the colour of the Signum "S" in the bundled
# logo (rgb 2,160,58); the dark surfaces are tinted towards it so the accent
# sits naturally on them.
# ---------------------------------------------------------------------------
SIGNUM_GREEN = "#02A03A"
SIGNUM_GREEN_HOVER = "#13B84F"
SIGNUM_GREEN_DARK = "#067A30"
BG = "#0D1512"
SURFACE = "#15211B"
SURFACE_ALT = "#1C2B23"
BORDER = "#27392F"
TEXT = "#E9F3EC"
TEXT_MUTED = "#93AD9C"
WARN = "#E3B341"
ERROR = "#F06A5A"
INFO_BLUE = "#5BA7E8"


def apply_signum_theme() -> None:
    """Recolour CustomTkinter's built-in green theme to the Signum palette."""
    ctk.set_appearance_mode("dark")
    ctk.set_default_color_theme("green")
    t = ctk.ThemeManager.theme
    t["CTk"]["fg_color"] = [BG, BG]
    t["CTkToplevel"]["fg_color"] = [BG, BG]
    t["CTkFrame"]["fg_color"] = [SURFACE, SURFACE]
    t["CTkFrame"]["top_fg_color"] = [SURFACE_ALT, SURFACE_ALT]
    t["CTkFrame"]["border_color"] = [BORDER, BORDER]
    t["CTkButton"]["fg_color"] = [SIGNUM_GREEN, SIGNUM_GREEN]
    t["CTkButton"]["hover_color"] = [SIGNUM_GREEN_HOVER, SIGNUM_GREEN_HOVER]
    t["CTkButton"]["text_color"] = ["#06130B", "#06130B"]
    t["CTkButton"]["text_color_disabled"] = [TEXT_MUTED, TEXT_MUTED]
    t["CTkLabel"]["text_color"] = [TEXT, TEXT]
    t["CTkEntry"]["fg_color"] = [BG, BG]
    t["CTkEntry"]["border_color"] = [BORDER, BORDER]
    t["CTkEntry"]["text_color"] = [TEXT, TEXT]
    t["CTkEntry"]["placeholder_text_color"] = [TEXT_MUTED, TEXT_MUTED]
    t["CTkTextbox"]["fg_color"] = [BG, BG]
    t["CTkTextbox"]["border_color"] = [BORDER, BORDER]
    t["CTkTextbox"]["text_color"] = [TEXT, TEXT]
    t["CTkTextbox"]["scrollbar_button_color"] = [BORDER, BORDER]
    t["CTkTextbox"]["scrollbar_button_hover_color"] = [SIGNUM_GREEN_DARK, SIGNUM_GREEN_DARK]
    t["CTkSwitch"]["progress_color"] = [SIGNUM_GREEN, SIGNUM_GREEN]
    t["CTkSwitch"]["button_color"] = [TEXT, TEXT]
    t["CTkSwitch"]["button_hover_color"] = [TEXT, TEXT]
    t["CTkSwitch"]["fg_color"] = [BORDER, BORDER]
    t["CTkSwitch"]["text_color"] = [TEXT, TEXT]
    t["CTkOptionMenu"]["fg_color"] = [SURFACE_ALT, SURFACE_ALT]
    t["CTkOptionMenu"]["button_color"] = [SIGNUM_GREEN_DARK, SIGNUM_GREEN_DARK]
    t["CTkOptionMenu"]["button_hover_color"] = [SIGNUM_GREEN, SIGNUM_GREEN]
    t["CTkOptionMenu"]["text_color"] = [TEXT, TEXT]
    t["CTkScrollableFrame"]["label_fg_color"] = [SURFACE_ALT, SURFACE_ALT]
    t["CTkScrollbar"]["button_color"] = [BORDER, BORDER]
    t["CTkScrollbar"]["button_hover_color"] = [SIGNUM_GREEN_DARK, SIGNUM_GREEN_DARK]
    t["CTkSegmentedButton"]["selected_color"] = [SIGNUM_GREEN, SIGNUM_GREEN]
    t["CTkSegmentedButton"]["selected_hover_color"] = [SIGNUM_GREEN_HOVER, SIGNUM_GREEN_HOVER]
    t["CTkSegmentedButton"]["unselected_color"] = [SURFACE_ALT, SURFACE_ALT]
    t["CTkSegmentedButton"]["unselected_hover_color"] = [BORDER, BORDER]
    t["CTkSegmentedButton"]["fg_color"] = [BORDER, BORDER]
    t["CTkSegmentedButton"]["text_color"] = [TEXT, TEXT]


# ---------------------------------------------------------------------------
# Config file handling that keeps comments and ordering
# ---------------------------------------------------------------------------

KEY_RE = re.compile(r"^([A-Za-z_][A-Za-z0-9_]*)\s*:(.*)$")


def yaml_scalar(value) -> str:
    """Render a scalar the way a human would write it in config.yaml."""
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        return repr(value)
    s = str(value)
    if s == "" or re.search(r"[:#'\"\[\]{}\\]|^\s|\s$", s) or s.lower() in (
        "true",
        "false",
        "yes",
        "no",
        "on",
        "off",
        "null",
        "~",
    ):
        return "'" + s.replace("'", "''") + "'"
    return s


def _split_value_comment(rest: str) -> tuple[str, str]:
    """Split ``value   # comment`` into (value, comment) respecting quotes."""
    in_quote = None
    for i, ch in enumerate(rest):
        if in_quote:
            if ch == in_quote:
                in_quote = None
        elif ch in ("'", '"'):
            in_quote = ch
        elif ch == "#" and (i == 0 or rest[i - 1].isspace()):
            return rest[:i].rstrip(), rest[i:]
    return rest.rstrip(), ""


class ConfigDocument:
    """A ``config.yaml`` kept as text so edits do not destroy comments."""

    def __init__(self, text: str = ""):
        self.lines = text.splitlines()

    @classmethod
    def load(cls, path: Path) -> "ConfigDocument":
        return cls(path.read_text(encoding="utf-8-sig"))

    def text(self) -> str:
        return "\n".join(self.lines) + ("\n" if self.lines else "")

    def save(self, path: Path) -> None:
        path.write_text(self.text(), encoding="utf-8")

    def data(self) -> dict:
        return yaml.safe_load(self.text()) or {}

    # -- locating blocks -------------------------------------------------
    def _find(self, key: str) -> Optional[int]:
        for i, line in enumerate(self.lines):
            m = KEY_RE.match(line)
            if m and m.group(1) == key:
                return i
        return None

    def _block_end(self, start: int) -> int:
        """Index one past the last line belonging to the block at ``start``."""
        end = start + 1
        while end < len(self.lines):
            line = self.lines[end]
            if line.strip() == "" or line[0] in " \t":
                end += 1
                continue
            break
        # Blank lines between blocks stay with the next block.
        while end > start + 1 and self.lines[end - 1].strip() == "":
            end -= 1
        return end

    # -- editing ---------------------------------------------------------
    def set_scalar(self, key: str, value) -> None:
        rendered = yaml_scalar(value)
        idx = self._find(key)
        if idx is None:
            self.lines.append(f"{key}: {rendered}")
            return
        m = KEY_RE.match(self.lines[idx])
        assert m
        old_value, comment = _split_value_comment(m.group(2).strip())
        if old_value == "" and self._block_end(idx) > idx + 1:
            # Was a block (list/map); collapse it to a scalar.
            del self.lines[idx + 1 : self._block_end(idx)]
        pad = ""
        if comment:
            # Keep the comment roughly where it was.
            old_width = len(f"{key}: {old_value}")
            pad = " " * max(1, len(m.group(0)) - len(comment) - old_width)
        self.lines[idx] = f"{key}: {rendered}{pad}{comment}".rstrip()

    def _set_block(self, key: str, body: list[str]) -> None:
        idx = self._find(key)
        if idx is None:
            if self.lines and self.lines[-1].strip():
                self.lines.append("")
            self.lines.append(f"{key}:")
            self.lines.extend(body)
            return
        m = KEY_RE.match(self.lines[idx])
        assert m
        _, comment = _split_value_comment(m.group(2).strip())
        end = self._block_end(idx)
        head = f"{key}:" + (f"  {comment}" if comment else "")
        self.lines[idx:end] = [head] + body

    def set_list(self, key: str, items: list) -> None:
        self._set_block(key, [f"  - {yaml_scalar(v)}" for v in items])

    def set_map(self, key: str, mapping: dict) -> None:
        self._set_block(key, [f"  {yaml_scalar(k)}: {yaml_scalar(v)}" for k, v in mapping.items()])

    def remove(self, key: str) -> None:
        idx = self._find(key)
        if idx is not None:
            del self.lines[idx : self._block_end(idx)]


# ---------------------------------------------------------------------------
# Option metadata for the typed editor
# ---------------------------------------------------------------------------


@dataclass
class Option:
    key: str
    label: str
    kind: str  # "str" | "int" | "bool" | "choice" | "list" | "map"
    help: str = ""
    choices: tuple = ()
    default: object = None


OPTION_GROUPS: list[tuple[str, list[Option]]] = [
    (
        "Pool and plots",
        [
            Option("url", "Pool / wallet URL", "str", "e.g. https://pool.example.com:8080", default=""),
            Option("plot_dirs", "Plot directories", "list", "One directory per line", default=[]),
            Option(
                "account_id_to_secret_phrase",
                "Solo mining passphrases",
                "map",
                "account id: passphrase (leave empty for pool mining)",
                default={},
            ),
        ],
    ),
    (
        "Performance",
        [
            Option("cpu_threads", "Hashing threads", "int", "0 = all cores", default=0),
            Option("cpu_worker_task_count", "CPU worker tasks", "int", "buffers in flight, 4 is good", default=4),
            Option("io_buffer_size", "Read size (bytes)", "int", "multiple of 4096; 16 MiB = 16777216", default=16777216),
            Option("hdd_use_direct_io", "Direct I/O", "bool", "bypass the page cache (recommended)", default=True),
            Option("hdd_reader_thread_count", "Reader threads", "int", "0 = one per drive", default=0),
            Option("hdd_wakeup_after", "Drive wake-up (s)", "int", "read every drive after this idle time; 0 = off", default=240),
            Option("cpu_thread_pinning", "Pin hashing threads", "bool", default=False),
        ],
    ),
    (
        "Mining",
        [
            Option("submit_only_best", "Submit only best deadline", "bool", default=True),
            Option("early_submit_deadline", "Submit immediately below (s)", "int", "a deadline this short is sent at once; 0 = off", default=600),
            Option("target_deadline", "Target deadline (s)", "int", "only submit deadlines below this", default=4294967295),
            Option("get_mining_info_interval", "Poll interval (ms)", "int", default=3000),
            Option("timeout", "Request timeout (ms)", "int", default=5000),
            Option("capacity_check_interval", "Rescan plots every (s)", "int", default=21600),
            Option("plot_missing_grace", "Keep missing plots for (s)", "int", "retry a vanished plot before dropping it", default=3600),
            Option("send_proxy_details", "Send proxy headers", "bool", "X-Capacity / X-Miner for proxies", default=False),
            Option(
                "account_id_to_target_deadline",
                "Per-account target deadlines",
                "map",
                "account id: deadline",
                default={},
            ),
        ],
    ),
    (
        "Logging",
        [
            Option("console_log_level", "Console level", "choice", choices=("off", "error", "warn", "info", "debug", "trace"), default="info"),
            Option("logfile_log_level", "Log file level", "choice", choices=("off", "error", "warn", "info", "debug", "trace"), default="warn"),
            Option("logfile_max_count", "Log files to keep", "int", default=10),
            Option("logfile_max_size", "Log file size (MiB)", "int", default=20),
            Option("show_progress", "Progress bar (terminal only)", "bool", default=True),
            Option("show_drive_stats", "Per-drive speed lines", "bool", default=False),
        ],
    ),
]


# ---------------------------------------------------------------------------
# Log parsing for the dashboard
# ---------------------------------------------------------------------------

LEVEL_RE = re.compile(r"\[(TRACE|DEBUG|INFO|WARN|ERROR)\s*\]")
# The default console pattern is verbose; show "time level message" instead.
COMPACT_RE = re.compile(
    r"^\r?(?P<time>\d\d:\d\d:\d\d)(?:\.\d+)?(?:[+-]\d{4})?\s+\[(?P<level>TRACE|DEBUG|INFO|WARN|ERROR)\s*\]"
    r"(?:\s+\[[^\]]*\])*\s*(?:-\s*[\w:]+:)?\s?(?P<msg>.*)$"
)


def compact_log_line(line: str) -> str:
    m = COMPACT_RE.match(line.rstrip())
    if not m:
        return line.rstrip()
    return f"{m.group('time')} {m.group('level'):<5} {m.group('msg')}"
BLOCK_RE = re.compile(r"new block: height=(\d+), scoop=(\d+)")
ROUND_RE = re.compile(r"round finished: roundtime=(\d+)ms, speed=([\d.]+)MiB/s")
ACCEPTED_RE = re.compile(r"deadline accepted: account=(\d+), nonce=(\d+), deadline=(\d+)")
NOT_ACCEPTED_RE = re.compile(r"submission not accepted|deadlines mismatch")
CAPACITY_RE = re.compile(r"total capacity[:=]\s*([\d.]+) TiB")
LOADED_RE = re.compile(r"plot files loaded: total drives=(\d+), total capacity=([\d.]+) TiB")
DIR_RE = re.compile(r"path=(.+?), files=(\d+), size=([\d.]+) TiB(.*)$")
BACKEND_RE = re.compile(r"hashing backend: (.+?), (\d+) logical CPU")
UNREADABLE_RE = re.compile(r"(\d+) plot file\(s\) are unreadable")
DROPPED_RE = re.compile(r"dropping (.+?) from the capacity")
OUTAGE_RE = re.compile(r"error getting mining info")
RESOLVED_RE = re.compile(r"outage resolved")
INTERRUPTED_RE = re.compile(r"round for height \d+ interrupted")
VERSION_RE = re.compile(r"signum-miner v([\d.]+)")
METRICS_START = "=== MINER METRICS ==="
HEALTH_START = "=== DISK HEALTH ==="


def format_deadline(seconds: int) -> str:
    if seconds >= 10**12:
        return "none"
    parts = []
    for unit, size in (("d", 86400), ("h", 3600), ("m", 60)):
        if seconds >= size:
            parts.append(f"{seconds // size}{unit}")
            seconds %= size
    parts.append(f"{seconds}s")
    return " ".join(parts[:3])


@dataclass
class LogStats:
    version: str = ""
    backend: str = ""
    cpus: int = 0
    height: int = 0
    scoop: int = 0
    block_started: float = 0.0
    rounds: int = 0
    last_round_ms: int = 0
    last_speed: float = 0.0
    capacity_tib: float = 0.0
    drives: int = 0
    dirs: list = field(default_factory=list)
    accepted: int = 0
    rejected: int = 0
    best_deadline: Optional[int] = None
    best_deadline_height: int = 0
    last_accepted: str = ""
    pool_state: str = "waiting"
    unreadable: int = 0
    dropped: list = field(default_factory=list)
    interrupted: int = 0
    errors: int = 0
    warnings: int = 0
    metrics_report: str = ""
    health_report: str = ""
    _capture: Optional[str] = None
    _capture_lines: list = field(default_factory=list)

    def feed(self, line: str) -> None:
        line = line.rstrip("\r\n")
        text = line.strip()

        # Multi-line reports: capture until the next timestamped line.
        if self._capture:
            if LEVEL_RE.search(line):
                self._finish_capture()
            else:
                self._capture_lines.append(text)
                return
        if METRICS_START in text:
            self._capture, self._capture_lines = "metrics", []
            return
        if HEALTH_START in text:
            self._capture, self._capture_lines = "health", []
            return

        level = LEVEL_RE.search(line)
        if level:
            if level.group(1) == "ERROR":
                self.errors += 1
            elif level.group(1) == "WARN":
                self.warnings += 1

        if m := VERSION_RE.search(text):
            self.version = m.group(1)
        if m := BACKEND_RE.search(text):
            self.backend, self.cpus = m.group(1), int(m.group(2))
        if m := BLOCK_RE.search(text):
            self.height, self.scoop = int(m.group(1)), int(m.group(2))
            self.block_started = time.time()
            self.pool_state = "connected"
        if m := ROUND_RE.search(text):
            self.rounds += 1
            self.last_round_ms, self.last_speed = int(m.group(1)), float(m.group(2))
        if m := ACCEPTED_RE.search(text):
            self.accepted += 1
            dl = int(m.group(3))
            if self.best_deadline_height != self.height or self.best_deadline is None or dl < self.best_deadline:
                self.best_deadline, self.best_deadline_height = dl, self.height
            self.last_accepted = f"{format_deadline(dl)} (nonce {m.group(2)})"
        if NOT_ACCEPTED_RE.search(text):
            self.rejected += 1
        if m := LOADED_RE.search(text):
            self.drives, self.capacity_tib = int(m.group(1)), float(m.group(2))
        elif m := CAPACITY_RE.search(text):
            self.capacity_tib = float(m.group(1))
        if m := DIR_RE.search(text):
            self.dirs = [d for d in self.dirs if d[0] != m.group(1)]
            self.dirs.append((m.group(1), int(m.group(2)), float(m.group(3)), m.group(4).strip()))
        if m := UNREADABLE_RE.search(text):
            self.unreadable = int(m.group(1))
        if m := DROPPED_RE.search(text):
            self.dropped.append(m.group(1))
        if OUTAGE_RE.search(text):
            self.pool_state = "unreachable"
        if RESOLVED_RE.search(text):
            self.pool_state = "connected"
        if INTERRUPTED_RE.search(text):
            self.interrupted += 1

    def _finish_capture(self) -> None:
        body = "\n".join(l for l in self._capture_lines if l)
        if self._capture == "metrics":
            self.metrics_report = body
        elif self._capture == "health":
            self.health_report = body
        self._capture, self._capture_lines = None, []

    def flush(self) -> None:
        if self._capture:
            self._finish_capture()


# ---------------------------------------------------------------------------
# Locating the miner binary
# ---------------------------------------------------------------------------


def find_binary(start: Path = HERE) -> Optional[Path]:
    names = ["signum-miner.exe", "signum-miner"] if os.name == "nt" else ["signum-miner"]
    candidates = []
    for name in names:
        candidates += [
            start / name,
            start / "target" / "release" / name,
            start / "target" / "debug" / name,
            Path.cwd() / name,
        ]
    for c in candidates:
        if c.is_file() and os.access(c, os.X_OK):
            return c
    found = shutil.which("signum-miner")
    return Path(found) if found else None


# ---------------------------------------------------------------------------
# The GUI
# ---------------------------------------------------------------------------

FONT = "Segoe UI" if os.name == "nt" else "DejaVu Sans"
MONO = "Consolas" if os.name == "nt" else "DejaVu Sans Mono"


class StatTile(ctk.CTkFrame if ctk else object):
    def __init__(self, master, title: str, value: str = "–", sub: str = "", accent: str = TEXT):
        super().__init__(master, corner_radius=12, fg_color=SURFACE, border_width=1, border_color=BORDER)
        self.grid_columnconfigure(0, weight=1)
        self.title = ctk.CTkLabel(self, text=title.upper(), font=(FONT, 10, "bold"), text_color=TEXT_MUTED, anchor="w")
        self.title.grid(row=0, column=0, sticky="w", padx=14, pady=(10, 0))
        self.value = ctk.CTkLabel(self, text=value, font=(FONT, 22, "bold"), text_color=accent, anchor="w")
        self.value.grid(row=1, column=0, sticky="w", padx=14)
        self.sub = ctk.CTkLabel(self, text=sub, font=(FONT, 11), text_color=TEXT_MUTED, anchor="w")
        self.sub.grid(row=2, column=0, sticky="w", padx=14, pady=(0, 10))

    def set(self, value: str, sub: str = "", accent: Optional[str] = None) -> None:
        self.value.configure(text=value, **({"text_color": accent} if accent else {}))
        self.sub.configure(text=sub)


class MinerUI:
    def __init__(self, master, config_path: Optional[Path] = None):
        self.master = master
        master.title(APP_NAME)
        master.geometry("1120x720")
        master.minsize(900, 600)
        master.configure(fg_color=BG)

        self.binary = find_binary()
        self.config_path = tk.StringVar(value=str(config_path or (HERE / "config.yaml")))
        self.doc = ConfigDocument()
        self.process: Optional[subprocess.Popen] = None
        self.log_queue: "queue.Queue[str]" = queue.Queue()
        self.stats = LogStats()
        self.log_lines: list[tuple[str, str]] = []
        self.log_filter = tk.StringVar(value="all")
        self.autoscroll = tk.BooleanVar(value=True)
        self.option_vars: dict[str, object] = {}
        self.started_at: Optional[float] = None

        self._load_logo()
        self._build_layout()
        self.load_config()
        self.show_page("dashboard")
        self._tick()

    # -- construction ----------------------------------------------------
    def _load_logo(self) -> None:
        self.logo = None
        if Image is None or not LOGO_PATH.exists():
            return
        try:
            img = Image.open(LOGO_PATH)
            img.size  # force load of the largest frame
            img = img.convert("RGBA")
            self.logo = ctk.CTkImage(light_image=img, dark_image=img, size=(56, 56))
            try:
                from PIL import ImageTk

                self._icon = ImageTk.PhotoImage(img.resize((64, 64)))
                self.master.iconphoto(True, self._icon)
            except Exception:
                pass
        except Exception:
            self.logo = None

    def _build_layout(self) -> None:
        self.master.grid_columnconfigure(1, weight=1)
        self.master.grid_rowconfigure(0, weight=1)

        # Sidebar ------------------------------------------------------
        side = ctk.CTkFrame(self.master, width=220, corner_radius=0, fg_color=SURFACE)
        side.grid(row=0, column=0, sticky="nsw")
        side.grid_propagate(False)
        side.grid_rowconfigure(6, weight=1)

        brand = ctk.CTkFrame(side, fg_color="transparent")
        brand.grid(row=0, column=0, sticky="ew", padx=18, pady=(22, 10))
        if self.logo:
            ctk.CTkLabel(brand, image=self.logo, text="").pack(side="left")
        txt = ctk.CTkFrame(brand, fg_color="transparent")
        txt.pack(side="left", padx=(10, 0))
        ctk.CTkLabel(txt, text="SIGNUM", font=(FONT, 20, "bold"), text_color=SIGNUM_GREEN).pack(anchor="w")
        ctk.CTkLabel(txt, text="miner", font=(FONT, 13), text_color=TEXT_MUTED).pack(anchor="w")

        self.nav_buttons = {}
        for row, (key, label) in enumerate([("dashboard", "Dashboard"), ("config", "Config"), ("logs", "Logs")], start=1):
            b = ctk.CTkButton(
                side,
                text=label,
                anchor="w",
                height=40,
                corner_radius=8,
                fg_color="transparent",
                hover_color=SURFACE_ALT,
                text_color=TEXT,
                font=(FONT, 14),
                command=lambda k=key: self.show_page(k),
            )
            b.grid(row=row, column=0, sticky="ew", padx=12, pady=3)
            self.nav_buttons[key] = b

        ctrl = ctk.CTkFrame(side, fg_color="transparent")
        ctrl.grid(row=7, column=0, sticky="ew", padx=12, pady=(10, 8))
        self.start_btn = ctk.CTkButton(ctrl, text="▶  Start mining", height=42, font=(FONT, 14, "bold"), command=self.start_miner)
        self.start_btn.pack(fill="x", pady=(0, 6))
        self.stop_btn = ctk.CTkButton(
            ctrl,
            text="■  Stop",
            height=36,
            fg_color=SURFACE_ALT,
            hover_color=ERROR,
            text_color=TEXT,
            state="disabled",
            command=self.stop_miner,
        )
        self.stop_btn.pack(fill="x")

        self.status_dot = ctk.CTkLabel(side, text="●  Idle", font=(FONT, 12), text_color=TEXT_MUTED, anchor="w")
        self.status_dot.grid(row=8, column=0, sticky="ew", padx=20, pady=(0, 6))
        bin_text = f"binary: {self.binary.name}" if self.binary else "binary: not found"
        self.binary_label = ctk.CTkLabel(side, text=bin_text, font=(FONT, 10), text_color=TEXT_MUTED if self.binary else ERROR, anchor="w")
        self.binary_label.grid(row=9, column=0, sticky="ew", padx=20, pady=(0, 16))

        # Pages --------------------------------------------------------
        self.pages = {}
        container = ctk.CTkFrame(self.master, fg_color="transparent")
        container.grid(row=0, column=1, sticky="nsew", padx=18, pady=16)
        container.grid_columnconfigure(0, weight=1)
        container.grid_rowconfigure(0, weight=1)
        for key, builder in (("dashboard", self._build_dashboard), ("config", self._build_config), ("logs", self._build_logs)):
            frame = ctk.CTkFrame(container, fg_color="transparent")
            frame.grid(row=0, column=0, sticky="nsew")
            builder(frame)
            self.pages[key] = frame

    def _build_dashboard(self, page) -> None:
        page.grid_columnconfigure((0, 1, 2, 3), weight=1, uniform="tiles")
        page.grid_rowconfigure(3, weight=1)

        head = ctk.CTkFrame(page, fg_color="transparent")
        head.grid(row=0, column=0, columnspan=4, sticky="ew", pady=(0, 10))
        ctk.CTkLabel(head, text="Dashboard", font=(FONT, 22, "bold")).pack(side="left")
        self.head_sub = ctk.CTkLabel(head, text="", font=(FONT, 12), text_color=TEXT_MUTED)
        self.head_sub.pack(side="left", padx=14, pady=(6, 0))

        self.tiles = {}
        specs = [
            ("status", "Status", "Idle", "", TEXT_MUTED),
            ("capacity", "Capacity", "–", "no plots loaded", TEXT),
            ("block", "Block", "–", "waiting for pool", TEXT),
            ("round", "Last round", "–", "", TEXT),
            ("deadline", "Best deadline", "–", "this block", SIGNUM_GREEN),
            ("accepted", "Accepted", "0", "submissions", TEXT),
            ("pool", "Pool", "waiting", "", TEXT_MUTED),
            ("health", "Drives", "–", "", TEXT),
        ]
        for i, (key, title, value, sub, accent) in enumerate(specs):
            tile = StatTile(page, title, value, sub, accent)
            tile.grid(row=1 + i // 4, column=i % 4, sticky="nsew", padx=5, pady=5)
            self.tiles[key] = tile

        panes = ctk.CTkFrame(page, fg_color="transparent")
        panes.grid(row=3, column=0, columnspan=4, sticky="nsew", pady=(10, 0))
        panes.grid_columnconfigure((0, 1), weight=1, uniform="panes")
        panes.grid_rowconfigure(1, weight=1)
        ctk.CTkLabel(panes, text="Plot directories", font=(FONT, 12, "bold"), text_color=TEXT_MUTED).grid(row=0, column=0, sticky="w", padx=6)
        ctk.CTkLabel(panes, text="Miner metrics (every 5 min)", font=(FONT, 12, "bold"), text_color=TEXT_MUTED).grid(row=0, column=1, sticky="w", padx=6)
        self.dirs_box = ctk.CTkTextbox(panes, font=(MONO, 11), state="disabled", wrap="none")
        self.dirs_box.grid(row=1, column=0, sticky="nsew", padx=5, pady=4)
        self.metrics_box = ctk.CTkTextbox(panes, font=(MONO, 11), state="disabled", wrap="none")
        self.metrics_box.grid(row=1, column=1, sticky="nsew", padx=5, pady=4)

    def _build_config(self, page) -> None:
        page.grid_columnconfigure(0, weight=1)
        page.grid_rowconfigure(2, weight=1)

        head = ctk.CTkFrame(page, fg_color="transparent")
        head.grid(row=0, column=0, sticky="ew", pady=(0, 8))
        ctk.CTkLabel(head, text="Config", font=(FONT, 22, "bold")).pack(side="left")
        ctk.CTkButton(head, text="Save", width=90, command=self.save_options).pack(side="right", padx=(6, 0))
        ctk.CTkButton(head, text="Validate", width=90, fg_color=SURFACE_ALT, text_color=TEXT, hover_color=BORDER, command=self.validate_config).pack(side="right", padx=(6, 0))
        ctk.CTkButton(head, text="Reload", width=90, fg_color=SURFACE_ALT, text_color=TEXT, hover_color=BORDER, command=self.load_config).pack(side="right", padx=(6, 0))

        path_row = ctk.CTkFrame(page, fg_color="transparent")
        path_row.grid(row=1, column=0, sticky="ew", pady=(0, 8))
        ctk.CTkLabel(path_row, text="File", text_color=TEXT_MUTED).pack(side="left", padx=(4, 8))
        ctk.CTkEntry(path_row, textvariable=self.config_path).pack(side="left", fill="x", expand=True)
        ctk.CTkButton(path_row, text="Browse", width=80, fg_color=SURFACE_ALT, text_color=TEXT, hover_color=BORDER, command=self.browse_config).pack(side="left", padx=(6, 0))

        self.config_tabs = ctk.CTkTabview(page, fg_color=SURFACE, segmented_button_fg_color=BORDER)
        self.config_tabs.grid(row=2, column=0, sticky="nsew")
        self.options_tab = self.config_tabs.add("Settings")
        self.raw_tab = self.config_tabs.add("Raw YAML")

        self.options_frame = ctk.CTkScrollableFrame(self.options_tab, fg_color="transparent")
        self.options_frame.pack(fill="both", expand=True)
        self.options_frame.grid_columnconfigure(1, weight=1)

        self.raw_text = ctk.CTkTextbox(self.raw_tab, font=(MONO, 12), wrap="none")
        self.raw_text.pack(fill="both", expand=True, padx=4, pady=4)
        raw_btns = ctk.CTkFrame(self.raw_tab, fg_color="transparent")
        raw_btns.pack(fill="x", padx=4, pady=(0, 4))
        ctk.CTkLabel(raw_btns, text="Edits here replace the whole file; the Settings tab keeps your comments.", text_color=TEXT_MUTED, font=(FONT, 11)).pack(side="left")
        ctk.CTkButton(raw_btns, text="Save raw YAML", width=130, command=self.save_raw).pack(side="right")

    def _build_logs(self, page) -> None:
        page.grid_columnconfigure(0, weight=1)
        page.grid_rowconfigure(1, weight=1)
        head = ctk.CTkFrame(page, fg_color="transparent")
        head.grid(row=0, column=0, sticky="ew", pady=(0, 8))
        ctk.CTkLabel(head, text="Logs", font=(FONT, 22, "bold")).pack(side="left")
        ctk.CTkButton(head, text="Clear", width=80, fg_color=SURFACE_ALT, text_color=TEXT, hover_color=BORDER, command=self.clear_logs).pack(side="right")
        ctk.CTkSwitch(head, text="Follow", variable=self.autoscroll).pack(side="right", padx=12)
        ctk.CTkOptionMenu(head, values=["all", "info+", "warn+", "error"], variable=self.log_filter, width=100, command=lambda _v: self._render_logs()).pack(side="right", padx=6)
        ctk.CTkLabel(head, text="Show", text_color=TEXT_MUTED).pack(side="right")

        self.log_text = ctk.CTkTextbox(page, font=(MONO, 11), state="disabled", wrap="none")
        self.log_text.grid(row=1, column=0, sticky="nsew")
        inner = self.log_text._textbox
        inner.tag_configure("ERROR", foreground=ERROR)
        inner.tag_configure("WARN", foreground=WARN)
        inner.tag_configure("INFO", foreground=TEXT)
        inner.tag_configure("DEBUG", foreground=TEXT_MUTED)
        inner.tag_configure("TRACE", foreground=TEXT_MUTED)
        inner.tag_configure("good", foreground=SIGNUM_GREEN)

    # -- navigation ------------------------------------------------------
    def show_page(self, key: str) -> None:
        for k, b in self.nav_buttons.items():
            b.configure(fg_color=SIGNUM_GREEN_DARK if k == key else "transparent", text_color="#FFFFFF" if k == key else TEXT)
        self.pages[key].tkraise()

    # -- config ----------------------------------------------------------
    def browse_config(self) -> None:
        path = filedialog.askopenfilename(initialfile=self.config_path.get(), filetypes=[("YAML", "*.yaml *.yml"), ("All files", "*")])
        if path:
            self.config_path.set(path)
            self.load_config()

    def load_config(self) -> None:
        path = Path(self.config_path.get())
        try:
            self.doc = ConfigDocument.load(path)
        except OSError as e:
            self.doc = ConfigDocument("")
            self._set_text(self.raw_text, "")
            self.populate_options({})
            if path.name != "config.yaml" or path.exists():
                messagebox.showerror(APP_NAME, f"Failed to load config:\n{e}")
            return
        try:
            data = self.doc.data()
        except yaml.YAMLError as e:
            messagebox.showerror(APP_NAME, f"{path} is not valid YAML:\n{e}")
            data = {}
        self._set_text(self.raw_text, self.doc.text())
        self.populate_options(data)

    def populate_options(self, data: dict) -> None:
        for w in self.options_frame.winfo_children():
            w.destroy()
        self.option_vars.clear()
        row = 0
        for group, options in OPTION_GROUPS:
            ctk.CTkLabel(self.options_frame, text=group, font=(FONT, 13, "bold"), text_color=SIGNUM_GREEN).grid(row=row, column=0, columnspan=3, sticky="w", padx=6, pady=(14 if row else 4, 4))
            row += 1
            for opt in options:
                value = data.get(opt.key, opt.default)
                ctk.CTkLabel(self.options_frame, text=opt.label, anchor="w").grid(row=row, column=0, sticky="w", padx=(10, 12), pady=4)
                if opt.kind == "bool":
                    var = tk.BooleanVar(value=bool(value))
                    ctk.CTkSwitch(self.options_frame, text="", variable=var, width=50).grid(row=row, column=1, sticky="w", pady=4)
                elif opt.kind == "choice":
                    var = tk.StringVar(value=str(value).lower())
                    ctk.CTkOptionMenu(self.options_frame, values=list(opt.choices), variable=var, width=140).grid(row=row, column=1, sticky="w", pady=4)
                elif opt.kind in ("list", "map"):
                    var = ctk.CTkTextbox(self.options_frame, height=84, font=(MONO, 12))
                    if opt.kind == "list":
                        var.insert("1.0", "\n".join(str(v) for v in (value or [])))
                    else:
                        var.insert("1.0", "\n".join(f"{k}: {v}" for k, v in (value or {}).items()))
                    var.grid(row=row, column=1, sticky="ew", pady=4)
                else:
                    var = tk.StringVar(value="" if value is None else str(value))
                    ctk.CTkEntry(self.options_frame, textvariable=var, width=320).grid(row=row, column=1, sticky="ew", pady=4)
                ctk.CTkLabel(self.options_frame, text=opt.help, text_color=TEXT_MUTED, font=(FONT, 11), anchor="w").grid(row=row, column=2, sticky="w", padx=10)
                self.option_vars[opt.key] = (opt, var)
                row += 1

    def collect_options(self) -> tuple[dict, list[str]]:
        """Read the editor widgets; returns (values, errors)."""
        values, errors = {}, []
        for key, (opt, var) in self.option_vars.items():
            try:
                if opt.kind == "bool":
                    values[key] = bool(var.get())
                elif opt.kind == "int":
                    raw = var.get().strip().replace("_", "")
                    if raw == "":
                        continue
                    values[key] = int(raw)
                elif opt.kind == "choice":
                    values[key] = var.get()
                elif opt.kind == "list":
                    items = [l.strip() for l in var.get("1.0", "end").splitlines() if l.strip()]
                    values[key] = items
                elif opt.kind == "map":
                    mapping = {}
                    for l in var.get("1.0", "end").splitlines():
                        if not l.strip():
                            continue
                        if ":" not in l:
                            raise ValueError(f"'{l.strip()}' is not 'key: value'")
                        k, v = l.split(":", 1)
                        k, v = k.strip(), v.strip()
                        mapping[int(k) if k.isdigit() else k] = int(v) if v.isdigit() else v
                    values[key] = mapping
                else:
                    values[key] = var.get().strip()
            except ValueError as e:
                errors.append(f"{opt.label}: {e}")
        return values, errors

    def apply_options_to_doc(self, values: dict) -> None:
        for key, (opt, _var) in self.option_vars.items():
            if key not in values:
                continue
            value = values[key]
            if opt.kind == "list":
                self.doc.set_list(key, value)
            elif opt.kind == "map":
                if value:
                    self.doc.set_map(key, value)
                else:
                    self.doc.remove(key)
            else:
                self.doc.set_scalar(key, value)

    def save_options(self) -> None:
        values, errors = self.collect_options()
        if errors:
            messagebox.showerror(APP_NAME, "Please fix these settings first:\n\n" + "\n".join(errors))
            return
        if not values.get("url"):
            messagebox.showerror(APP_NAME, "The pool / wallet URL is required.")
            return
        self.apply_options_to_doc(values)
        try:
            self.doc.save(Path(self.config_path.get()))
        except OSError as e:
            messagebox.showerror(APP_NAME, f"Failed to save config:\n{e}")
            return
        self._set_text(self.raw_text, self.doc.text())
        self.set_status("Config saved")
        if self.process and self.process.poll() is None:
            messagebox.showinfo(APP_NAME, "Saved. Restart the miner to apply the new settings.")

    def save_raw(self) -> None:
        text = self.raw_text.get("1.0", "end").rstrip("\n") + "\n"
        try:
            yaml.safe_load(text)
        except yaml.YAMLError as e:
            messagebox.showerror(APP_NAME, f"Not valid YAML:\n{e}")
            return
        self.doc = ConfigDocument(text)
        try:
            self.doc.save(Path(self.config_path.get()))
        except OSError as e:
            messagebox.showerror(APP_NAME, f"Failed to save config:\n{e}")
            return
        self.populate_options(self.doc.data())
        self.set_status("Config saved")

    def validate_config(self) -> None:
        if not self.binary:
            messagebox.showerror(APP_NAME, "signum-miner binary not found next to miner_ui.py or in target/release.")
            return
        values, errors = self.collect_options()
        if errors:
            messagebox.showerror(APP_NAME, "Please fix these settings first:\n\n" + "\n".join(errors))
            return
        # Validate what is on screen without touching the real file.
        import tempfile

        self.apply_options_to_doc(values)
        with tempfile.NamedTemporaryFile("w", suffix=".yaml", delete=False, encoding="utf-8") as tmp:
            tmp.write(self.doc.text())
            tmp_path = tmp.name
        try:
            proc = subprocess.run(
                [str(self.binary), "--config", tmp_path, "--check"],
                capture_output=True,
                text=True,
                timeout=120,
                cwd=str(Path(self.config_path.get()).parent),
            )
            output = (proc.stdout + proc.stderr).strip()
            if proc.returncode == 0:
                messagebox.showinfo(APP_NAME, "Configuration check passed.\n\n" + output[-3000:])
            else:
                messagebox.showerror(APP_NAME, "Configuration check failed:\n\n" + output[-3000:])
        except (OSError, subprocess.TimeoutExpired) as e:
            messagebox.showerror(APP_NAME, f"Could not run the check: {e}")
        finally:
            os.unlink(tmp_path)

    # -- miner process ---------------------------------------------------
    def start_miner(self) -> None:
        if not self.binary:
            messagebox.showerror(APP_NAME, "signum-miner binary not found.\nBuild it with `cargo build --release` or put it next to miner_ui.py.")
            return
        config = Path(self.config_path.get())
        if not config.exists():
            messagebox.showerror(APP_NAME, f"Config file not found:\n{config}")
            return
        cmd = [str(self.binary), "--config", str(config)]
        try:
            self.process = subprocess.Popen(
                cmd,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                cwd=str(config.parent),
                encoding="utf-8",
                errors="replace",
            )
        except OSError as e:
            messagebox.showerror(APP_NAME, f"Failed to start miner:\n{e}")
            self.process = None
            return
        self.stats = LogStats()
        self.started_at = time.time()
        self.start_btn.configure(state="disabled")
        self.stop_btn.configure(state="normal")
        self.set_status("Mining", SIGNUM_GREEN)
        threading.Thread(target=self._pump_output, args=(self.process,), daemon=True).start()
        self.show_page("dashboard")

    def stop_miner(self) -> None:
        proc = self.process
        if proc and proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
        self._miner_exited()

    def _pump_output(self, proc: subprocess.Popen) -> None:
        assert proc.stdout
        for line in proc.stdout:
            self.log_queue.put(line)
        self.log_queue.put(None)  # sentinel: process ended

    def _miner_exited(self) -> None:
        code = self.process.poll() if self.process else None
        self.process = None
        self.start_btn.configure(state="normal")
        self.stop_btn.configure(state="disabled")
        if code not in (None, 0, -15):
            self.set_status(f"Miner exited with status {code}", ERROR)
        else:
            self.set_status("Stopped", TEXT_MUTED)

    # -- periodic refresh ------------------------------------------------
    def _tick(self) -> None:
        drained = 0
        while drained < 500:
            try:
                line = self.log_queue.get_nowait()
            except queue.Empty:
                break
            if line is None:
                self.stats.flush()
                self._miner_exited()
                continue
            drained += 1
            self.stats.feed(line)
            level = LEVEL_RE.search(line)
            self.log_lines.append((level.group(1) if level else "INFO", line.rstrip("\n")))
        if len(self.log_lines) > 5000:
            del self.log_lines[: len(self.log_lines) - 5000]
        if drained:
            self._append_logs(self.log_lines[-drained:])
        self._refresh_dashboard()
        self.master.after(250, self._tick)

    def _refresh_dashboard(self) -> None:
        s = self.stats
        running = self.process is not None and self.process.poll() is None
        if running:
            up = int(time.time() - (self.started_at or time.time()))
            self.tiles["status"].set("Mining", f"up {format_deadline(up)} · {s.backend or '…'}", SIGNUM_GREEN)
        else:
            self.tiles["status"].set("Idle", f"v{s.version}" if s.version else "", TEXT_MUTED)
        if s.capacity_tib:
            cap = f"{s.capacity_tib * 1.0995:.2f} TB" if s.capacity_tib >= 1 else f"{s.capacity_tib * 1024:.1f} GiB"
            sub = f"{s.drives} drive{'s' if s.drives != 1 else ''}"
            if s.unreadable:
                sub += f" · {s.unreadable} plot(s) unreadable"
            self.tiles["capacity"].set(cap, sub, WARN if s.unreadable else TEXT)
        if s.height:
            age = int(time.time() - s.block_started)
            self.tiles["block"].set(f"{s.height:,}", f"scoop {s.scoop} · {age}s ago")
        if s.rounds:
            rt = s.last_round_ms / 1000
            accent = ERROR if rt > 180 else WARN if rt > 120 else TEXT
            self.tiles["round"].set(f"{rt:.1f} s", f"{s.last_speed:.0f} MiB/s · {s.rounds} rounds", accent)
        if s.best_deadline is not None and s.best_deadline_height == s.height:
            self.tiles["deadline"].set(format_deadline(s.best_deadline), f"block {s.height:,}")
        elif s.height:
            self.tiles["deadline"].set("–", "scanning…")
        rej = f" · {s.rejected} rejected" if s.rejected else ""
        self.tiles["accepted"].set(str(s.accepted), (s.last_accepted or "submissions") + rej, ERROR if s.rejected else TEXT)
        pool_colour = {"connected": SIGNUM_GREEN, "unreachable": ERROR}.get(s.pool_state, TEXT_MUTED)
        self.tiles["pool"].set(s.pool_state, f"{s.interrupted} interrupted rounds" if s.interrupted else "", pool_colour)
        health = "ok"
        colour = SIGNUM_GREEN if running else TEXT_MUTED
        if s.health_report and ("CRITICAL" in s.health_report or "WARNING" in s.health_report):
            health, colour = "check", WARN
        if s.dropped:
            health, colour = f"{len(s.dropped)} dropped", ERROR
        self.tiles["health"].set(health if s.drives else "–", f"{s.errors} errors · {s.warnings} warnings")
        self.tiles["health"].value.configure(text_color=colour)

        dirs = "\n".join(f"{d[1]:>4} files  {d[2]:7.3f} TiB  {d[0]}{('  ' + d[3]) if d[3] else ''}" for d in s.dirs) or "No plot directories reported yet."
        self._set_text(self.dirs_box, dirs, readonly=True)
        report = s.metrics_report or "The miner prints a metrics summary every 5 minutes."
        if s.health_report:
            report += "\n\n" + s.health_report
        self._set_text(self.metrics_box, report, readonly=True)
        if s.version:
            self.head_sub.configure(text=f"signum-miner v{s.version}" + (f" · {s.backend} · {s.cpus} CPUs" if s.backend else ""))

    # -- logs ------------------------------------------------------------
    def _visible(self, level: str) -> bool:
        f = self.log_filter.get()
        order = {"TRACE": 0, "DEBUG": 1, "INFO": 2, "WARN": 3, "ERROR": 4}
        threshold = {"all": 0, "info+": 2, "warn+": 3, "error": 4}[f]
        return order.get(level, 2) >= threshold

    def _append_logs(self, lines) -> None:
        inner = self.log_text._textbox
        self.log_text.configure(state="normal")
        for level, line in lines:
            if not self._visible(level):
                continue
            tag = "good" if "deadline accepted" in line or "round finished" in line else level
            inner.insert("end", compact_log_line(line) + "\n", tag)
        self.log_text.configure(state="disabled")
        if self.autoscroll.get():
            self.log_text.see("end")

    def _render_logs(self) -> None:
        self.log_text.configure(state="normal")
        self.log_text.delete("1.0", "end")
        self.log_text.configure(state="disabled")
        self._append_logs(self.log_lines)

    def clear_logs(self) -> None:
        self.log_lines.clear()
        self._render_logs()

    # -- helpers ---------------------------------------------------------
    def set_status(self, text: str, colour: str = TEXT_MUTED) -> None:
        self.status_dot.configure(text=f"●  {text}", text_color=colour)

    @staticmethod
    def _set_text(box, text: str, readonly: bool = False) -> None:
        if readonly:
            current = box.get("1.0", "end").rstrip("\n")
            if current == text:
                return
            box.configure(state="normal")
        box.delete("1.0", "end")
        box.insert("1.0", text)
        if readonly:
            box.configure(state="disabled")

    def on_close(self) -> None:
        if self.process and self.process.poll() is None:
            if not messagebox.askyesno(APP_NAME, "The miner is running. Stop it and quit?"):
                return
            self.stop_miner()
        self.master.destroy()


def main(argv: Optional[list[str]] = None) -> int:
    parser = argparse.ArgumentParser(description="Signum Miner UI")
    parser.add_argument("--config", help="config.yaml to open (default: next to miner_ui.py)")
    parser.add_argument("--screenshot", help=argparse.SUPPRESS)
    parser.add_argument("--screenshot-page", default="dashboard", help=argparse.SUPPRESS)
    parser.add_argument("--autostart", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args(argv)

    if ctk is None:
        print("The GUI needs tkinter, customtkinter and pillow: pip install -r requirements.txt", file=sys.stderr)
        return 1

    apply_signum_theme()
    root = ctk.CTk()
    app = MinerUI(root, Path(args.config) if args.config else None)
    root.protocol("WM_DELETE_WINDOW", app.on_close)

    if args.autostart:
        root.after(300, app.start_miner)
    if args.screenshot:
        def shoot():
            app.show_page(args.screenshot_page)
            root.update()
            subprocess.run(["scrot", "-o", args.screenshot], check=False)
            app.stop_miner()
            root.destroy()

        root.after(9000 if args.autostart else 1500, shoot)
    root.mainloop()
    return 0


if __name__ == "__main__":
    sys.exit(main())
