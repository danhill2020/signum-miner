"""Unit tests for the GUI's non-graphical logic: run with
``python3 -m unittest discover -s tests/ui`` from the repository root."""
import sys
import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))

import miner_ui  # noqa: E402
from miner_ui import ConfigDocument, LogStats, format_deadline, yaml_scalar  # noqa: E402

SAMPLE = """\
# List of directories with your plot files, adjust to suit your disks
plot_dirs:
  - 'D:\\plot\\dir'             # Sample Windows directory
  - '/mnt/hd1/plot/dir'       # Sample Linux directory

url: 'https://pool.burstcoin.ro'      # mainnet pool
#url: 'http://localhost:8125'         # solo mining

hdd_use_direct_io: true               # default true
cpu_threads: 4                        # default 4 (0=auto)
#account_id_to_target_deadline:        # target dls for multi-id (optional)
# 10282355196851764065: 600000
console_log_level: 'info'             # default Info
"""


class ConfigDocumentTests(unittest.TestCase):
    def test_scalar_edit_keeps_comment_and_order(self):
        doc = ConfigDocument(SAMPLE)
        doc.set_scalar("cpu_threads", 0)
        doc.set_scalar("hdd_use_direct_io", False)
        doc.set_scalar("url", "pool.example.com:8080")
        text = doc.text()
        self.assertIn("cpu_threads: 0", text)
        self.assertIn("# default 4 (0=auto)", text)
        self.assertIn("hdd_use_direct_io: false", text)
        self.assertIn("url: 'pool.example.com:8080'", text)
        self.assertIn("# mainnet pool", text)
        self.assertIn("#url: 'http://localhost:8125'", text)
        self.assertLess(text.index("plot_dirs"), text.index("url:"))
        self.assertEqual(doc.data()["cpu_threads"], 0)
        self.assertEqual(doc.data()["url"], "pool.example.com:8080")

    def test_list_edit_replaces_block_only(self):
        doc = ConfigDocument(SAMPLE)
        doc.set_list("plot_dirs", ["/mnt/bay1/plots", "/mnt/bay2/plots"])
        data = doc.data()
        self.assertEqual(data["plot_dirs"], ["/mnt/bay1/plots", "/mnt/bay2/plots"])
        self.assertEqual(data["url"], "https://pool.burstcoin.ro")
        self.assertIn("# List of directories", doc.text())
        self.assertNotIn("Sample Windows", doc.text())

    def test_missing_keys_are_appended_and_maps_round_trip(self):
        doc = ConfigDocument(SAMPLE)
        doc.set_scalar("plot_missing_grace", 3600)
        doc.set_map("account_id_to_target_deadline", {10282355196851764065: 600000})
        data = doc.data()
        self.assertEqual(data["plot_missing_grace"], 3600)
        self.assertEqual(data["account_id_to_target_deadline"], {10282355196851764065: 600000})
        doc.remove("account_id_to_target_deadline")
        self.assertNotIn("account_id_to_target_deadline", doc.data())
        # The commented-out example is untouched.
        self.assertIn("#account_id_to_target_deadline:", doc.text())

    def test_windows_paths_and_quoting(self):
        doc = ConfigDocument("")
        doc.set_list("plot_dirs", ["D:\\plots", "/mnt/x y", "plain"])
        doc.set_scalar("url", "https://p.example:8080")
        self.assertEqual(doc.data()["plot_dirs"], ["D:\\plots", "/mnt/x y", "plain"])
        self.assertEqual(yaml_scalar(True), "true")
        self.assertEqual(yaml_scalar("yes"), "'yes'")
        self.assertEqual(yaml_scalar("it's"), "'it''s'")
        self.assertEqual(yaml_scalar(12), "12")

    def test_bom_is_tolerated_on_load(self):
        import tempfile

        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "config.yaml"
            p.write_bytes(b"\xef\xbb\xbfurl: 'http://x'\nplot_dirs: []\n")
            doc = ConfigDocument.load(p)
            self.assertEqual(doc.data()["url"], "http://x")

    def test_sample_config_round_trips_through_every_option(self):
        doc = ConfigDocument((ROOT / "config.yaml").read_text())
        before = doc.data()
        for _group, options in miner_ui.OPTION_GROUPS:
            for opt in options:
                if opt.key not in before:
                    continue
                value = before[opt.key]
                if opt.kind == "list":
                    doc.set_list(opt.key, value)
                elif opt.kind == "map":
                    doc.set_map(opt.key, value)
                else:
                    doc.set_scalar(opt.key, value)
        self.assertEqual(doc.data(), before)
        self.assertIn("# Low noise log patterns", doc.text())


class LogStatsTests(unittest.TestCase):
    def test_parses_the_miner_log_format(self):
        s = LogStats()
        for line in [
            "16:06:37 [INFO] signum-miner v2.1.0",
            "16:06:37 [INFO] hashing backend: NEON, 4 logical CPU(s)",
            "16:06:37 [INFO] path=/mnt/bay1, files=12, size=15.9980 TiB (USB)",
            "16:06:37 [INFO] plot files loaded: total drives=3, total capacity=88.1234 TiB",
            "16:06:38 [INFO] new block: height=1000000, scoop=1723",
            "16:06:39 [INFO] round finished: roundtime=61234ms, speed=411.20MiB/s",
            "16:06:41 [INFO] deadline accepted: account=1, nonce=7, deadline=90061",
            "16:06:41 [INFO] deadline accepted: account=1, nonce=8, deadline=59",
            "16:06:42 [WARN] 2 plot file(s) are unreadable right now and are kept in the capacity for up to 60 min (retried every round): /a, /b",
            "16:06:43 [ERROR] error getting mining info => connection outage: http error",
            "16:06:44 [INFO] \n",
            "=== MINER METRICS ===",
            "Uptime: 1h 0m 0s",
            "Rounds: 10 completed, 0 failed (100.0% success)",
            "16:06:45 [ERROR] outage resolved.",
            "16:06:46 [WARN] round for height 1000001 interrupted after 250s with 2 of 3 drives finished",
        ]:
            s.feed(line)
        s.flush()
        self.assertEqual(s.version, "2.1.0")
        self.assertEqual((s.backend, s.cpus), ("NEON", 4))
        self.assertEqual(s.dirs[0][:3], ("/mnt/bay1", 12, 15.998))
        self.assertEqual(s.dirs[0][3], "(USB)")
        self.assertEqual((s.drives, s.capacity_tib), (3, 88.1234))
        self.assertEqual((s.height, s.scoop), (1000000, 1723))
        self.assertEqual((s.last_round_ms, s.last_speed), (61234, 411.2))
        self.assertEqual(s.accepted, 2)
        self.assertEqual(s.best_deadline, 59)
        self.assertEqual(s.unreadable, 2)
        self.assertEqual(s.pool_state, "connected")
        self.assertIn("Rounds: 10 completed", s.metrics_report)
        self.assertEqual(s.interrupted, 1)
        self.assertEqual(s.errors, 2)

    def test_compact_log_line(self):
        from miner_ui import compact_log_line

        self.assertEqual(
            compact_log_line("17:25:38.887+0000 [INFO ] [main] [signum_miner] - signum_miner:signum-miner v2.1.0"),
            "17:25:38 INFO  signum-miner v2.1.0",
        )
        self.assertEqual(
            compact_log_line("16:06:37 [WARN] no plots in /mnt/x\n"),
            "16:06:37 WARN  no plots in /mnt/x",
        )
        self.assertEqual(compact_log_line("=== MINER METRICS ==="), "=== MINER METRICS ===")

    def test_format_deadline(self):
        self.assertEqual(format_deadline(59), "59s")
        self.assertEqual(format_deadline(90061), "1d 1h 1m")
        self.assertEqual(format_deadline(3600), "1h 0s")
        self.assertEqual(format_deadline(10**13), "none")


class BinaryLookupTests(unittest.TestCase):
    def test_find_binary_prefers_sibling_then_target(self):
        import os
        import tempfile

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            (root / "target" / "release").mkdir(parents=True)
            exe = root / "target" / "release" / "signum-miner"
            exe.write_text("#!/bin/sh\n")
            exe.chmod(0o755)
            self.assertEqual(miner_ui.find_binary(root), exe)
            sibling = root / "signum-miner"
            sibling.write_text("#!/bin/sh\n")
            sibling.chmod(0o755)
            self.assertEqual(miner_ui.find_binary(root), sibling)
            os.chmod(sibling, 0o644)
            self.assertEqual(miner_ui.find_binary(root), exe)


class NewParsingTests(unittest.TestCase):
    def test_round_history_and_drive_health(self):
        s = LogStats()
        for line in [
            "16:06:39 [INFO] round finished: roundtime=61234ms, speed=411.20MiB/s",
            "16:07:39 [INFO] round finished: roundtime=58000ms, speed=430.00MiB/s",
            "16:07:40 [INFO] \n",
            "=== DISK HEALTH ===",
            "Drive /mnt/bay1: \u2713 HEALTHY (errors: 0/120, rate: 0.00%, consecutive: 0; last round 57.5s at 282 MiB/s, slowest 61.2s)",
            "Drive /mnt/bay2: \u26a0 WARNING (errors: 3/120, rate: 2.50%, consecutive: 1; last round 58.0s at 275 MiB/s, slowest 90.1s)",
            "",
            "16:07:41 [INFO] new block: height=1000001, scoop=5",
        ]:
            s.feed(line)
        s.flush()
        self.assertEqual(s.round_history, [(61234, 411.2), (58000, 430.0)])
        self.assertEqual(sorted(s.drive_health), ["/mnt/bay1", "/mnt/bay2"])
        bay2 = s.drive_health["/mnt/bay2"]
        self.assertEqual((bay2.status, bay2.failed, bay2.total, bay2.rate, bay2.consecutive), ("WARNING", 3, 120, 2.5, 1))
        self.assertEqual((bay2.last_round_s, bay2.speed, bay2.slowest_s), (58.0, 275.0, 90.1))
        self.assertEqual(s.height, 1000001)

    def test_plotter_stats(self):
        from miner_ui import PlotterStats

        p = PlotterStats()
        p.feed("progress: 12.5% (512/4096 nonces), 2900 nonces/min (12.7 MB/s), elapsed 10s, eta 1m 14s")
        self.assertEqual((p.percent, p.done, p.total, p.rate, p.mb_s, p.elapsed, p.eta), (12.5, 512, 4096, 2900, 12.7, "10s", "1m 14s"))
        self.assertFalse(p.finished)
        p.feed("done: 4096 nonces in 1m 25s (2891 nonces/min, 12.6 MB/s) -> /mnt/bay1/plots/5_0_4096")
        self.assertTrue(p.finished)
        self.assertEqual((p.percent, p.done, p.total, p.output_path), (100.0, 4096, 4096, "/mnt/bay1/plots/5_0_4096"))
        p.feed("error: plotting failed: No space left on device (os error 28)")
        self.assertEqual(p.error, "plotting failed: No space left on device (os error 28)")
        p.feed("resuming: 1024 of 4096 nonces were already on disk")
        self.assertEqual(p.resumed_from, 1024)

    def test_parse_device_list(self):
        from miner_ui import parse_device_list

        out = "0:0  Portable Computing Language / cpu-haswell-AMD EPYC (4 compute units, 15.6 GiB)\n0:1  P / GPU X (32 compute units, 8.0 GiB)\nNo OpenCL devices found.\n"
        self.assertEqual(
            parse_device_list(out),
            [("0:0", "Portable Computing Language / cpu-haswell-AMD EPYC (4 compute units, 15.6 GiB)"), ("0:1", "P / GPU X (32 compute units, 8.0 GiB)")],
        )
        self.assertEqual(parse_device_list("This build has no OpenCL support"), [])

    def test_accounts_in_dirs_and_settings(self):
        import os
        import tempfile

        from miner_ui import accounts_in_dirs, load_settings, save_settings, settings_path

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            for name in ["5_0_100", "5_100_50", "6_0_1000", "notes.txt", "5_x_1"]:
                (root / name).write_text("")
            self.assertEqual(accounts_in_dirs([str(root), str(root / "missing")]), [5, 6])
            old = os.environ.get("XDG_CONFIG_HOME")
            os.environ["XDG_CONFIG_HOME"] = str(root / "cfg")
            try:
                self.assertEqual(settings_path(), root / "cfg" / "signum-miner" / "ui.json")
                self.assertEqual(load_settings(), {})
                save_settings({"plotter": {"path": "/mnt/bay1/plots", "gpu": "0:0"}})
                self.assertEqual(load_settings()["plotter"]["gpu"], "0:0")
            finally:
                if old is None:
                    del os.environ["XDG_CONFIG_HOME"]
                else:
                    os.environ["XDG_CONFIG_HOME"] = old

    def test_plotter_command_builder(self):
        """The command line the GUI builds for signum-plotter (no window needed)."""
        import os
        import tempfile
        import types

        class Var:
            def __init__(self, v):
                self.v = v

            def get(self):
                return self.v

        plot_dir = tempfile.mkdtemp()
        app = types.SimpleNamespace(
            plotter_binary=Path("/opt/signum-plotter"),
            plot_account=Var("1234567890123456789"),
            plot_dir=Var(plot_dir),
            plot_size=Var("0.5"),
            plot_start=Var("4096"),
            plot_device=Var("GPU 0:1  Some GPU (32 compute units, 8.0 GiB)"),
            plot_threads=Var("3"),
            plot_mem=Var("2"),
            plot_low_priority=Var(True),
        )
        cmd = miner_ui.MinerUI._plotter_command(app, bench=True)
        self.assertEqual(
            cmd,
            ["/opt/signum-plotter", "--id", "1234567890123456789", "--path", plot_dir, "--nonces", "2048", "--start-nonce", "4096", "--gpu", "0:1", "--mem", "2", "--low-priority", "--bench"],
        )
        app.plot_device = Var("CPU")
        app.plot_mem = Var("")
        app.plot_low_priority = Var(False)
        cmd = miner_ui.MinerUI._plotter_command(app, bench=False)
        self.assertEqual(cmd[-4:], ["--start-nonce", "4096", "--threads", "3"])
        os.rmdir(plot_dir)

    def test_find_binary_by_name(self):
        import tempfile

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            (root / "target" / "release").mkdir(parents=True)
            exe = root / "target" / "release" / "signum-plotter"
            exe.write_text("#!/bin/sh\n")
            exe.chmod(0o755)
            self.assertEqual(miner_ui.find_binary(root, "signum-plotter"), exe)


if __name__ == "__main__":
    unittest.main()
