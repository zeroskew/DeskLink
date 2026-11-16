//! Logging to the console (when there is one), the last lines in memory (the
//! settings window shows them) and, optionally, a size-rotated text file.

use crossbeam_channel::{Receiver, Sender, unbounded};
use desklink_config::LogSettings;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Lines kept in memory for the settings window.
pub const MEMORY_LINES: usize = 1000;

/// The last `MEMORY_LINES` lines, and the windows following new ones.
struct Memory {
    lines: VecDeque<String>,
    followers: Vec<Sender<String>>,
}

static MEMORY: Mutex<Memory> = Mutex::new(Memory { lines: VecDeque::new(), followers: Vec::new() });

fn remember(text: &str) {
    let mut m = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    for line in text.lines().filter(|l| !l.is_empty()) {
        if m.lines.len() == MEMORY_LINES {
            m.lines.pop_front();
        }
        m.lines.push_back(line.to_string());
        // A follower that went away is dropped here.
        m.followers.retain(|f| f.send(line.to_string()).is_ok());
    }
}

/// The lines in memory, then every new line as it is logged.
pub fn follow() -> (Vec<String>, Receiver<String>) {
    let (tx, rx) = unbounded();
    let mut m = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    m.followers.push(tx);
    (m.lines.iter().cloned().collect(), rx)
}

/// A text file that rotates to `<name>.1` when it exceeds `max_bytes`.
struct LogFile {
    path: PathBuf,
    file: Option<File>,
    written: u64,
    max_bytes: u64,
}

impl LogFile {
    fn open(path: PathBuf, max_bytes: u64) -> io::Result<LogFile> {
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(LogFile { path, file: Some(file), written, max_bytes })
    }

    fn rotate(&mut self) {
        self.file = None; // close before renaming
        let old = rotated(&self.path);
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(&self.path, &old);
        self.file = OpenOptions::new().create(true).append(true).open(&self.path).ok();
        self.written = 0;
    }

    fn write(&mut self, buf: &[u8]) {
        if self.max_bytes > 0 && self.written + buf.len() as u64 > self.max_bytes {
            self.rotate();
        }
        if let Some(f) = self.file.as_mut()
            && f.write_all(buf).is_ok()
        {
            self.written += buf.len() as u64;
        }
    }
}

fn rotated(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".1");
    PathBuf::from(s)
}

/// Every formatted log record goes to stderr, memory and the file.
struct Sink {
    file: Option<LogFile>,
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = io::stderr().write_all(buf);
        remember(&String::from_utf8_lossy(buf));
        if let Some(f) = self.file.as_mut() {
            f.write(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = io::stderr().flush();
        if let Some(f) = self.file.as_mut().and_then(|f| f.file.as_mut()) {
            f.flush()?;
        }
        Ok(())
    }
}

/// Initialise logging. Returns the log file path if file logging is on, or
/// an error message to report once the logger is up.
pub fn init(settings: &LogSettings, verbose: bool, dir: &Path) -> Result<Option<PathBuf>, String> {
    let level = if verbose { "debug" } else { settings.level.as_str() };
    // Our crates at `level`; chatty dependencies stay at info.
    let filter = format!("{level},quinn=info,quinn_proto=info,rustls=info");
    let mut b = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(filter));
    b.format_timestamp_millis();

    let mut result = Ok(None);
    let mut sink = Sink { file: None };
    if settings.file {
        let path = match &settings.path {
            Some(p) if Path::new(p).is_absolute() => PathBuf::from(p),
            Some(p) => dir.join(p),
            None => dir.join("desklink.log"),
        };
        match LogFile::open(path.clone(), settings.max_size_mb as u64 * 1024 * 1024) {
            Ok(f) => {
                sink.file = Some(f);
                result = Ok(Some(path));
            }
            Err(e) => result = Err(format!("cannot open log file {}: {e}", path.display())),
        }
    }
    b.target(env_logger::Target::Pipe(Box::new(sink)));
    b.init();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_at_size_limit() {
        let dir = std::env::temp_dir().join(format!("desklink-log-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.log");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(rotated(&path));

        let mut f = LogFile::open(path.clone(), 100).unwrap();
        f.write(&[b'a'; 60]);
        f.write(&[b'b'; 60]); // exceeds 100: rotate first
        drop(f);
        assert_eq!(std::fs::read(&path).unwrap(), vec![b'b'; 60]);
        assert_eq!(std::fs::read(rotated(&path)).unwrap(), vec![b'a'; 60]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn memory_keeps_the_last_lines_and_feeds_followers() {
        for i in 0..MEMORY_LINES + 5 {
            remember(&format!("line {i}\n"));
        }
        let (lines, rx) = follow();
        assert_eq!(lines.len(), MEMORY_LINES);
        assert_eq!(lines.last().unwrap(), &format!("line {}", MEMORY_LINES + 4));
        remember("next\n");
        assert_eq!(rx.try_recv().unwrap(), "next");
    }
}

/// Name of the file a crash or fatal error is written to, next to the config.
pub const CRASH_FILE: &str = "desklink-crash.log";

/// Append `reason` and the last lines of the log to `desklink-crash.log`, so
/// a crash leaves evidence even with the log file off. Never blocks: a
/// panic may happen while the log is locked.
pub fn crash_report(dir: &Path, reason: &str) {
    const LAST: usize = 300;
    let lines: Vec<String> = match MEMORY.try_lock() {
        Ok(m) => m.lines.iter().rev().take(LAST).rev().cloned().collect(),
        Err(_) => vec!["(log busy)".into()],
    };
    let Ok(mut f) = OpenOptions::new().create(true).append(true).open(dir.join(CRASH_FILE)) else { return };
    // The log lines carry the time; this happened right after the last one.
    let _ = writeln!(f, "==== DeskLink {} stopped: {reason}", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(f, "---- last {} log lines:", lines.len());
    for l in lines {
        let _ = writeln!(f, "{l}");
    }
    let _ = writeln!(f);
}
