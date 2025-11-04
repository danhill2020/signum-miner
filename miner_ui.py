import os
import subprocess
import threading
import queue
import tkinter as tk
from tkinter import filedialog, messagebox
import customtkinter as ctk
import yaml

class MinerUI:
    def __init__(self, master):
        self.master = master
        self.master.title("Signum Miner UI")
        self.master.geometry("800x600")

        # configure CustomTkinter appearance
        ctk.set_appearance_mode("Dark")
        ctk.set_default_color_theme("blue")

        self.config_path = tk.StringVar(value="config.yaml")
        self.config_data = {}

        self.tabview = ctk.CTkTabview(master)
        self.tabview.pack(fill="both", expand=True)

        self.home_tab = self.tabview.add("Home")
        self.config_tab = self.tabview.add("Config")
        self.options_tab = self.tabview.add("Options")
        self.metrics_tab = self.tabview.add("Metrics")
        self.logs_tab = self.tabview.add("Logs")

        # Home tab content with ASCII logo
        ascii_logo = """
   ███████╗██╗ ██████╗ ███╗   ██╗██╗   ██╗███╗   ███╗
   ██╔════╝██║██╔════╝ ████╗  ██║██║   ██║████╗ ████║
   ███████╗██║██║  ███╗██╔██╗ ██║██║   ██║██╔████╔██║
   ╚════██║██║██║   ██║██║╚██╗██║██║   ██║██║╚██╔╝██║
   ███████║██║╚██████╔╝██║ ╚████║╚██████╔╝██║ ╚═╝ ██║
   ╚══════╝╚═╝ ╚═════╝ ╚═╝  ╚═══╝ ╚═════╝ ╚═╝     ╚═╝

        ⛏️  The Sustainable Blockchain  ⛏️
        """

        logo_label = ctk.CTkLabel(
            self.home_tab,
            text=ascii_logo,
            font=("Courier", 12),
            justify="center"
        )
        logo_label.pack(pady=10)

        ctk.CTkLabel(
            self.home_tab,
            text="Join the green revolution of decentralized computing — mine with purpose. Mine with Signum.",
            wraplength=600,
            justify="center",
        ).pack(pady=10)

        path_frame = ctk.CTkFrame(self.config_tab, fg_color="transparent")
        path_frame.pack(fill="x", padx=5, pady=5)
        ctk.CTkLabel(path_frame, text="Config Path:").pack(side="left")
        ctk.CTkEntry(path_frame, textvariable=self.config_path, width=400).pack(side="left", expand=True, fill="x")
        ctk.CTkButton(path_frame, text="Browse", command=self.browse_config).pack(side="left", padx=5)
        ctk.CTkButton(path_frame, text="Load", command=self.load_config).pack(side="left")
        ctk.CTkButton(path_frame, text="Save", command=self.save_config).pack(side="left")

        self.text = ctk.CTkTextbox(self.config_tab, width=800, height=300)
        self.text.pack(fill="both", expand=True, padx=5, pady=5)

        # Options tab - scrollable frame
        self.options_frame = ctk.CTkScrollableFrame(self.options_tab)
        self.options_frame.pack(fill="both", expand=True, padx=5, pady=5)
        self.option_widgets = {}

        save_opts = ctk.CTkButton(self.options_tab, text="Save Options", command=self.save_options)
        save_opts.pack(side="bottom", pady=5)

        # Metrics tab - comprehensive monitoring
        metrics_scroll = ctk.CTkScrollableFrame(self.metrics_tab)
        metrics_scroll.pack(fill="both", expand=True, padx=5, pady=5)

        # Miner Status Section
        status_frame = ctk.CTkFrame(metrics_scroll)
        status_frame.pack(fill="x", padx=5, pady=5)
        ctk.CTkLabel(status_frame, text="Miner Status", font=("Arial", 16, "bold")).pack(anchor="w", padx=5, pady=5)

        self.miner_status_label = ctk.CTkLabel(status_frame, text="Status: Not Running", anchor="w")
        self.miner_status_label.pack(anchor="w", padx=20)

        self.mining_round_label = ctk.CTkLabel(status_frame, text="Mining Round: N/A", anchor="w")
        self.mining_round_label.pack(anchor="w", padx=20)

        self.deadline_label = ctk.CTkLabel(status_frame, text="Best Deadline: N/A", anchor="w")
        self.deadline_label.pack(anchor="w", padx=20)

        # Disk Health Section
        disk_frame = ctk.CTkFrame(metrics_scroll)
        disk_frame.pack(fill="x", padx=5, pady=5)
        ctk.CTkLabel(disk_frame, text="Disk Health Monitor", font=("Arial", 16, "bold")).pack(anchor="w", padx=5, pady=5)

        self.disk_health_text = ctk.CTkTextbox(disk_frame, height=150, font=("Courier", 10))
        self.disk_health_text.pack(fill="both", expand=True, padx=5, pady=5)
        self.disk_health_text.insert("1.0", "Waiting for mining data...\n\nDisk health information will appear here when mining starts.")
        self.disk_health_text.configure(state="disabled")

        # Performance Metrics Section
        perf_frame = ctk.CTkFrame(metrics_scroll)
        perf_frame.pack(fill="x", padx=5, pady=5)
        ctk.CTkLabel(perf_frame, text="Performance Metrics", font=("Arial", 16, "bold")).pack(anchor="w", padx=5, pady=5)

        self.reads_completed_label = ctk.CTkLabel(perf_frame, text="Total Reads: 0", anchor="w")
        self.reads_completed_label.pack(anchor="w", padx=20)

        self.reads_failed_label = ctk.CTkLabel(perf_frame, text="Failed Reads: 0", anchor="w")
        self.reads_failed_label.pack(anchor="w", padx=20)

        self.avg_speed_label = ctk.CTkLabel(perf_frame, text="Avg Read Speed: N/A", anchor="w")
        self.avg_speed_label.pack(anchor="w", padx=20)

        # Network Status Section
        network_frame = ctk.CTkFrame(metrics_scroll)
        network_frame.pack(fill="x", padx=5, pady=5)
        ctk.CTkLabel(network_frame, text="Network Status", font=("Arial", 16, "bold")).pack(anchor="w", padx=5, pady=5)

        self.pool_status_label = ctk.CTkLabel(network_frame, text="Pool Connection: Unknown", anchor="w")
        self.pool_status_label.pack(anchor="w", padx=20)

        self.submissions_label = ctk.CTkLabel(network_frame, text="Successful Submissions: 0", anchor="w")
        self.submissions_label.pack(anchor="w", padx=20)

        self.network_errors_label = ctk.CTkLabel(network_frame, text="Network Errors: 0", anchor="w")
        self.network_errors_label.pack(anchor="w", padx=20)

        # Refresh button
        refresh_btn = ctk.CTkButton(self.metrics_tab, text="Refresh Metrics", command=self.update_metrics)
        refresh_btn.pack(side="bottom", pady=5)

        # Initialize metrics tracking
        self.metrics_data = {
            'total_reads': 0,
            'failed_reads': 0,
            'submissions': 0,
            'network_errors': 0,
            'disk_health': {},
            'current_round': 'N/A',
            'best_deadline': 'N/A'
        }

        self.log_text = ctk.CTkTextbox(self.logs_tab, width=800, height=300, state="disabled")
        self.log_text.pack(fill="both", expand=True, padx=5, pady=5)

        btn_frame = ctk.CTkFrame(master, fg_color="transparent")
        btn_frame.pack(fill="x", padx=5, pady=5)
        self.start_btn = ctk.CTkButton(btn_frame, text="Start Miner", command=self.start_miner)
        self.start_btn.pack(side="left")
        self.stop_btn = ctk.CTkButton(btn_frame, text="Stop Miner", command=self.stop_miner, state="disabled")
        self.stop_btn.pack(side="left", padx=5)
        ctk.CTkButton(btn_frame, text="Quit", command=master.quit).pack(side="right")

        self.status_var = tk.StringVar(value="Idle")
        status_bar = ctk.CTkLabel(master, textvariable=self.status_var, anchor="w")
        status_bar.pack(fill="x", side="bottom")

        self.process = None
        self.log_queue = queue.Queue()
        self.load_config()
        self.update_logs()
        self.update_metrics_periodic()

    def browse_config(self):
        path = filedialog.askopenfilename(initialfile=self.config_path.get())
        if path:
            self.config_path.set(path)
            self.load_config()

    def load_config(self):
        try:
            with open(self.config_path.get(), 'r') as f:
                data = f.read()
            self.text.delete('1.0', tk.END)
            self.text.insert(tk.END, data)
            self.config_data = yaml.safe_load(data) or {}
            self.populate_options()
        except OSError as e:
            messagebox.showerror("Error", f"Failed to load config: {e}")

    def save_config(self):
        try:
            with open(self.config_path.get(), 'w') as f:
                f.write(self.text.get('1.0', tk.END))
            messagebox.showinfo("Saved", "Config saved")
            self.config_data = yaml.safe_load(self.text.get('1.0', tk.END)) or {}
            self.populate_options()
        except OSError as e:
            messagebox.showerror("Error", f"Failed to save config: {e}")

    def start_miner(self):
        # The UI expects the miner binary to be named "signum-miner". Ensure the
        # compiled binary uses this exact name or launching will fail.
        cmd = [os.path.join('.', 'signum-miner'), '-c', self.config_path.get()]
        try:
            self.process = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
            self.start_btn.configure(state="disabled")
            self.stop_btn.configure(state="normal")
            self.status_var.set("Mining...")
            threading.Thread(target=self.enqueue_output, daemon=True).start()
        except OSError as e:
            messagebox.showerror("Error", f"Failed to start miner: {e}")
            self.process = None

    def stop_miner(self):
        if self.process:
            self.process.terminate()
            self.process.wait()
            self.process = None
        self.start_btn.configure(state="normal")
        self.stop_btn.configure(state="disabled")
        self.status_var.set("Stopped")

    def enqueue_output(self):
        assert self.process and self.process.stdout
        for line in self.process.stdout:
            self.log_queue.put(line)
        self.process = None
        self.start_btn.configure(state="normal")
        self.stop_btn.configure(state="disabled")
        self.status_var.set("Stopped")

    def update_logs(self):
        while not self.log_queue.empty():
            line = self.log_queue.get_nowait()
            self.log_text.configure(state="normal")
            self.log_text.insert(tk.END, line)
            self.log_text.see(tk.END)
            self.log_text.configure(state="disabled")

            # Parse log line for metrics
            self.parse_log_for_metrics(line)

        self.master.after(100, self.update_logs)

    def parse_log_for_metrics(self, line):
        """Extract metrics information from log lines"""
        line_lower = line.lower()

        # Track mining rounds
        if "new mining round" in line_lower or "height" in line_lower:
            # Extract round/height information
            if "height" in line_lower:
                try:
                    parts = line.split("height")
                    if len(parts) > 1:
                        height = parts[1].strip().split()[0].replace(':', '').replace(',', '')
                        self.metrics_data['current_round'] = height
                except:
                    pass

        # Track deadlines
        if "deadline" in line_lower and "best" in line_lower:
            try:
                # Extract deadline value
                parts = line.split("deadline")
                if len(parts) > 1:
                    deadline_part = parts[1].strip().split()[0].replace(':', '').replace(',', '')
                    self.metrics_data['best_deadline'] = deadline_part
            except:
                pass

        # Track submissions
        if "submitted" in line_lower or "submission" in line_lower:
            if "success" in line_lower or "accepted" in line_lower:
                self.metrics_data['submissions'] += 1

        # Track network errors
        if "error" in line_lower and ("network" in line_lower or "connection" in line_lower or "timeout" in line_lower):
            self.metrics_data['network_errors'] += 1

        # Track disk operations
        if "finished" in line_lower and "speed" in line_lower:
            self.metrics_data['total_reads'] += 1

        if "failed" in line_lower and ("read" in line_lower or "disk" in line_lower or "i/o" in line_lower):
            self.metrics_data['failed_reads'] += 1

        # Track disk health
        if "health" in line_lower:
            if "healthy" in line_lower or "✓" in line:
                # Store health information
                self.extract_disk_health(line)
            elif "warning" in line_lower or "⚠" in line:
                self.extract_disk_health(line)
            elif "critical" in line_lower or "✗" in line:
                self.extract_disk_health(line)

    def extract_disk_health(self, line):
        """Extract disk health information from health summary lines"""
        # Store the health line for display
        if 'health_lines' not in self.metrics_data:
            self.metrics_data['health_lines'] = []

        # Keep last 20 health-related lines
        self.metrics_data['health_lines'].append(line)
        if len(self.metrics_data['health_lines']) > 20:
            self.metrics_data['health_lines'] = self.metrics_data['health_lines'][-20:]

    def update_metrics(self):
        """Update the metrics display with current data"""
        # Update status
        if self.process and self.process.poll() is None:
            self.miner_status_label.configure(text="Status: Mining ✓", text_color="green")
        else:
            self.miner_status_label.configure(text="Status: Not Running", text_color="gray")

        # Update mining info
        self.mining_round_label.configure(text=f"Mining Round: {self.metrics_data.get('current_round', 'N/A')}")
        self.deadline_label.configure(text=f"Best Deadline: {self.metrics_data.get('best_deadline', 'N/A')}")

        # Update performance metrics
        total_reads = self.metrics_data.get('total_reads', 0)
        failed_reads = self.metrics_data.get('failed_reads', 0)
        self.reads_completed_label.configure(text=f"Total Reads: {total_reads}")

        if failed_reads > 0:
            self.reads_failed_label.configure(text=f"Failed Reads: {failed_reads}", text_color="red")
        else:
            self.reads_failed_label.configure(text=f"Failed Reads: {failed_reads}", text_color="green")

        # Calculate error rate
        if total_reads > 0:
            error_rate = (failed_reads / total_reads) * 100
            if error_rate > 5:
                health_status = "CRITICAL"
                color = "red"
            elif error_rate > 1:
                health_status = "WARNING"
                color="orange"
            else:
                health_status = "HEALTHY"
                color = "green"
            self.avg_speed_label.configure(text=f"Overall Health: {health_status} ({error_rate:.2f}% error rate)", text_color=color)
        else:
            self.avg_speed_label.configure(text="Avg Read Speed: N/A")

        # Update network status
        submissions = self.metrics_data.get('submissions', 0)
        network_errors = self.metrics_data.get('network_errors', 0)

        if submissions > 0 or (self.process and self.process.poll() is None):
            self.pool_status_label.configure(text="Pool Connection: Connected ✓", text_color="green")
        else:
            self.pool_status_label.configure(text="Pool Connection: Unknown", text_color="gray")

        self.submissions_label.configure(text=f"Successful Submissions: {submissions}")

        if network_errors > 0:
            self.network_errors_label.configure(text=f"Network Errors: {network_errors}", text_color="orange")
        else:
            self.network_errors_label.configure(text=f"Network Errors: {network_errors}", text_color="green")

        # Update disk health display
        self.disk_health_text.configure(state="normal")
        self.disk_health_text.delete("1.0", tk.END)

        if 'health_lines' in self.metrics_data and self.metrics_data['health_lines']:
            self.disk_health_text.insert("1.0", "Recent Disk Health Reports:\n" + "="*60 + "\n\n")
            for health_line in self.metrics_data['health_lines'][-10:]:
                self.disk_health_text.insert(tk.END, health_line)
        else:
            if self.process and self.process.poll() is None:
                self.disk_health_text.insert("1.0", "Mining in progress...\n\nDisk health information will appear when health checks run.\n\nTo enable detailed disk stats, set 'show_drive_stats: true' in config.")
            else:
                self.disk_health_text.insert("1.0", "Waiting for mining data...\n\nStart the miner to see disk health information.")

        self.disk_health_text.configure(state="disabled")

    def update_metrics_periodic(self):
        """Periodically update metrics display"""
        self.update_metrics()
        self.master.after(2000, self.update_metrics_periodic)  # Update every 2 seconds

    def populate_options(self):
        for w in self.options_frame.winfo_children():
            w.destroy()
        self.option_widgets.clear()
        for row, (key, val) in enumerate(self.config_data.items()):
            ctk.CTkLabel(self.options_frame, text=key).grid(row=row, column=0, sticky="w", padx=5, pady=2)
            if isinstance(val, bool):
                var = tk.BooleanVar(value=val)
                cb = ctk.CTkCheckBox(self.options_frame, variable=var, text="")
                cb.grid(row=row, column=1, sticky="w")
                self.option_widgets[key] = ('bool', var)
            elif isinstance(val, (int, float, str)):
                var = tk.StringVar(value=str(val))
                entry = ctk.CTkEntry(self.options_frame, textvariable=var, width=200)
                entry.grid(row=row, column=1, sticky="we", padx=5)
                self.option_widgets[key] = ('scalar', var, type(val))
            else:
                txt = ctk.CTkTextbox(self.options_frame, height=70, width=200)
                txt.insert("1.0", yaml.dump(val))
                txt.grid(row=row, column=1, sticky="we", padx=5)
                self.option_widgets[key] = ('text', txt)

    def save_options(self):
        for key, widget_info in self.option_widgets.items():
            kind = widget_info[0]
            if kind == 'bool':
                var = widget_info[1]
                self.config_data[key] = bool(var.get())
            elif kind == 'scalar':
                var, typ = widget_info[1], widget_info[2]
                value = var.get()
                try:
                    if typ is int:
                        self.config_data[key] = int(value)
                    elif typ is float:
                        self.config_data[key] = float(value)
                    else:
                        self.config_data[key] = value
                except ValueError:
                    self.config_data[key] = value
            else:
                txt = widget_info[1]
                try:
                    self.config_data[key] = yaml.safe_load(txt.get('1.0', tk.END))
                except yaml.YAMLError:
                    self.config_data[key] = txt.get('1.0', tk.END)
        with open(self.config_path.get(), 'w') as f:
            yaml.dump(self.config_data, f, sort_keys=False)
        self.text.delete('1.0', tk.END)
        self.text.insert(tk.END, yaml.dump(self.config_data, sort_keys=False))
        messagebox.showinfo('Saved', 'Config saved')


def main():
    root = ctk.CTk()
    app = MinerUI(root)
    root.mainloop()

if __name__ == "__main__":
    main()
