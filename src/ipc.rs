//! Tiny line protocol between the control panel (parent) and the receiver
//! (child process). The child only speaks it when `KPLAY_IPC` is set.
//!
//! child -> panel (stdout): `EVT ready <port>`, `EVT connected <name>`,
//!                          `EVT disconnected`, `EVT muted <0|1>`
//! panel -> child (stdin):  `mute <0|1>`, `reset`, `quit` (or closing stdin)

use crate::{audio::AudioHandle, display::UiLink};
use std::{
    io::{BufRead, Write},
    thread,
};

pub fn enabled() -> bool {
    std::env::var_os("KPLAY_IPC").is_some()
}

pub fn emit(line: &str) {
    if enabled() {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "EVT {line}");
        let _ = out.flush();
    }
}

pub fn spawn_command_reader(audio: AudioHandle, ui: UiLink) {
    if !enabled() {
        return;
    }
    let _ = thread::Builder::new().name("ipc".into()).spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            match line.trim() {
                "mute 1" => audio.set_muted(true),
                "mute 0" => audio.set_muted(false),
                "reset" => audio.reset(),
                "quit" => break,
                _ => continue,
            }
            ui.refresh();
            emit(&format!("muted {}", audio.is_muted() as u8));
        }
        // "quit", or the panel went away (stdin closed): shut down cleanly
        ui.quit();
        thread::sleep(std::time::Duration::from_secs(3));
        std::process::exit(0);
    });
}
