//! The control panel: settings, start/stop, live status and quick audio actions.
//!
//! The receiver runs as a supervised child process (`kplay --headless`). That
//! makes "apply settings" a clean restart, keeps a crash in the receiver from
//! taking the panel down, and lets the panel show its log.

use crate::{
    audio,
    config::{Config, Settings},
    proc::{self, Child},
};
use anyhow::{anyhow, Result};
use eframe::egui::{self, Color32, RichText};
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Write},
    process::{ChildStdin, Stdio},
    sync::{
        atomic::{AtomicIsize, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

const ACCENT: Color32 = Color32::from_rgb(10, 132, 255);
const GOOD: Color32 = Color32::from_rgb(48, 209, 88);
const BAD: Color32 = Color32::from_rgb(255, 69, 58);
const WARN: Color32 = Color32::from_rgb(255, 214, 10);
const PRIMARY_TEXT: Color32 = Color32::from_gray(235);
const MUTED_TEXT: Color32 = Color32::from_gray(150);
const MAX_LOG: usize = 400;

pub fn run(verbose: bool) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("KPlay")
            .with_icon(egui::IconData { rgba: crate::icon::rgba(256), width: 256, height: 256 })
            .with_inner_size([440.0, 660.0])
            .with_min_inner_size([400.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "KPlay",
        options,
        Box::new(move |cc| {
            style(&cc.egui_ctx);
            let hwnd = Arc::new(AtomicIsize::new(0));
            #[cfg(windows)]
            if let Ok(h) = cc.window_handle() {
                if let RawWindowHandle::Win32(w) = h.as_raw() {
                    hwnd.store(w.hwnd.get(), Ordering::Relaxed);
                }
            }
            Ok(Box::new(Panel::new(cc.egui_ctx.clone(), verbose, hwnd)))
        }),
    )
    .map_err(|e| anyhow!("{e}"))
}

#[cfg(windows)]
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// Show the panel again straight through Win32: a hidden window is never asked
/// to repaint, so the UI loop can't be the one to wake it up.
fn show_native(hwnd: &AtomicIsize) {
    #[cfg(windows)]
    {
        unsafe extern "system" {
            fn ShowWindow(h: isize, cmd: i32) -> i32;
            fn SetForegroundWindow(h: isize) -> i32;
        }
        let h = hwnd.load(Ordering::Relaxed);
        if h != 0 {
            unsafe {
                ShowWindow(h, 9); // SW_RESTORE
                SetForegroundWindow(h);
            }
        }
    }
    #[cfg(not(windows))]
    let _ = hwnd;
}

fn style(ctx: &egui::Context) {
    // The panel uses a fixed dark background. Lock the matching theme so a
    // later OS theme event cannot switch its labels to light-theme black.
    ctx.set_theme(egui::Theme::Dark);
    let mut visuals = egui::Visuals::dark();
    visuals.override_text_color = Some(PRIMARY_TEXT);
    ctx.set_visuals(visuals);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 8.0);
        s.spacing.button_padding = egui::vec2(12.0, 6.0);
        s.spacing.interact_size.y = 28.0;
        s.visuals.selection.bg_fill = ACCENT;
        s.visuals.widgets.noninteractive.rounding = 8.0.into();
        s.visuals.widgets.inactive.rounding = 6.0.into();
        s.visuals.widgets.hovered.rounding = 6.0.into();
        s.visuals.widgets.active.rounding = 6.0.into();
        s.visuals.window_rounding = 10.0.into();
    });
}

enum Msg {
    /// stderr line (log)
    Log(String),
    /// `EVT ...` line from the receiver
    Event(String),
}

#[derive(Clone, PartialEq)]
enum Status {
    Stopped,
    Starting,
    Waiting(u16),
    Connected(String),
    Failed(String),
}

struct Running {
    child: Child,
    stdin: ChildStdin,
    /// Set once a stop was requested; used to force-kill if it lingers.
    stopping: Option<Instant>,
}

struct Panel {
    ctx: egui::Context,
    verbose: bool,
    saved: Config,
    edit: Config,
    devices: Vec<String>,
    ffmpeg: std::result::Result<String, String>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    running: Option<Running>,
    status: Status,
    muted: bool,
    log: VecDeque<String>,
    notice: Option<(String, Instant)>,
    restart_after_stop: bool,
    /// The panel is tucked away while a phone is mirroring.
    hidden: bool,
    /// Native window handle (Windows) so other threads can re-show a hidden panel.
    hwnd: Arc<AtomicIsize>,
}

impl Panel {
    fn new(ctx: egui::Context, verbose: bool, hwnd: Arc<AtomicIsize>) -> Self {
        let (cfg, load_err) = match Config::load(None) {
            Ok(c) => (c, None),
            Err(e) => {
                let mut c = Config::load(None).unwrap_or_else(|_| unreachable_cfg());
                c.settings = Settings::default();
                (c, Some(format!("Could not read settings: {e:#}")))
            }
        };
        let (tx, rx) = mpsc::channel();
        let mut p = Panel {
            ctx,
            verbose,
            edit: cfg.clone(),
            saved: cfg,
            devices: audio::list_output_devices(),
            ffmpeg: Err("checking...".into()),
            tx,
            rx,
            running: None,
            status: Status::Stopped,
            muted: false,
            log: VecDeque::new(),
            notice: load_err.map(|e| (e, Instant::now())),
            restart_after_stop: false,
            hidden: false,
            hwnd,
        };
        p.check_ffmpeg();
        if p.saved.settings.auto_start && p.ffmpeg.is_ok() {
            p.start();
        }
        p
    }

    fn check_ffmpeg(&mut self) {
        let path = self.edit.settings.ffmpeg.trim();
        let candidate = if path.is_empty() { proc::default_ffmpeg() } else { std::path::PathBuf::from(path) };
        self.ffmpeg = proc::ffmpeg_version(&candidate)
            .map(|v| v.replace("ffmpeg version ", "ffmpeg ").split(" Copyright").next().unwrap_or("ffmpeg").to_string())
            .ok_or_else(|| {
                if path.is_empty() {
                    "ffmpeg not found beside KPlay or on PATH".to_string()
                } else {
                    "this file does not run as ffmpeg".to_string()
                }
            });
    }

    fn validation(&self) -> Option<String> {
        if self.edit.name.trim().is_empty() {
            return Some("Give the device a name.".into());
        }
        if self.edit.settings.port < 1024 {
            return Some("Port must be 1024 or higher.".into());
        }
        None
    }

    fn dirty(&self) -> bool {
        self.edit != self.saved
    }

    fn is_running(&self) -> bool {
        self.running.is_some()
    }

    fn say(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), Instant::now()));
    }

    // ------------------------------------------------------------ process control

    fn save(&mut self) -> bool {
        self.edit.name = self.edit.name.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_string();
        match self.edit.save() {
            Ok(()) => {
                self.saved = self.edit.clone();
                true
            }
            Err(e) => {
                self.say(format!("Could not save settings: {e:#}"));
                false
            }
        }
    }

    fn start(&mut self) {
        if self.is_running() {
            return;
        }
        if self.validation().is_some() || !self.save() {
            return;
        }
        self.check_ffmpeg();
        if let Err(e) = &self.ffmpeg {
            self.status = Status::Failed(e.clone());
            return;
        }
        match self.spawn() {
            Ok(r) => {
                self.running = Some(r);
                self.status = Status::Starting;
                self.muted = self.saved.settings.start_muted;
            }
            Err(e) => self.status = Status::Failed(format!("{e:#}")),
        }
    }

    fn spawn(&self) -> Result<Running> {
        let mut cmd = proc::command(std::env::current_exe()?);
        cmd.arg("--headless");
        if self.verbose {
            cmd.arg("-v");
        }
        cmd.env("KPLAY_IPC", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = Child::new(cmd.spawn()?);
        let stdin = child.0.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.0.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let stderr = child.0.stderr.take().ok_or_else(|| anyhow!("no stderr"))?;

        let pump = |reader: Box<dyn std::io::Read + Send>, tx: Sender<Msg>, ctx: egui::Context, hwnd: Arc<AtomicIsize>| {
            thread::spawn(move || {
                for line in BufReader::new(reader).lines().map_while(std::result::Result::ok) {
                    let msg = match line.strip_prefix("EVT ") {
                        Some(e) => {
                            // A hidden window gets no UI updates, so bring it back from here.
                            if e == "disconnected" {
                                show_native(&hwnd);
                            }
                            Msg::Event(e.to_string())
                        }
                        None => Msg::Log(line),
                    };
                    if tx.send(msg).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
                // pipe closed: the receiver exited (crash, stop, ...)
                show_native(&hwnd);
                ctx.request_repaint();
            });
        };
        pump(Box::new(stdout), self.tx.clone(), self.ctx.clone(), self.hwnd.clone());
        pump(Box::new(stderr), self.tx.clone(), self.ctx.clone(), self.hwnd.clone());
        Ok(Running { child, stdin, stopping: None })
    }

    fn command(&mut self, line: &str) {
        if let Some(r) = self.running.as_mut() {
            if writeln!(r.stdin, "{line}").and_then(|_| r.stdin.flush()).is_err() {
                log::debug!("receiver is not accepting commands");
            }
        }
    }

    fn stop(&mut self) {
        self.command("quit");
        if let Some(r) = self.running.as_mut() {
            r.stopping.get_or_insert_with(Instant::now);
        }
    }

    /// Stop (if running), then start again with whatever is saved.
    fn restart(&mut self) {
        if self.is_running() {
            self.restart_after_stop = true;
            self.stop();
        } else {
            self.start();
        }
    }

    /// Drain messages and notice a receiver that exited.
    fn pump(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Log(l) => {
                    if self.log.len() >= MAX_LOG {
                        self.log.pop_front();
                    }
                    self.log.push_back(l);
                }
                Msg::Event(e) => {
                    let (kind, rest) = e.split_once(' ').unwrap_or((e.as_str(), ""));
                    match kind {
                        "ready" => self.status = Status::Waiting(rest.parse().unwrap_or(0)),
                        "connected" => self.status = Status::Connected(rest.to_string()),
                        "disconnected" => {
                            if let Status::Connected(_) = self.status {
                                let port = self.saved.settings.port;
                                self.status = Status::Waiting(port);
                            }
                        }
                        "muted" => self.muted = rest == "1",
                        _ => {}
                    }
                }
            }
        }

        let Some(r) = self.running.as_mut() else { return };
        if r.stopping.is_some_and(|t| t.elapsed() > Duration::from_secs(3)) {
            r.child.kill();
        }
        let exited = r.child.0.try_wait().ok().flatten();
        if let Some(code) = exited {
            let requested = r.stopping.is_some();
            self.running = None;
            if requested {
                self.status = Status::Stopped;
                if std::mem::take(&mut self.restart_after_stop) {
                    self.start();
                }
            } else {
                let reason = self
                    .log
                    .iter()
                    .rev()
                    .find(|l| l.starts_with("Error") || l.contains("ERROR"))
                    .cloned()
                    .unwrap_or_else(|| format!("The receiver exited unexpectedly ({code})."));
                self.status = Status::Failed(reason);
            }
        }
    }

    // ------------------------------------------------------------ drawing

    fn header(&mut self, ui: &mut egui::Ui) {
        let (color, title, sub) = match &self.status {
            Status::Stopped => (MUTED_TEXT, "Stopped".to_string(), "The receiver is not running.".to_string()),
            Status::Starting => (WARN, "Starting...".into(), "Bringing up the receiver.".into()),
            Status::Waiting(p) => (
                GOOD,
                "Ready".into(),
                format!("Pick \"{}\" from Screen Mirroring on your iPhone (port {p}).", self.saved.name),
            ),
            Status::Connected(d) => (ACCENT, "Mirroring".into(), format!("{d} is connected.")),
            Status::Failed(e) => (BAD, "Problem".into(), e.clone()),
        };
        let stopping = self.running.as_ref().is_some_and(|r| r.stopping.is_some());

        egui::Frame::none()
            .fill(Color32::from_gray(28))
            .rounding(10.0)
            .inner_margin(14.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 6.0, color);
                    ui.label(RichText::new(if stopping { "Stopping..." } else { &title }).size(20.0).strong());
                });
                ui.label(RichText::new(sub).color(MUTED_TEXT));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let running = self.is_running();
                    let ready = self.validation().is_none() && self.ffmpeg.is_ok();
                    let label = if running { "Stop" } else { "Start" };
                    let button = egui::Button::new(RichText::new(label).strong())
                        .fill(if running { Color32::from_gray(60) } else { ACCENT })
                        .min_size(egui::vec2(100.0, 32.0));
                    if ui.add_enabled(!stopping && (running || ready), button).clicked() {
                        if running {
                            self.stop();
                        } else {
                            self.start();
                        }
                    }
                    ui.add_enabled_ui(running && !stopping, |ui| {
                        let mute = if self.muted { "Unmute" } else { "Mute" };
                        if ui.button(mute).on_hover_text("Same as pressing M in the mirror window").clicked() {
                            self.command(if self.muted { "mute 0" } else { "mute 1" });
                            self.muted = !self.muted;
                        }
                        if ui
                            .button("Reset audio")
                            .on_hover_text("Restart the decoder and re-open the output device (R in the window)")
                            .clicked()
                        {
                            self.command("reset");
                            self.say("Audio reset.");
                        }
                    });
                });
            });
    }

    fn settings(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.label(RichText::new("Settings").size(16.0).strong());

        egui::Grid::new("settings").num_columns(2).spacing([12.0, 10.0]).show(ui, |ui| {
            ui.label("Device name");
            ui.add(
                egui::TextEdit::singleline(&mut self.edit.name)
                    .hint_text("Name shown on your iPhone")
                    .char_limit(60)
                    .desired_width(f32::INFINITY),
            );
            ui.end_row();

            ui.label("Port");
            ui.add(egui::DragValue::new(&mut self.edit.settings.port).range(1024..=65535).speed(1));
            ui.end_row();

            // The phone reads this when it connects, so it is pointless (and hidden) mid-session.
            if !matches!(self.status, Status::Connected(_)) {
                ui.label("Refresh rate");
                egui::ComboBox::from_id_salt("fps")
                    .selected_text(format!("{} Hz", self.edit.settings.fps))
                    .show_ui(ui, |ui| {
                        for f in [30u32, 60, 90, 120] {
                            ui.selectable_value(&mut self.edit.settings.fps, f, format!("{f} Hz"));
                        }
                    })
                    .response
                    .on_hover_text("Highest frame rate the phone may send. Applies to the next connection.");
                ui.end_row();
            }

            ui.label("Audio output");
            ui.horizontal(|ui| {
                let current = if self.edit.settings.audio_device.is_empty() {
                    "System default".to_string()
                } else {
                    self.edit.settings.audio_device.clone()
                };
                egui::ComboBox::from_id_salt("audio_device").selected_text(current).width(ui.available_width() - 40.0).show_ui(
                    ui,
                    |ui| {
                        ui.selectable_value(&mut self.edit.settings.audio_device, String::new(), "System default");
                        for d in &self.devices {
                            ui.selectable_value(&mut self.edit.settings.audio_device, d.clone(), d);
                        }
                    },
                );
                if ui.button("\u{21BB}").on_hover_text("Rescan audio devices").clicked() {
                    self.devices = audio::list_output_devices();
                }
            });
            ui.end_row();

            ui.label("ffmpeg");
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.edit.settings.ffmpeg)
                            .hint_text("auto (from PATH)")
                            .desired_width(ui.available_width() - 80.0),
                    );
                    if r.lost_focus() {
                        self.check_ffmpeg();
                    }
                    if ui.button("Browse").clicked() {
                        if let Some(p) = rfd::FileDialog::new().add_filter("ffmpeg", &["exe"]).pick_file() {
                            self.edit.settings.ffmpeg = p.display().to_string();
                            self.check_ffmpeg();
                        }
                    }
                });
                match &self.ffmpeg {
                    Ok(v) => ui.label(RichText::new(format!("\u{2714} {v}")).color(GOOD).small()),
                    Err(e) => ui.label(RichText::new(format!("\u{2716} {e}")).color(BAD).small()),
                };
            });
            ui.end_row();
        });

        ui.add_space(2.0);
        ui.checkbox(&mut self.edit.settings.start_muted, "Start with audio muted");
        ui.checkbox(&mut self.edit.settings.always_on_top, "Keep the mirror window on top");
        ui.checkbox(&mut self.edit.settings.auto_start, "Start the receiver when KPlay opens");

        ui.add_space(4.0);
        let problem = self.validation();
        if let Some(p) = &problem {
            ui.label(RichText::new(p).color(BAD));
        }
        ui.horizontal(|ui| {
            let dirty = self.dirty();
            let running = self.is_running();
            let label = if running { "Save & restart" } else { "Save" };
            if ui
                .add_enabled(dirty && problem.is_none(), egui::Button::new(RichText::new(label).strong()).fill(ACCENT))
                .clicked()
                && self.save()
            {
                if running {
                    self.restart();
                    self.say("Settings saved. Restarting the receiver...");
                } else {
                    self.say("Settings saved.");
                }
            }
            if ui.add_enabled(dirty, egui::Button::new("Revert")).clicked() {
                self.edit = self.saved.clone();
                self.check_ffmpeg();
            }
            if dirty {
                ui.label(RichText::new("Unsaved changes").color(WARN).small());
            }
        });
        ui.label(
            RichText::new("In the mirror window: scroll = resize, M = mute, R = reset audio, F = fullscreen, T = on top.")
                .color(MUTED_TEXT)
                .small(),
        );
    }

    fn log_view(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Log").default_open(false).show(ui, |ui| {
            if ui.small_button("Clear").clicked() {
                self.log.clear();
            }
            egui::ScrollArea::vertical().max_height(180.0).stick_to_bottom(true).auto_shrink([false, true]).show(ui, |ui| {
                if self.log.is_empty() {
                    ui.label(RichText::new("Nothing yet.").color(MUTED_TEXT));
                }
                for l in &self.log {
                    let color = if l.contains("ERROR") {
                        BAD
                    } else if l.contains("WARN") {
                        WARN
                    } else {
                        Color32::from_gray(190)
                    };
                    ui.label(RichText::new(l).monospace().small().color(color));
                }
            });
        });
    }
}

/// Last-resort config when even creating one on disk fails.
fn unreachable_cfg() -> Config {
    serde_json::from_str(r#"{"name":"KPlay","device_id":"02:00:00:00:00:01","key_seed":"00"}"#).expect("static json")
}

impl eframe::App for Panel {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump();
        // Get out of the way while mirroring; come back when the phone leaves or the receiver dies.
        let connected = matches!(self.status, Status::Connected(_));
        if connected != self.hidden {
            self.hidden = connected;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(!connected));
            if !connected {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }
        if self.is_running() {
            ctx.request_repaint_after(Duration::from_millis(400));
        }
        if self.notice.as_ref().is_some_and(|(_, t)| t.elapsed() > Duration::from_secs(5)) {
            self.notice = None;
        }

        egui::CentralPanel::default().frame(egui::Frame::none().fill(Color32::from_gray(20)).inner_margin(16.0)).show(
            ctx,
            |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("KPlay").size(24.0).strong());
                        ui.label(RichText::new("AirPlay receiver").color(MUTED_TEXT));
                    });
                    self.header(ui);
                    if let Some((n, _)) = &self.notice {
                        ui.label(RichText::new(n).color(ACCENT));
                    }
                    self.settings(ui);
                    ui.separator();
                    self.log_view(ui);
                });
            },
        );
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // never leave a receiver (and its mirror window) running behind us
        if let Some(r) = self.running.as_mut() {
            r.child.kill();
        }
    }
}
