//! Platform-neutral file sharing: building an offer from copied paths, and
//! downloading an offer into a folder.

use desklink_proto::{FileEntry, FileOffer};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Refuse to offer absurdly large trees (the list itself must stay small).
pub const MAX_ENTRIES: usize = 100_000;

/// Build an offer from the paths the user copied. Returns the offer and, per
/// entry, the local file to serve (`None` for directories).
pub fn build_offer(id: u64, roots: &[PathBuf]) -> (FileOffer, Vec<Option<PathBuf>>) {
    let mut entries = Vec::new();
    let mut paths = Vec::new();
    for root in roots {
        let Some(name) = root.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue; // a drive root like C:\ has no name; skip
        };
        add(root, name, &mut entries, &mut paths);
    }
    if entries.len() > MAX_ENTRIES {
        log::warn!("{} files copied; only the first {MAX_ENTRIES} are offered", entries.len());
        entries.truncate(MAX_ENTRIES);
        paths.truncate(MAX_ENTRIES);
    }
    (FileOffer { id, entries }, paths)
}

fn add(path: &Path, rel: String, entries: &mut Vec<FileEntry>, paths: &mut Vec<Option<PathBuf>>) {
    if entries.len() > MAX_ENTRIES {
        return;
    }
    // symlink_metadata: never follow links (no loops, no surprises).
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        log::warn!("cannot read {}; skipped", path.display());
        return;
    };
    let modified_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64);
    if meta.is_dir() {
        entries.push(FileEntry { path: rel.clone(), is_dir: true, size: 0, modified_ms });
        paths.push(None);
        let Ok(rd) = std::fs::read_dir(path) else {
            log::warn!("cannot list {}; its contents are skipped", path.display());
            return;
        };
        let mut children: Vec<_> = rd.filter_map(Result::ok).collect();
        children.sort_by_key(|e| e.file_name());
        for c in children {
            let name = c.file_name().to_string_lossy().into_owned();
            add(&c.path(), format!("{rel}/{name}"), entries, paths);
        }
    } else if meta.is_file() {
        entries.push(FileEntry { path: rel, is_dir: false, size: meta.len(), modified_ms });
        paths.push(Some(path.to_path_buf()));
    }
}

/// Where file contents come from (the network, or a fake in tests).
pub trait FileSource {
    fn open(&self, index: u32) -> Result<Box<dyn Read>, String>;
}

/// Progress reporting and cancellation (a dialog, or a test double).
pub trait Progress {
    /// `done`/`total` bytes; `current` is the file being copied.
    fn update(&mut self, done: u64, total: u64, current: &str);
    fn cancelled(&self) -> bool;
    /// A file starts; `files_left` counts it and the ones after it.
    fn file_started(&mut self, _path: &str, _files_left: usize) {}
}

#[derive(Debug, PartialEq, Eq)]
pub enum DownloadError {
    Cancelled,
    Failed { path: String, error: String },
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DownloadError::Cancelled => write!(f, "cancelled"),
            DownloadError::Failed { path, error } => write!(f, "{path}: {error}"),
        }
    }
}

/// What to do when a file being copied already exists at the destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Replace it (only after the new file arrived complete and verified).
    Overwrite,
    /// Keep both: the new file becomes `name (2).ext`.
    Rename,
    /// Keep the existing file; don't copy this one.
    Skip,
}

/// Decides file conflicts (asks the user, or applies a fixed policy).
pub trait Conflicts {
    /// `existing` is the file in the way; `incoming` the one being copied.
    /// `None` cancels the whole copy.
    fn resolve(&mut self, existing: &Path, incoming: &FileEntry) -> Option<Resolution>;
}

/// Always the same answer.
pub struct Policy(pub Resolution);

impl Conflicts for Policy {
    fn resolve(&mut self, _: &Path, _: &FileEntry) -> Option<Resolution> {
        Some(self.0)
    }
}

/// Copy every entry of `offer` into `dest`, like a file manager would:
/// folders that exist are merged into; files that exist are resolved by
/// `conflicts`. Each file is written as `<name>.partial` and moved into place
/// only once complete and verified (the source checks the hash), so a failed
/// or cancelled copy never leaves a truncated file under the real name and
/// never destroys the file it was going to replace. Returns the top-level
/// paths written.
pub fn download(
    offer: &FileOffer,
    source: &dyn FileSource,
    dest: &Path,
    progress: &mut dyn Progress,
    conflicts: &mut dyn Conflicts,
) -> Result<Vec<PathBuf>, DownloadError> {
    let total = offer.total_bytes();
    let mut done = 0u64;
    let mut files_left = offer.file_count();
    let mut created = Vec::new();
    let mut buf = vec![0u8; 256 << 10];

    for (index, e) in offer.entries.iter().enumerate() {
        if progress.cancelled() {
            return Err(DownloadError::Cancelled);
        }
        // The offer comes from the network: validate before touching disk.
        if !is_safe_relative(&e.path) {
            return Err(DownloadError::Failed { path: e.path.clone(), error: "unsafe path in offer".into() });
        }
        let fail = |error: String| DownloadError::Failed { path: e.path.clone(), error };
        let mut target = dest.join(e.path.replace('/', std::path::MAIN_SEPARATOR_STR));
        let top_level = !e.path.contains('/');

        if e.is_dir {
            if target.is_file() {
                return Err(fail(format!("a file named \"{}\" is in the way", target.display())));
            }
            std::fs::create_dir_all(&target).map_err(|x| fail(x.to_string()))?;
            if top_level {
                created.push(target);
            }
            continue;
        }

        if target.exists() {
            // A folder in the way can't be replaced by a file: keep both.
            let choice = if target.is_dir() {
                Resolution::Rename
            } else {
                conflicts.resolve(&target, e).ok_or(DownloadError::Cancelled)?
            };
            match choice {
                Resolution::Overwrite => {}
                Resolution::Rename => {
                    let parent = target.parent().unwrap_or(dest).to_path_buf();
                    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    target = parent.join(free_name(&parent, &name));
                }
                Resolution::Skip => {
                    done += e.size;
                    files_left -= 1;
                    progress.update(done, total, &e.path);
                    continue;
                }
            }
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|x| fail(x.to_string()))?;
        }
        progress.file_started(&e.path, files_left);
        files_left -= 1;
        progress.update(done, total, &e.path);
        let partial = partial_name(&target);
        let result = (|| -> Result<(), DownloadError> {
            let mut reader = source.open(index as u32).map_err(fail)?;
            let mut out = std::fs::File::create(&partial).map_err(|x| fail(x.to_string()))?;
            loop {
                if progress.cancelled() {
                    return Err(DownloadError::Cancelled);
                }
                let n = reader.read(&mut buf).map_err(|x| fail(x.to_string()))?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n]).map_err(|x| fail(x.to_string()))?;
                done += n as u64;
                progress.update(done, total, &e.path);
            }
            out.flush().map_err(|x| fail(x.to_string()))?;
            if e.modified_ms > 0 {
                let _ = out.set_modified(UNIX_EPOCH + Duration::from_millis(e.modified_ms));
            }
            drop(out);
            // Replaces an existing file atomically (Overwrite).
            std::fs::rename(&partial, &target).map_err(|x| fail(x.to_string()))
        })();
        if let Err(err) = result {
            let _ = std::fs::remove_file(&partial);
            return Err(err);
        }
        if top_level {
            created.push(target);
        }
    }
    progress.update(total, total, "");
    Ok(created)
}

/// `name`, or `name (2).ext`, `name (3).ext`... whichever doesn't exist.
fn free_name(dest: &Path, name: &str) -> String {
    if !dest.join(name).exists() {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (2..)
        .map(|n| format!("{stem} ({n}){ext}"))
        .find(|c| !dest.join(c).exists())
        .unwrap()
}

/// The offer comes from the network: never let it escape `dest`.
fn is_safe_relative(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.starts_with('/')
        && !rel.contains('\\')
        && !rel.contains(':')
        && rel.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

fn partial_name(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_owned();
    s.push(".partial");
    PathBuf::from(s)
}


/// Transfer limits in bytes (0 = unlimited). Returns a message for the user
/// if `offer` exceeds them.
pub fn check_limits(offer: &FileOffer, max_file: u64, max_total: u64) -> Result<(), String> {
    if max_file > 0
        && let Some(big) = offer.entries.iter().filter(|e| !e.is_dir).max_by_key(|e| e.size)
        && big.size > max_file
    {
        return Err(format!(
            "\"{}\" is {}, over the {} limit per file",
            big.path,
            human_size(big.size),
            human_size(max_file)
        ));
    }
    let total = offer.total_bytes();
    if max_total > 0 && total > max_total {
        return Err(format!("{} is over the {} limit for one copy", human_size(total), human_size(max_total)));
    }
    Ok(())
}
/// Offer ids: unique enough across restarts and copies.
pub fn new_offer_id() -> u64 {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    (ms << 16) ^ COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// "3 files, 2 folders, 41.2 MB".
pub fn summary(offer: &FileOffer) -> String {
    let files = offer.file_count();
    let dirs = offer.entries.len() - files;
    let mut parts = vec![format!("{files} file{}", if files == 1 { "" } else { "s" })];
    if dirs > 0 {
        parts.push(format!("{dirs} folder{}", if dirs == 1 { "" } else { "s" }));
    }
    parts.push(human_size(offer.total_bytes()));
    parts.join(", ")
}

pub fn human_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct LocalSource(Vec<Option<PathBuf>>);
    impl FileSource for LocalSource {
        fn open(&self, index: u32) -> Result<Box<dyn Read>, String> {
            let p = self.0[index as usize].as_ref().ok_or("dir")?;
            Ok(Box::new(std::fs::File::open(p).map_err(|e| e.to_string())?))
        }
    }

    struct NoProgress(bool);
    impl Progress for NoProgress {
        fn update(&mut self, _: u64, _: u64, _: &str) {}
        fn cancelled(&self) -> bool {
            self.0
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("desklink-core-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn offer_lists_tree_parents_first_and_downloads_it() {
        let src = tmp("src");
        std::fs::create_dir_all(src.join("photos/2026")).unwrap();
        std::fs::write(src.join("photos/a.jpg"), b"AAAA").unwrap();
        std::fs::write(src.join("photos/2026/b.jpg"), b"BB").unwrap();
        std::fs::write(src.join("notes.txt"), b"hello").unwrap();

        let (offer, paths) = build_offer(1, &[src.join("photos"), src.join("notes.txt")]);
        let names: Vec<_> = offer.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(names, ["photos", "photos/2026", "photos/2026/b.jpg", "photos/a.jpg", "notes.txt"]);
        assert_eq!(offer.total_bytes(), 11);
        assert_eq!(summary(&offer), "3 files, 2 folders, 11 B");

        let dst = tmp("dst");
        let src_files = LocalSource(paths);
        let created = download(&offer, &src_files, &dst, &mut NoProgress(false), &mut Policy(Resolution::Skip)).unwrap();
        assert_eq!(created, vec![dst.join("photos"), dst.join("notes.txt")]);
        assert_eq!(std::fs::read(dst.join("photos/2026/b.jpg")).unwrap(), b"BB");
        assert_eq!(std::fs::read(dst.join("notes.txt")).unwrap(), b"hello");
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }

    /// Copy the same tree twice with each policy; folders always merge.
    fn copy_twice(policy: Resolution, tag: &str) -> PathBuf {
        let src = tmp(&format!("csrc-{tag}"));
        std::fs::create_dir_all(src.join("a")).unwrap();
        std::fs::write(src.join("a/x.txt"), b"new").unwrap();
        let (offer, paths) = build_offer(1, &[src.join("a")]);
        let dst = tmp(&format!("cdst-{tag}"));
        std::fs::create_dir_all(dst.join("a")).unwrap();
        std::fs::write(dst.join("a/x.txt"), b"old").unwrap();
        download(&offer, &LocalSource(paths), &dst, &mut NoProgress(false), &mut Policy(policy)).unwrap();
        let _ = std::fs::remove_dir_all(&src);
        dst
    }

    #[test]
    fn conflicts_merge_folders_and_follow_policy() {
        let d = copy_twice(Resolution::Overwrite, "o");
        assert_eq!(std::fs::read(d.join("a/x.txt")).unwrap(), b"new");
        assert!(!d.join("a (2)").exists(), "folders merge, never 'a (2)'");

        let d = copy_twice(Resolution::Rename, "r");
        assert_eq!(std::fs::read(d.join("a/x.txt")).unwrap(), b"old");
        assert_eq!(std::fs::read(d.join("a/x (2).txt")).unwrap(), b"new");

        let d = copy_twice(Resolution::Skip, "s");
        assert_eq!(std::fs::read(d.join("a/x.txt")).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(d.join("a")).unwrap().count(), 1);
    }

    #[test]
    fn failed_overwrite_keeps_the_old_file() {
        struct Broken;
        impl FileSource for Broken {
            fn open(&self, _: u32) -> Result<Box<dyn Read>, String> {
                Err("link lost".into())
            }
        }
        let offer = FileOffer {
            id: 1,
            entries: vec![FileEntry { path: "x.txt".into(), is_dir: false, size: 3, modified_ms: 0 }],
        };
        let dst = tmp("keepold");
        std::fs::write(dst.join("x.txt"), b"old").unwrap();
        let r = download(&offer, &Broken, &dst, &mut NoProgress(false), &mut Policy(Resolution::Overwrite));
        assert!(matches!(r, Err(DownloadError::Failed { .. })));
        assert_eq!(std::fs::read(dst.join("x.txt")).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(&dst).unwrap().count(), 1, "no .partial left");
    }

    #[test]
    fn asking_can_cancel() {
        struct No;
        impl Conflicts for No {
            fn resolve(&mut self, _: &Path, _: &FileEntry) -> Option<Resolution> {
                None
            }
        }
        let src = tmp("asrc");
        std::fs::write(src.join("x.txt"), b"new").unwrap();
        let (offer, paths) = build_offer(1, &[src.join("x.txt")]);
        let dst = tmp("adst");
        std::fs::write(dst.join("x.txt"), b"old").unwrap();
        let r = download(&offer, &LocalSource(paths), &dst, &mut NoProgress(false), &mut No);
        assert_eq!(r, Err(DownloadError::Cancelled));
        assert_eq!(std::fs::read(dst.join("x.txt")).unwrap(), b"old");
    }

    #[test]
    fn cancel_leaves_no_partial_files() {
        let src = tmp("src2");
        std::fs::write(src.join("f.bin"), vec![1u8; 1000]).unwrap();
        let (offer, paths) = build_offer(1, &[src.join("f.bin")]);
        let dst = tmp("dst2");
        let r = download(&offer, &LocalSource(paths), &dst, &mut NoProgress(true), &mut Policy(Resolution::Skip));
        assert_eq!(r, Err(DownloadError::Cancelled));
        assert_eq!(std::fs::read_dir(&dst).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }

    #[test]
    fn hostile_paths_are_rejected() {
        for bad in ["../x", "a/../../x", "/etc/passwd", "C:/Windows/x", "a\\..\\x", "a//b", "."] {
            assert!(!is_safe_relative(bad), "{bad}");
        }
        assert!(is_safe_relative("photos/2026/b.jpg"));
        let offer = FileOffer {
            id: 1,
            entries: vec![FileEntry { path: "../evil.txt".into(), is_dir: false, size: 1, modified_ms: 0 }],
        };
        struct Never;
        impl FileSource for Never {
            fn open(&self, _: u32) -> Result<Box<dyn Read>, String> {
                panic!("must not fetch");
            }
        }
        let dst = tmp("dst3");
        let r = download(&offer, &Never, &dst, &mut NoProgress(false), &mut Policy(Resolution::Skip));
        assert!(matches!(r, Err(DownloadError::Failed { .. })));
        let _ = std::fs::remove_dir_all(&dst);
    }
}

#[cfg(test)]
mod limit_tests {
    use super::*;

    fn offer(sizes: &[u64]) -> FileOffer {
        FileOffer {
            id: 1,
            entries: sizes
                .iter()
                .enumerate()
                .map(|(i, &s)| FileEntry { path: format!("f{i}"), is_dir: false, size: s, modified_ms: 0 })
                .collect(),
        }
    }

    #[test]
    fn zero_means_unlimited() {
        assert!(check_limits(&offer(&[u64::MAX / 4, u64::MAX / 4]), 0, 0).is_ok());
    }

    #[test]
    fn per_file_and_total() {
        let mb = 1 << 20;
        let o = offer(&[3 * mb, 2 * mb]);
        assert!(check_limits(&o, 3 * mb, 5 * mb).is_ok());
        let e = check_limits(&o, 2 * mb, 0).unwrap_err();
        assert!(e.contains("\"f0\" is 3.0 MB, over the 2.0 MB limit per file"), "{e}");
        let e = check_limits(&o, 0, 4 * mb).unwrap_err();
        assert!(e.contains("5.0 MB is over the 4.0 MB limit"), "{e}");
    }
}
