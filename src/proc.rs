//! Helpers for the ffmpeg child processes.

use anyhow::{bail, Result};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

/// A child process that is killed (and reaped) when dropped.
pub struct Child(pub std::process::Child);

impl Child {
    pub fn new(c: std::process::Child) -> Self {
        Child(c)
    }
    pub fn kill(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        self.kill();
    }
}

/// A `Command` that never flashes a console window on Windows.
pub fn command(program: impl AsRef<Path>) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program.as_ref());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

/// First line of fmpeg -version (e.g. "ffmpeg version 8.1.2"), if it runs.
pub fn ffmpeg_version(candidate: &Path) -> Option<String> {
    let out = command(candidate).arg("-version").output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("ffmpeg").to_string())
}

/// Default ffmpeg location: bundled beside this executable, then `ffmpeg` on PATH.
pub fn default_ffmpeg() -> PathBuf {
    #[cfg(windows)]
    if let Ok(exe) = std::env::current_exe() {
        let bundled = exe.with_file_name("ffmpeg.exe");
        if bundled.is_file() {
            return bundled;
        }
    }
    PathBuf::from("ffmpeg")
}

/// Locate a working ffmpeg: the explicit path if given, otherwise bundled/PATH.
pub fn find_ffmpeg(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let candidate = explicit.unwrap_or_else(default_ffmpeg);
    match command(&candidate).arg("-version").output() {
        Ok(o) if o.status.success() => Ok(candidate),
        _ => bail!(
            "ffmpeg not found ({}). Install it (e.g. `winget install Gyan.FFmpeg`) or pass --ffmpeg <path>",
            candidate.display()
        ),
    }
}
