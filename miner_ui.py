import os
import subprocess
import threading
import queue
import tkinter as tk
from tkinter import filedialog, messagebox
import customtkinter as ctk
import yaml
from PIL import Image

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
        self.logs_tab = self.tabview.add("Logs")

        # Home tab content
        logo_path = os.path.join(os.path.dirname(__file__), "signum_logo.png")
        if os.path.exists(logo_path):
            image = Image.open(logo_path)
            self.logo_image = ctk.CTkImage(light_image=image, dark_image=image)
            ctk.CTkLabel(self.home_tab, image=self.logo_image, text="").pack(pady=10)
        ctk.CTkLabel(
            self.home_tab,
            text="Signum — The sustainable blockchain",
            font=("Arial", 20),
        ).pack(pady=(0, 10))
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
        self.master.after(100, self.update_logs)

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
