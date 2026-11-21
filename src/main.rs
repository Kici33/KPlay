//! KPlay - a small AirPlay screen-mirroring receiver.
//!
//! * `kplay`            opens the control panel (settings, start/stop, status)
//! * `kplay --headless` runs the receiver itself; the panel launches this as a child

#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

mod audio;
mod config;
mod demo;
mod discovery;
mod display;
mod fairplay;
mod gui;
mod h264;
mod icon;
mod ipc;
mod pairing;
mod proc;
mod rtp;
mod server;
mod video;

use anyhow::Result;
use clap::Parser;
use std::{path::PathBuf, thread};

#[derive(Parser)]
#[command(version, about = "AirPlay screen-mirroring receiver for iPhone/iPad")]
struct Args {
    /// Run the receiver without the control panel (logs to the console)
    #[arg(long)]
    headless: bool,

    /// Name shown in the iOS Screen Mirroring list (saved)
    #[arg(short, long)]
    name: Option<String>,

    /// TCP port for the AirPlay control channel (default: from settings)
    #[arg(short, long)]
    port: Option<u16>,

    /// Path to ffmpeg (default: from settings, else the one on PATH)
    #[arg(long, value_name = "PATH")]
    ffmpeg: Option<PathBuf>,

    /// Start with audio muted
    #[arg(long)]
    muted: bool,

    /// Show an animated test pattern (try the window controls without a phone)
    #[arg(long, hide = true)]
    demo: bool,

    /// Verbose logging
    #[arg(short, long)]
    verbose: bool,
}

/// A windows-subsystem build starts without a console. When launched from a
/// terminal, hook up to it so `--headless` / `--help` output is visible.
#[cfg(all(not(debug_assertions), windows))]
fn attach_parent_console() {
    use std::os::windows::io::IntoRawHandle;
    unsafe extern "system" {
        fn AttachConsole(pid: u32) -> i32;
        fn SetStdHandle(which: u32, handle: *mut core::ffi::c_void) -> i32;
    }
    unsafe {
        if AttachConsole(u32::MAX) != 0 {
            if let Ok(f) = std::fs::OpenOptions::new().write(true).open("CONOUT$") {
                let h = f.into_raw_handle();
                SetStdHandle(-11i32 as u32, h); // STD_OUTPUT_HANDLE
                SetStdHandle(-12i32 as u32, h); // STD_ERROR_HANDLE
            }
        }
    }
}
#[cfg(not(all(not(debug_assertions), windows)))]
fn attach_parent_console() {}

fn main() -> Result<()> {
    if std::env::args().any(|a| a == "--help" || a == "-h" || a == "--version" || a == "-V") && !ipc::enabled() {
        attach_parent_console();
    }
    let args = Args::parse();
    if !(args.headless || args.demo) {
        return gui::run(args.verbose);
    }
    if !ipc::enabled() {
        attach_parent_console();
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(if args.verbose {
        "kplay=debug"
    } else {
        "kplay=info"
    }))
    .format_timestamp(None)
    .init();

    let cfg = config::Config::load(args.name.as_deref())?;
    let key = cfg.signing_key();
    let st = cfg.settings.clone();

    let ffmpeg_pref = args.ffmpeg.or_else(|| (!st.ffmpeg.trim().is_empty()).then(|| PathBuf::from(st.ffmpeg.trim())));
    let ffmpeg = proc::find_ffmpeg(ffmpeg_pref)?;

    let (event_loop, ui) = display::create_event_loop()?;
    let audio = audio::AudioHandle::spawn(ffmpeg.clone(), args.muted || st.start_muted, st.audio_device.clone());
    ipc::spawn_command_reader(audio.clone(), ui.clone());

    let listener = server::bind(args.port.unwrap_or(st.port))?;
    let port = listener.local_addr()?.port();
    let ctx = server::Ctx::new(cfg.clone(), key.clone(), ffmpeg, audio.clone(), ui.clone());
    thread::Builder::new().name("listener".into()).spawn({
        let ctx = ctx.clone();
        move || server::serve(ctx, listener)
    })?;

    if args.demo {
        demo::spawn(ui.clone());
    }

    let _mdns = discovery::start(&cfg, &key, port)?;
    log::info!("\"{}\" is ready on port {port} - pick it from Screen Mirroring on your iPhone", cfg.name);
    log::info!("window: scroll = resize, M = mute, R = reset audio, F/double-click = fullscreen, T = on top");
    ipc::emit(&format!("muted {}", audio.is_muted() as u8));
    ipc::emit(&format!("ready {port}"));

    display::run(event_loop, ui, cfg.name, audio, st.always_on_top)
}
