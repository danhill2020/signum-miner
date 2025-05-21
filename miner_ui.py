import os
import subprocess
import threading
import queue
import tkinter as tk
from tkinter import ttk, filedialog, scrolledtext, messagebox

class MinerUI:
    def __init__(self, master: tk.Tk):
        self.master = master
        self.master.title("Signum Miner")

        self.process = None
        self.log_queue: queue.Queue[str] = queue.Queue()

        self.config_path = tk.StringVar(value="config.yaml")

        # Notebook with two tabs
        self.notebook = ttk.Notebook(master)
        self.cfg_frame = ttk.Frame(self.notebook)
        self.log_frame = ttk.Frame(self.notebook)
        self.notebook.add(self.cfg_frame, text="Configuration")
        self.notebook.add(self.log_frame, text="Logs")
        self.notebook.pack(fill=tk.BOTH, expand=True)

        self._build_config_tab()
        self._build_log_tab()
        self._build_controls()

        self.update_logs()

    def _build_config_tab(self) -> None:
        path_frame = ttk.Frame(self.cfg_frame)
        path_frame.pack(fill=tk.X, padx=5, pady=5)
        ttk.Label(path_frame, text="Config Path:").pack(side=tk.LEFT)
        ttk.Entry(path_frame, textvariable=self.config_path, width=50).pack(side=tk.LEFT, expand=True, fill=tk.X)
        ttk.Button(path_frame, text="Browse", command=self.browse_config).pack(side=tk.LEFT, padx=5)
        ttk.Button(path_frame, text="Load", command=self.load_config).pack(side=tk.LEFT)
        ttk.Button(path_frame, text="Save", command=self.save_config).pack(side=tk.LEFT)

        self.text = scrolledtext.ScrolledText(self.cfg_frame, width=80, height=20)
        self.text.pack(fill=tk.BOTH, expand=True, padx=5, pady=5)

    def _build_log_tab(self) -> None:
        self.log_text = scrolledtext.ScrolledText(self.log_frame, width=80, height=25, state=tk.DISABLED)
        self.log_text.pack(fill=tk.BOTH, expand=True, padx=5, pady=5)

    def _build_controls(self) -> None:
        btn_frame = ttk.Frame(self.master)
        btn_frame.pack(fill=tk.X, padx=5, pady=5)
        self.run_btn = ttk.Button(btn_frame, text="Start Miner", command=self.toggle_miner)
        self.run_btn.pack(side=tk.LEFT)
        self.status = ttk.Label(btn_frame, text="Idle")
        self.status.pack(side=tk.LEFT, padx=10)
        ttk.Button(btn_frame, text="Quit", command=self.master.quit).pack(side=tk.RIGHT)

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
        except OSError as e:
            messagebox.showerror("Error", f"Failed to load config: {e}")

    def save_config(self):
        try:
            with open(self.config_path.get(), 'w') as f:
                f.write(self.text.get('1.0', tk.END))
            messagebox.showinfo("Saved", "Config saved")
        except OSError as e:
            messagebox.showerror("Error", f"Failed to save config: {e}")

    def toggle_miner(self):
        if self.process:
            self.stop_miner()
        else:
            self.start_miner()

    def start_miner(self) -> None:
        cmd = [os.path.join('.', 'signum-miner'), '-c', self.config_path.get()]
        try:
            self.process = subprocess.Popen(
                cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True
            )
            self.run_btn.config(text="Stop Miner")
            self.status.config(text="Running")
            threading.Thread(target=self.enqueue_output, daemon=True).start()
        except OSError as e:
            messagebox.showerror("Error", f"Failed to start miner: {e}")
            self.process = None

    def stop_miner(self) -> None:
        if self.process:
            self.process.terminate()
            self.process.wait()
            self.on_process_exit()

    def enqueue_output(self) -> None:
        assert self.process and self.process.stdout
        for line in self.process.stdout:
            self.log_queue.put(line)
        self.on_process_exit()

    def on_process_exit(self) -> None:
        self.process = None
        self.run_btn.config(text="Start Miner")
        self.status.config(text="Idle")

    def update_logs(self):
        while not self.log_queue.empty():
            line = self.log_queue.get_nowait()
            self.log_text.configure(state=tk.NORMAL)
            self.log_text.insert(tk.END, line)
            self.log_text.see(tk.END)
            self.log_text.configure(state=tk.DISABLED)
        self.master.after(100, self.update_logs)


def main():
    root = tk.Tk()
    app = MinerUI(root)
    root.mainloop()

if __name__ == "__main__":
    main()
