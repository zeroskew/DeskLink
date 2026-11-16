//! DeskLink's relauncher. The update check and download run in DeskLink
//! (src/update.rs); this only swaps the exe. build.rs compiles this file on
//! its own (std only) and DeskLink embeds the result; when it exits with a
//! downloaded update, it writes it to %TEMP%\DeskLink\relauncher.exe and
//! starts it:
//!
//!     relauncher.exe <new exe> <target exe> [--relaunch [DeskLink args...]]
//!
//! It waits until nothing runs from the target exe (it stays locked for
//! writing while any DeskLink process does: the tray app, the settings
//! window, the cursor guard), swaps the new exe in (the old one is restored
//! if the copy fails), and optionally starts DeskLink again. What happened
//! goes to relauncher.log next to it.

#![windows_subsystem = "windows"]

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn log(msg: &str) {
    let path = std::env::current_exe().map(|p| p.with_file_name("relauncher.log")).unwrap_or_else(|_| PathBuf::from("relauncher.log"));
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = writeln!(f, "[{secs}] {msg}");
    }
}

/// Waits until nothing runs the target (it opens for writing), up to `limit`.
fn wait_unlocked(target: &Path, limit: Duration) -> bool {
    let start = Instant::now();
    loop {
        if fs::OpenOptions::new().write(true).open(target).is_ok() {
            return true;
        }
        if start.elapsed() > limit {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn replace(new: &Path, target: &Path) -> Result<(), String> {
    if !new.is_file() {
        return Err(format!("{} not found", new.display()));
    }
    // Only the exe DeskLink ran from (it passes its own path) is replaced;
    // none is ever created.
    if !target.is_file() {
        return Err(format!("{} not found", target.display()));
    }
    if !wait_unlocked(target, Duration::from_secs(60)) {
        return Err(format!("{} is still in use after 60 s", target.display()));
    }
    let mut backup = target.as_os_str().to_owned();
    backup.push(".old");
    let backup = PathBuf::from(backup);
    let _ = fs::remove_file(&backup);
    fs::rename(target, &backup).map_err(|e| format!("couldn't move the old exe aside: {e}"))?;
    if let Err(e) = fs::copy(new, target) {
        let _ = fs::remove_file(target);
        let _ = fs::rename(&backup, target);
        return Err(format!("couldn't copy the new exe: {e}"));
    }
    let _ = fs::remove_file(&backup);
    let _ = fs::remove_file(new);
    Ok(())
}

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.len() < 2 {
        log("usage: relauncher.exe <new exe> <target exe> [--relaunch [args...]]");
        return;
    }
    let (new, target) = (PathBuf::from(&args[0]), PathBuf::from(&args[1]));
    let relaunch = args.get(2).is_some_and(|a| a == "--relaunch");
    match replace(&new, &target) {
        Ok(()) => log(&format!("updated {}", target.display())),
        Err(e) => log(&format!("update failed: {e}")),
    }
    if relaunch {
        let rest = args.get(3..).unwrap_or(&[]);
        if let Err(e) = std::process::Command::new(&target).args(rest).spawn() {
            log(&format!("couldn't start {}: {e}", target.display()));
        }
    }
}
