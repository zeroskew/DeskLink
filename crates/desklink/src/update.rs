//! Self-update from GitHub Releases (the running DeskLink's side; the
//! settings window only shows the status and passes requests on).
//!
//! The build stamps a version (build.rs: UTC "YYYY.MM.DD-HHMM");
//! build_publish_exe.cmd tags the release "v<version>". A check asks the
//! GitHub API for the latest release; when its tag is newer, its
//! `desklink.zip` is downloaded into %LOCALAPPDATA%\DeskLink\update\ and the
//! exe unpacked there. It's installed when DeskLink exits from the tray, or
//! restarts for "Restart to update": [`apply_on_exit`] writes the embedded
//! relauncher (relauncher.rs) to %TEMP%\DeskLink and starts it; the
//! relauncher waits until no DeskLink runs from the exe, swaps it and, for
//! "Restart to update", starts it again.
//!
//! The status goes to the settings window through the "update" pipe, one
//! line per change (see [`Status::line`]).

use crossbeam_channel::{Receiver, Sender};
use desklink_config::UpdateSettings;
use desklink_win::http;
use desklink_win::tray::TrayHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const VERSION: &str = env!("DESKLINK_VERSION");
const REPO: &str = "zeroskew/DeskLink";
/// The release asset (build_publish_exe.cmd) and the exe inside it.
const ASSET: &str = "desklink.zip";
const EXE: &str = "desklink.exe";
const RELAUNCHER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/relauncher.exe"));
/// The first automatic check runs this long after DeskLink starts.
const FIRST_CHECK: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Idle,
    Checking,
    /// The latest release is this one (or older): its tag.
    UpToDate(String),
    Downloading { tag: String, done: u64, total: u64 },
    /// Downloaded and unpacked; installs when DeskLink exits from the tray.
    Ready(String),
    Failed(String),
    /// The settings window's own state: no DeskLink to ask.
    NotRunning,
}

impl Status {
    /// One line for the pipe: kind, then tab-separated values.
    pub fn line(&self) -> String {
        match self {
            Status::Idle => "idle".into(),
            Status::Checking => "checking".into(),
            Status::UpToDate(t) => format!("uptodate\t{t}"),
            Status::Downloading { tag, done, total } => format!("downloading\t{tag}\t{done}\t{total}"),
            Status::Ready(t) => format!("ready\t{t}"),
            Status::Failed(e) => format!("failed\t{}", e.replace(['\t', '\n', '\r'], " ")),
            Status::NotRunning => "notrunning".into(),
        }
    }

    pub fn parse(line: &str) -> Status {
        let mut f = line.split('\t');
        let kind = f.next().unwrap_or("");
        let mut s = || f.next().unwrap_or("").to_string();
        match kind {
            "checking" => Status::Checking,
            "uptodate" => Status::UpToDate(s()),
            "downloading" => {
                let tag = s();
                let done = s().parse().unwrap_or(0);
                let total = s().parse().unwrap_or(0);
                Status::Downloading { tag, done, total }
            }
            "ready" => Status::Ready(s()),
            "failed" => Status::Failed(s()),
            "notrunning" => Status::NotRunning,
            _ => Status::Idle,
        }
    }
}

static STATUS: Mutex<Status> = Mutex::new(Status::Idle);
/// Settings windows following the status.
static WATCHERS: Mutex<Vec<Sender<String>>> = Mutex::new(Vec::new());
/// The unpacked new exe, once ready.
static READY: Mutex<Option<PathBuf>> = Mutex::new(None);
static BUSY: AtomicBool = AtomicBool::new(false);
static SETTINGS: Mutex<Option<UpdateSettings>> = Mutex::new(None);
static TRAY: OnceLock<TrayHandle> = OnceLock::new();
/// The exe this process was started from: the only file an update replaces.
static LAUNCHED: OnceLock<PathBuf> = OnceLock::new();
/// What to do at exit: nothing, install, or install and start again.
static ON_EXIT: AtomicU8 = AtomicU8::new(0);
const INSTALL: u8 = 1;
const INSTALL_AND_RESTART: u8 = 2;

pub fn status() -> Status {
    STATUS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn set(s: Status) {
    let line = s.line();
    *STATUS.lock().unwrap_or_else(|e| e.into_inner()) = s;
    WATCHERS.lock().unwrap_or_else(|e| e.into_inner()).retain(|w| w.send(line.clone()).is_ok());
}

fn notify(title: &str, text: &str) {
    if let Some(t) = TRAY.get() {
        t.notify(title, text);
    }
}

/// %LOCALAPPDATA%\DeskLink\update: downloads.
fn update_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("DeskLink").join("update")
}

/// %TEMP%\DeskLink: the relauncher runs from here.
fn relauncher_dir() -> PathBuf {
    std::env::temp_dir().join("DeskLink")
}

/// A build run from a cargo `target` folder: no automatic checks (the tray's
/// "Check for updates" still works).
pub fn is_dev_build() -> bool {
    LAUNCHED.get().is_none_or(|p| p.components().any(|c| c.as_os_str().eq_ignore_ascii_case("target")))
}

/// "v2026.11.15-2342" -> 202611152342, for comparing versions.
fn version_number(v: &str) -> u64 {
    v.chars().filter(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0)
}

/// At start of the tray app: remember the exe, clear what earlier updates
/// left behind, serve the status to settings windows, run the automatic
/// checks and listen for the settings window's requests. Returns the
/// "install now" requests from the settings window.
pub fn start(tray: TrayHandle, settings: &UpdateSettings) -> Receiver<()> {
    if let Ok(exe) = std::env::current_exe() {
        let _ = LAUNCHED.set(exe);
    }
    let _ = TRAY.set(tray);
    configure(settings);
    log::info!("DeskLink version {VERSION}");

    let _ = std::thread::Builder::new().name("dl-update-clean".into()).spawn(|| {
        // The relauncher may still be finishing after starting us again.
        std::thread::sleep(Duration::from_secs(5));
        for (dir, keep) in [(relauncher_dir(), "relauncher"), (update_dir(), "")] {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_lowercase();
                let ours = if keep.is_empty() { n.ends_with(".exe") || n.ends_with(".part") } else { n.starts_with(keep) && n.ends_with(".exe") };
                if ours && READY.lock().unwrap_or_else(|e| e.into_inner()).as_deref() != Some(e.path().as_path()) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    });

    if let Err(e) = desklink_win::pipe::serve("update", |mut out| {
        use std::io::Write;
        let (tx, rx) = crossbeam_channel::unbounded();
        let current = status().line();
        WATCHERS.lock().unwrap_or_else(|e| e.into_inner()).push(tx);
        if writeln!(out, "{current}").and_then(|_| out.flush()).is_err() {
            return;
        }
        for line in rx {
            if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
                break;
            }
        }
    }) {
        log::warn!("update status for the settings window unavailable: {e}");
    }

    match desklink_win::instance::listen_for_update_check() {
        Ok(rx) => {
            let _ = std::thread::Builder::new().name("dl-update-req".into()).spawn(move || {
                for () in rx {
                    check(false);
                }
            });
        }
        Err(e) => log::warn!("{e}"),
    }

    let _ = std::thread::Builder::new().name("dl-update-auto".into()).spawn(|| {
        let started = Instant::now();
        let mut last: Option<Instant> = None;
        loop {
            std::thread::sleep(Duration::from_secs(15));
            let Some(s) = SETTINGS.lock().unwrap_or_else(|e| e.into_inner()).clone() else { continue };
            if !s.auto_check || is_dev_build() {
                continue;
            }
            let every = Duration::from_secs(s.check_every_hours as u64 * 3600);
            let due = match last {
                None => started.elapsed() >= FIRST_CHECK,
                Some(t) => s.check_every_hours > 0 && t.elapsed() >= every,
            };
            if due {
                last = Some(Instant::now());
                check(false);
            }
        }
    });

    desklink_win::instance::listen_for_update_apply().unwrap_or_else(|e| {
        log::warn!("{e}");
        crossbeam_channel::never()
    })
}

/// The `[updates]` settings (at start, and when they change).
pub fn configure(settings: &UpdateSettings) {
    *SETTINGS.lock().unwrap_or_else(|e| e.into_inner()) = Some(settings.clone());
}

pub fn is_ready() -> bool {
    READY.lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

/// The tray's "Check for updates": check, in a dialog that follows it live
/// and shows the result ("Restart now" once an update is downloaded). Notification
/// balloons aren't enough: Windows drops them when notifications are off.
pub fn check_with_dialog() {
    static OPEN: AtomicBool = AtomicBool::new(false);
    if OPEN.swap(true, Ordering::SeqCst) {
        return; // already showing
    }
    check(false);
    let _ = std::thread::Builder::new().name("dl-update-dialog".into()).spawn(|| {
        use desklink_win::ui::UpdateView;
        let restart = desklink_win::ui::update_dialog(VERSION, &mut || match status() {
            Status::Idle | Status::Checking | Status::NotRunning => UpdateView::Busy { text: "Checking GitHub for a new release…".into(), progress: None },
            Status::Downloading { tag, done, total } => UpdateView::Busy {
                text: if total > 0 { format!("Downloading {tag}: {} of {}…", mb(done), mb(total)) } else { format!("Downloading {tag}: {}…", mb(done)) },
                progress: Some((done, total)),
            },
            Status::UpToDate(tag) => UpdateView::Done { ok: true, main: "DeskLink is up to date".into(), text: format!("This is v{VERSION}; the latest release is {tag}.") },
            Status::Failed(e) => UpdateView::Done { ok: false, main: "Couldn't check for updates".into(), text: format!("{}.", capitalize(&e)) },
            Status::Ready(tag) => UpdateView::Ready {
                main: format!("DeskLink {tag} is downloaded"),
                text: "Restart now to install it: DeskLink exits, updates and starts again (a few seconds without sharing). Later: it installs when you exit DeskLink from the tray, or pick Restart to update in the tray menu.".into(),
            },
        });
        OPEN.store(false, Ordering::SeqCst);
        if restart {
            // The main loop picks this up like the settings window's button.
            desklink_win::instance::request_update_apply();
        }
    });
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// Check for (and download) a newer release on a thread. `tell` (the tray's
/// "Check for updates"): also say in a balloon when there's nothing new or
/// it failed. Nothing happens while a check runs or once an update is ready.
pub fn check(tell: bool) {
    if is_ready() {
        if tell {
            if let Status::Ready(tag) = status() {
                notify(&format!("DeskLink {tag} is downloaded"), "Right-click the tray icon and pick Restart to update.");
            }
        }
        return;
    }
    if BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    set(Status::Checking);
    let _ = std::thread::Builder::new().name("dl-update".into()).spawn(move || {
        let result = run_check();
        BUSY.store(false, Ordering::SeqCst);
        match result {
            Ok(Status::Ready(tag)) => {
                log::info!("update {tag} downloaded; it installs at the next restart");
                if let Some(t) = TRAY.get() {
                    t.set_update_ready(Some(tag.clone()));
                }
                notify(
                    &format!("DeskLink {tag} is downloaded"),
                    "Right-click the tray icon and pick Restart to update. It also installs when you exit DeskLink.",
                );
                set(Status::Ready(tag));
            }
            Ok(s) => {
                if let Status::UpToDate(tag) = &s {
                    log::info!("no update: the latest release is {tag}, this is v{VERSION}");
                    if tell {
                        notify("DeskLink is up to date", &format!("This is v{VERSION}; the latest release is {tag}."));
                    }
                }
                set(s);
            }
            Err(e) => {
                log::warn!("update check failed: {e}");
                if tell {
                    notify("Couldn't check for updates", &e);
                }
                set(Status::Failed(e));
            }
        }
    });
}

fn run_check() -> Result<Status, String> {
    if RELAUNCHER.is_empty() {
        return Err("this build has no relauncher (it couldn't be compiled)".into());
    }
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let body = match http::get(&url, "application/vnd.github+json", &mut |_, _| {}) {
        Ok(b) => b,
        Err(http::Error::Status(404)) => return Err("no release is published yet".into()),
        Err(http::Error::Status(403 | 429)) => return Err("GitHub's rate limit was hit; try again later".into()),
        Err(e) => return Err(e.to_string()),
    };
    let json: serde_json::Value = serde_json::from_slice(&body).map_err(|e| format!("unexpected reply from GitHub: {e}"))?;
    let tag = json["tag_name"].as_str().ok_or("the latest release has no tag")?.to_string();
    if version_number(&tag) <= version_number(VERSION) {
        return Ok(Status::UpToDate(tag));
    }
    let assets = json["assets"].as_array().cloned().unwrap_or_default();
    let asset = assets
        .iter()
        .find(|a| a["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(ASSET)))
        .ok_or_else(|| format!("release {tag} has no {ASSET}"))?;
    let dl = asset["browser_download_url"].as_str().ok_or("the release asset has no download link")?.to_string();
    let size = asset["size"].as_u64().unwrap_or(0);

    set(Status::Downloading { tag: tag.clone(), done: 0, total: size });
    let mut last = Instant::now();
    let data = http::get(&dl, "application/octet-stream", &mut |done, total| {
        if last.elapsed() > Duration::from_millis(200) {
            last = Instant::now();
            set(Status::Downloading { tag: tag.clone(), done, total: if total > 0 { total } else { size } });
        }
    })
    .map_err(|e| format!("download failed: {e}"))?;
    let exe = zip::extract(&data, EXE)?;
    if exe.len() < 256 << 10 || !exe.starts_with(b"MZ") {
        return Err("the downloaded file isn't a valid DeskLink executable".into());
    }
    let dir = update_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
    let path = dir.join(format!("desklink-{}.exe", tag.trim_start_matches('v')));
    let part = path.with_extension("part");
    std::fs::write(&part, &exe)
        .and_then(|_| std::fs::rename(&part, &path))
        .map_err(|e| format!("couldn't save the update: {e}"))?;
    *READY.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
    Ok(Status::Ready(tag))
}

/// DeskLink is about to exit from the tray: install a downloaded update
/// then (and start again, for "Restart to update").
pub fn install_on_exit(restart: bool) {
    ON_EXIT.store(if restart { INSTALL_AND_RESTART } else { INSTALL }, Ordering::SeqCst);
}

/// Last thing before exiting: start the relauncher if asked to install and an
/// update is ready. `config` is passed on to the restarted DeskLink when it
/// isn't the default one.
pub fn apply_on_exit(config: Option<&Path>) {
    let mode = ON_EXIT.load(Ordering::SeqCst);
    if mode == 0 {
        return;
    }
    let Some(new) = READY.lock().unwrap_or_else(|e| e.into_inner()).clone() else { return };
    let Some(target) = LAUNCHED.get().cloned() else { return };
    let Some(relauncher) = write_relauncher(&relauncher_dir()) else {
        log::error!("couldn't write the relauncher to {}", relauncher_dir().display());
        return;
    };
    let mut cmd = std::process::Command::new(&relauncher);
    cmd.arg(&new).arg(&target);
    if mode == INSTALL_AND_RESTART {
        cmd.arg("--relaunch");
        if let Some(c) = config {
            cmd.arg("--config").arg(c);
        }
    }
    match cmd.spawn() {
        Ok(_) => log::info!("installing the update into {}", target.display()),
        Err(e) => log::error!("couldn't start the relauncher: {e}"),
    }
}

fn write_relauncher(dir: &Path) -> Option<PathBuf> {
    let _ = std::fs::create_dir_all(dir);
    // A copy may still be running from the last update; then use another name.
    for name in ["relauncher.exe".to_string(), format!("relauncher-{}.exe", std::process::id())] {
        let p = dir.join(name);
        if std::fs::write(&p, RELAUNCHER).is_ok() {
            return Some(p);
        }
    }
    None
}

/// "2.4 MB"
pub fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1048576.0)
}

mod zip {
    /// One file out of a zip (stored or deflated), found by name through the
    /// central directory.
    pub fn extract(data: &[u8], want: &str) -> Result<Vec<u8>, String> {
        let bad = || "the downloaded zip is damaged".to_string();
        let u16_at = |o: usize| data.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize).ok_or_else(bad);
        let u32_at = |o: usize| data.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize).ok_or_else(bad);
        // End of central directory: the last "PK\5\6".
        let eocd = (0..data.len().saturating_sub(21)).rev().find(|&i| data[i..].starts_with(b"PK\x05\x06")).ok_or_else(bad)?;
        let entries = u16_at(eocd + 10)?;
        let mut p = u32_at(eocd + 16)?;
        for _ in 0..entries {
            if !data.get(p..).is_some_and(|d| d.starts_with(b"PK\x01\x02")) {
                return Err(bad());
            }
            let method = u16_at(p + 10)?;
            let csize = u32_at(p + 20)?;
            let size = u32_at(p + 24)?;
            let (nlen, xlen, clen) = (u16_at(p + 28)?, u16_at(p + 30)?, u16_at(p + 32)?);
            let local = u32_at(p + 42)?;
            let name = String::from_utf8_lossy(data.get(p + 46..p + 46 + nlen).ok_or_else(bad)?).to_string();
            p += 46 + nlen + xlen + clen;
            let base = name.rsplit(['/', '\\']).next().unwrap_or(&name);
            if !base.eq_ignore_ascii_case(want) {
                continue;
            }
            if !data.get(local..).is_some_and(|d| d.starts_with(b"PK\x03\x04")) {
                return Err(bad());
            }
            let start = local + 30 + u16_at(local + 26)? + u16_at(local + 28)?;
            let raw = data.get(start..start + csize).ok_or_else(bad)?;
            let out = match method {
                0 => raw.to_vec(),
                8 => miniz_oxide::inflate::decompress_to_vec(raw).map_err(|_| bad())?,
                m => return Err(format!("the zip uses an unsupported compression method ({m})")),
            };
            if out.len() != size {
                return Err(bad());
            }
            return Ok(out);
        }
        Err(format!("{want} isn't in the downloaded zip"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_their_digits() {
        assert!(version_number("v2026.11.16-0930") > version_number("2026.11.15-2342"));
        assert_eq!(version_number(&format!("v{VERSION}")), version_number(VERSION));
        assert!(version_number(VERSION) > 2026_00_00_0000);
    }

    #[test]
    fn status_lines_round_trip() {
        for s in [
            Status::Idle,
            Status::Checking,
            Status::UpToDate("v2026.11.16-0930".into()),
            Status::Downloading { tag: "v1".into(), done: 5, total: 10 },
            Status::Ready("v2".into()),
            Status::Failed("no release is published yet".into()),
            Status::NotRunning,
        ] {
            assert_eq!(Status::parse(&s.line()), s);
        }
    }

    #[test]
    fn extracts_the_exe_from_a_windows_zip() {
        // Made by the same tar.exe as the release (bsdtar, deflated).
        let dir = std::env::temp_dir().join(format!("desklink-zip-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let body: Vec<u8> = (0..200_000u32).flat_map(|i| (i % 251).to_le_bytes()).collect();
        std::fs::write(dir.join(EXE), &body).unwrap();
        let tar = std::path::Path::new(&std::env::var("SystemRoot").unwrap()).join(r"System32\tar.exe");
        let ok = std::process::Command::new(tar).arg("-a").arg("-c").arg("-f").arg(dir.join(ASSET)).arg("-C").arg(&dir).arg(EXE).status().unwrap();
        assert!(ok.success());
        let zip = std::fs::read(dir.join(ASSET)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(zip::extract(&zip, EXE).unwrap(), body);
        assert!(zip::extract(&zip, "other.exe").is_err());
    }
}
