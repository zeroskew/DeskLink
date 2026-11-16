//! Copying a peer's files into a folder with DeskLink's own UI: progress
//! window (files remaining, speed, time left, Cancel), overwrite questions,
//! safe replace. Used by Ctrl+V in file managers and by the paste hotkey.

use crate::ui;
use desklink_core::files::{self, Conflicts, DownloadError, FileSource, Resolution};
use desklink_proto::{FileEntry, FileOffer};
use std::path::{Path, PathBuf};

pub struct CopyRequest {
    pub offer: FileOffer,
    pub source: Box<dyn FileSource + Send>,
    pub dest: PathBuf,
    pub peer_name: String,
    /// `None`: ask per file (Replace / Keep both / Skip, "for all").
    pub conflict: Option<Resolution>,
    /// Limits in bytes, 0 = unlimited.
    pub max_file: u64,
    pub max_total: u64,
}

/// Copy on a new thread (dialogs block; input must not).
pub fn spawn_copy(req: CopyRequest) {
    std::thread::Builder::new()
        .name("dl-copy".into())
        .spawn(move || {
            let _com = ui::com_init();
            copy_with_dialog(req);
        })
        .expect("spawn copy thread");
}

/// Blocking; call on a thread that ran `ui::com_init()`.
pub fn copy_with_dialog(req: CopyRequest) {
    copy_with_dialog_result(req);
}

/// Like `copy_with_dialog`; true if everything was copied.
pub fn copy_with_dialog_result(req: CopyRequest) -> bool {
    let title = format!("DeskLink: files from {}", req.peer_name);
    if let Err(why) = files::check_limits(&req.offer, req.max_file, req.max_total) {
        log::warn!("copy refused: {why}");
        ui::show_error(&title, &format!("Not copied: {why}.\n\nChange the limits in desklink.cfg ([clipboard] max_file_mb / max_total_mb)."));
        return false;
    }
    log::info!("copying {} from {} into {}", files::summary(&req.offer), req.peer_name, req.dest.display());
    let started = std::time::Instant::now();
    let result = match ui::ProgressDialog::start(&format!("Copying from {}", req.peer_name)) {
        Some(mut progress) => {
            let mut conflicts = resolver(req.conflict, progress.hwnd());
            files::download(&req.offer, &*req.source, &req.dest, &mut progress, &mut *conflicts)
        }
        None => {
            let mut conflicts = resolver(req.conflict, 0);
            files::download(&req.offer, &*req.source, &req.dest, &mut Unattended, &mut *conflicts)
        }
    };
    // Tell Explorer, Total Commander and co. the folder changed, so the new
    // files show up at once (also after a cancel or failure).
    notify_folder_changed(&req.dest);
    ui::refresh_total_commander();
    match result {
        Ok(created) => {
            log::info!(
                "copied {} in {:.1}s: {}",
                files::summary(&req.offer),
                started.elapsed().as_secs_f32(),
                created.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
            );
            true
        }
        Err(DownloadError::Cancelled) => {
            log::info!("file copy cancelled");
            false
        }
        Err(e) => {
            log::warn!("file copy failed: {e}");
            ui::show_error(
                &title,
                &format!("The copy stopped. Files already copied were kept; the unfinished one was removed.\n\n{e}"),
            );
            false
        }
    }
}

fn resolver(policy: Option<Resolution>, owner: isize) -> Box<dyn Conflicts> {
    match policy {
        Some(r) => Box::new(files::Policy(r)),
        None => Box::new(Ask { owner, remembered: None }),
    }
}

/// Asks per file; "do this for all" is remembered for the rest of the copy.
struct Ask {
    owner: isize,
    remembered: Option<Resolution>,
}

impl Conflicts for Ask {
    fn resolve(&mut self, existing: &Path, incoming: &FileEntry) -> Option<Resolution> {
        if let Some(r) = self.remembered {
            return Some(r);
        }
        let (choice, all) = ui::ask_conflict(self.owner, existing, incoming.size, incoming.modified_ms);
        let r = match choice {
            ui::ConflictChoice::Replace => Resolution::Overwrite,
            ui::ConflictChoice::KeepBoth => Resolution::Rename,
            ui::ConflictChoice::Skip => Resolution::Skip,
            ui::ConflictChoice::Cancel => return None,
        };
        if all {
            self.remembered = Some(r);
        }
        Some(r)
    }
}

struct Unattended;
impl files::Progress for Unattended {
    fn update(&mut self, _: u64, _: u64, _: &str) {}
    fn cancelled(&self) -> bool {
        false
    }
}

fn notify_folder_changed(dir: &Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::UI::Shell::{SHCNE_UPDATEDIR, SHCNF_FLUSHNOWAIT, SHCNF_PATHW, SHChangeNotify};
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        SHChangeNotify(SHCNE_UPDATEDIR, SHCNF_PATHW | SHCNF_FLUSHNOWAIT, Some(wide.as_ptr().cast()), None);
    }
}
