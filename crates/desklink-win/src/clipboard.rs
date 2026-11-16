//! Clipboard access on a dedicated thread.
//!
//! Reading can block: apps like Excel and Word render clipboard data on
//! demand, sometimes slowly. The worker absorbs that so input never waits.
//! The worker also pumps messages, which a clipboard owner window must do
//! (other apps' `EmptyClipboard` sends it `WM_DESTROYCLIPBOARD`).
//!
//! Files from a peer are pasted on demand: we put a *delayed-rendered*
//! `CF_HDROP` on the clipboard. Nothing is transferred until an app pastes;
//! Windows then sends us `WM_RENDERFORMAT`, we download into a cache folder
//! (with a progress dialog) and hand back real paths. Works in every app that
//! pastes files (Explorer, Total Commander, file dialogs, ...).

use crate::ui;
use crossbeam_channel::{Receiver, Sender, unbounded};
use desklink_core::files::{self, DownloadError, FileSource};
use desklink_proto::{ClipFormat, ClipItem, FileOffer};
use std::cell::RefCell;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::collections::HashMap;
use std::thread::JoinHandle;
use std::time::Duration;
use windows_sys::Win32::Foundation::{GlobalFree, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardOwner, GetClipboardSequenceNumber,
    IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;

const WM_APP_JOB: u32 = WM_APP + 10;

pub type PeerId = usize;

/// Opens entry `index` of offer `offer` on `peer` at a byte offset (the network).
pub type Fetch = Arc<dyn Fn(PeerId, u64, u32, u64) -> Result<Box<dyn Read>, String> + Send + Sync>;

#[derive(Clone, Debug)]
pub struct ClipboardOptions {
    pub text: bool,
    pub images: bool,
    /// Offer files copied in Explorer.
    pub files: bool,
    /// Limits for offered files in bytes (0 = unlimited).
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    /// Paste files as virtual files (true) or download-on-paste (false).
    pub virtual_files: bool,
    /// Existing files when copying into a folder: `None` = ask.
    pub conflict: Option<desklink_core::files::Resolution>,
    pub max_bytes: usize,
    /// Skip content that password managers mark private.
    pub respect_private: bool,
}

pub enum ClipEvent {
    /// Our clipboard, read for `peer`. Empty `items` are never reported.
    /// `files`: copied files, as an offer plus the local path of each entry.
    Read { peer: PeerId, items: Vec<ClipItem>, files: Option<(FileOffer, Vec<Option<PathBuf>>)> },
}

enum Job {
    Read { peer: PeerId },
    Write { peer: PeerId, peer_name: String, items: Vec<ClipItem>, files: Option<FileOffer> },
}

/// A Ctrl+V copy DeskLink is doing into a folder.
struct DirectCopy {
    offer: u64,
    started: std::time::Instant,
    running: Arc<std::sync::atomic::AtomicBool>,
}

/// Requests right after a copy started are treated as the same paste
/// (apps can ask twice for one paste).
const DIRECT_COPY_QUIET: Duration = Duration::from_secs(3);

/// A paste must follow a key press or click within this time.
const PASTE_INTENT_WINDOW: Duration = Duration::from_secs(3);
/// Requests this soon after the files appeared on the clipboard come from
/// apps reacting to the change, not from a person.
const ARRIVAL_GRACE: Duration = Duration::from_millis(500);

/// Files a peer offered, waiting for an app to paste them.
struct PendingFiles {
    peer: PeerId,
    peer_name: String,
    offer: FileOffer,
}

thread_local! {
    static WORKER: RefCell<Option<Worker>> = const { RefCell::new(None) };
}

pub struct ClipboardWorker {
    jobs: Sender<Job>,
    events: Receiver<ClipEvent>,
    tid: u32,
    thread: Option<JoinHandle<()>>,
}

impl ClipboardWorker {
    /// `fetch` downloads offered files from peers (pasting files is off
    /// without it).
    pub fn start(opts: ClipboardOptions, fetch: Option<Fetch>) -> Result<ClipboardWorker, String> {
        let (jobs_tx, jobs_rx) = unbounded::<Job>();
        let (ev_tx, ev_rx) = unbounded();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("dl-clipboard".into())
            .spawn(move || {
                // STA: the paste progress dialog is COM.
                let _com = ui::com_init();
                // OLE clipboard (virtual files) needs OleInitialize on this thread.
                let _ = unsafe { windows::Win32::System::Ole::OleInitialize(None) };
                let hwnd = match create_window() {
                    Ok(h) => h,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(unsafe { GetCurrentThreadId() }));
                let cache = std::env::temp_dir().join("DeskLink").join("incoming");
                let _ = std::fs::remove_dir_all(&cache); // leftovers of earlier runs
                WORKER.set(Some(Worker {
                    hwnd,
                    opts,
                    formats: Formats::register(),
                    synced: HashMap::new(),
                    events: ev_tx,
                    fetch,
                    pending: None,
                    declined: None,
                    virtual_obj: None,
                    vformats: crate::vfiles::Formats::register(),
                    cache,
                    direct: None,
                    placeholder_at: None,
                    ignored_logged: None,
                }));
                let mut msg: MSG = unsafe { std::mem::zeroed() };
                while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
                    if msg.hwnd.is_null() && msg.message == WM_APP_JOB {
                        while let Ok(job) = jobs_rx.try_recv() {
                            WORKER.with_borrow_mut(|w| w.as_mut().map(|w| w.run(job)));
                        }
                        continue;
                    }
                    unsafe {
                        TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                // Never leave our data object on the clipboard after we are gone.
                WORKER.with_borrow(|w| {
                    if let Some(w) = w
                        && w.virtual_obj.is_some()
                        && w.own_clipboard()
                    {
                        unsafe {
                            let _ = windows::Win32::System::Ole::OleSetClipboard(None);
                        }
                    }
                });
                WORKER.take();
                unsafe { DestroyWindow(hwnd) };
            })
            .map_err(|e| e.to_string())?;
        let tid = ready_rx.recv().map_err(|_| "clipboard thread died".to_string())??;
        Ok(ClipboardWorker { jobs: jobs_tx, events: ev_rx, tid, thread: Some(thread) })
    }

    pub fn events(&self) -> &Receiver<ClipEvent> {
        &self.events
    }

    /// Read the clipboard for `peer` (reported as `ClipEvent::Read`) unless
    /// it is unchanged since we last exchanged clipboards with that peer.
    pub fn request_read(&self, peer: PeerId) {
        self.submit(Job::Read { peer });
    }

    /// Put `peer`'s clipboard on ours. `files` become pasteable on demand.
    pub fn write(&self, peer: PeerId, peer_name: &str, items: Vec<ClipItem>, files: Option<FileOffer>) {
        self.submit(Job::Write { peer, peer_name: peer_name.to_string(), items, files });
    }

    fn submit(&self, job: Job) {
        if self.jobs.send(job).is_ok() {
            unsafe { PostThreadMessageW(self.tid, WM_APP_JOB, 0, 0) };
        }
    }
}

impl Drop for ClipboardWorker {
    fn drop(&mut self) {
        unsafe { PostThreadMessageW(self.tid, WM_QUIT, 0, 0) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Formats {
    html: u32,
    rtf: u32,
    png: u32,
    /// Set by password managers etc. on content that must not be monitored.
    private: u32,
    drop_effect: u32,
}

impl Formats {
    fn register() -> Self {
        let reg = |name: &str| {
            let w: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
            unsafe { RegisterClipboardFormatW(w.as_ptr()) }
        };
        Formats {
            html: reg("HTML Format"),
            rtf: reg("Rich Text Format"),
            png: reg("PNG"),
            private: reg("ExcludeClipboardContentFromMonitorProcessing"),
            drop_effect: reg("Preferred DropEffect"),
        }
    }
}

struct Worker {
    hwnd: HWND,
    opts: ClipboardOptions,
    formats: Formats,
    /// Clipboard sequence number when we last exchanged with each peer: if
    /// unchanged, the peer already has this content (avoids echo and resends).
    synced: HashMap<PeerId, u32>,
    events: Sender<ClipEvent>,
    fetch: Option<Fetch>,
    /// A peer's files behind our delayed `CF_HDROP`, until pasted/replaced.
    pending: Option<PendingFiles>,
    /// Offer whose paste was just cancelled or failed, and when.
    declined: Option<(u64, std::time::Instant)>,
    /// Virtual-files data object we put on the clipboard (paste_mode virtual).
    virtual_obj: Option<windows::Win32::System::Com::IDataObject>,
    vformats: crate::vfiles::Formats,
    /// Pasted files land here first; the pasting app copies them onwards.
    cache: PathBuf,
    /// The current Ctrl+V copy into a folder, to ignore repeat requests.
    direct: Option<DirectCopy>,
    /// When the files placeholder was put on the clipboard.
    placeholder_at: Option<std::time::Instant>,
    /// Offer whose ignored request was already logged (log once).
    ignored_logged: Option<u64>,
}

impl Worker {
    fn run(&mut self, job: Job) {
        match job {
            Job::Read { peer } => {
                let seq = unsafe { GetClipboardSequenceNumber() };
                if self.synced.get(&peer) == Some(&seq) {
                    return;
                }
                let (items, roots) = self.read();
                self.synced.insert(peer, seq);
                // Walk folders after closing the clipboard: never hold it
                // (and block other apps) while scanning a big tree.
                let files = roots
                    .map(|r| desklink_core::files::build_offer(desklink_core::files::new_offer_id(), &r))
                    .filter(|(offer, _)| match desklink_core::files::check_limits(offer, self.opts.max_file_bytes, self.opts.max_total_bytes) {
                        Ok(()) => true,
                        Err(why) => {
                            log::warn!("copied files not shared: {why}");
                            false
                        }
                    });
                if !items.is_empty() || files.is_some() {
                    let _ = self.events.send(ClipEvent::Read { peer, items, files });
                }
            }
            Job::Write { peer, peer_name, items, files } => {
                let files = files.filter(|_| self.fetch.is_some());
                self.virtual_obj = None;
                let written = match files {
                    Some(offer) if self.opts.virtual_files => {
                        self.pending = None;
                        self.write_virtual(peer, &items, offer)
                    }
                    files => {
                        let ok = self.write(&items, files.is_some());
                        if ok && files.is_some() {
                            self.placeholder_at = Some(std::time::Instant::now());
                        }
                        if ok {
                            self.pending = files.map(|offer| PendingFiles { peer, peer_name, offer });
                        }
                        ok
                    }
                };
                if written {
                    // This content came from `peer`: don't send it back.
                    self.synced.insert(peer, unsafe { GetClipboardSequenceNumber() });
                    // Older downloads are no longer on the clipboard.
                    let _ = std::fs::remove_dir_all(&self.cache);
                }
            }
        }
    }

    fn open(&self) -> bool {
        // Other apps hold the clipboard briefly; retry for up to ~0.5 s.
        for _ in 0..25 {
            if unsafe { OpenClipboard(self.hwnd) } != 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        log::warn!("clipboard busy (held by another app)");
        false
    }

    fn read(&self) -> (Vec<ClipItem>, Option<Vec<PathBuf>>) {
        let f = &self.formats;
        if !self.open() {
            return (Vec::new(), None);
        }
        let mut items = Vec::new();
        let available = |fmt: u32| unsafe { IsClipboardFormatAvailable(fmt) } != 0;
        if self.opts.respect_private && available(f.private) {
            log::info!("clipboard content is marked private; not shared");
            unsafe { CloseClipboard() };
            return (items, None);
        }
        if self.opts.text {
            if let Some(w) = get(CF_UNICODETEXT) {
                let units: Vec<u16> = w.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
                let text = String::from_utf16_lossy(&units[..end]);
                items.push(ClipItem { format: ClipFormat::Text, data: text.into_bytes() });
            }
            for (fmt, kind) in [(f.html, ClipFormat::Html), (f.rtf, ClipFormat::Rtf)] {
                if let Some(mut b) = get(fmt) {
                    trim_nul(&mut b);
                    items.push(ClipItem { format: kind, data: b });
                }
            }
        }
        if self.opts.images {
            if let Some(b) = get(f.png) {
                items.push(ClipItem { format: ClipFormat::Png, data: b });
            }
            // Windows synthesises CF_DIB from CF_BITMAP / CF_DIBV5.
            if let Some(b) = get(CF_DIB) {
                items.push(ClipItem { format: ClipFormat::Dib, data: b });
            }
        }
        // Our own not-yet-pasted placeholder: asking for it would start the
        // download. Those files live on another machine; don't re-offer them.
        let own_placeholder = (self.pending.is_some() || self.virtual_obj.is_some()) && self.own_clipboard();
        let roots = if self.opts.files && !own_placeholder { hdrop_paths() } else { None };
        unsafe { CloseClipboard() };

        let total: usize = items.iter().map(|i| i.data.len()).sum();
        if total > self.opts.max_bytes {
            log::warn!(
                "clipboard is {} MB, over the {} MB limit; not shared",
                total >> 20,
                self.opts.max_bytes >> 20
            );
            return (Vec::new(), roots);
        }
        (items, roots)
    }

    fn write(&self, items: &[ClipItem], files: bool) -> bool {
        if (items.is_empty() && !files) || !self.open() {
            return false;
        }
        unsafe { EmptyClipboard() };
        if files {
            // Delayed rendering: a null handle promises CF_HDROP; Windows
            // asks us (WM_RENDERFORMAT) only when an app actually pastes.
            unsafe { SetClipboardData(CF_HDROP, std::ptr::null_mut()) };
            // Paste as a copy, never a move.
            set(self.formats.drop_effect, &DROPEFFECT_COPY.to_le_bytes());
        }
        for item in items {
            let Some((fmt, bytes)) = self.encode(item) else { continue };
            if !set(fmt, &bytes) {
                log::warn!("SetClipboardData({:?}) failed", item.format);
            }
        }
        unsafe { CloseClipboard() };
        true
    }

    /// Windows clipboard format and bytes for an item (if enabled).
    fn encode(&self, item: &ClipItem) -> Option<(u32, Vec<u8>)> {
        let f = &self.formats;
        Some(match item.format {
            ClipFormat::Text if self.opts.text => {
                let s = String::from_utf8_lossy(&item.data);
                let w: Vec<u8> = s.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect();
                (CF_UNICODETEXT, w)
            }
            ClipFormat::Html if self.opts.text => (f.html, with_nul(&item.data)),
            ClipFormat::Rtf if self.opts.text => (f.rtf, with_nul(&item.data)),
            ClipFormat::Png if self.opts.images => (f.png, item.data.clone()),
            ClipFormat::Dib if self.opts.images => (CF_DIB, item.data.clone()),
            _ => return None,
        })
    }

    /// Put a peer's files (plus any text/images copied with them) on the
    /// clipboard as virtual files.
    fn write_virtual(&mut self, peer: PeerId, items: &[ClipItem], offer: FileOffer) -> bool {
        let Some(fetch) = self.fetch.clone() else { return false };
        let extra = items.iter().filter_map(|i| self.encode(i)).map(|(cf, b)| (cf as u16, b)).collect();
        let obj = crate::vfiles::RemoteFiles::create(offer, peer, fetch, self.vformats, extra);
        match unsafe { windows::Win32::System::Ole::OleSetClipboard(&obj) } {
            Ok(()) => {
                self.virtual_obj = Some(obj);
                true
            }
            Err(e) => {
                log::warn!("OleSetClipboard failed: {e}");
                false
            }
        }
    }

    /// Is the clipboard content ours (placeholder or virtual files)? Both
    /// our window and OLE's clipboard window live on this thread.
    fn own_clipboard(&self) -> bool {
        use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
        let owner = unsafe { GetClipboardOwner() };
        !owner.is_null() && unsafe { GetWindowThreadProcessId(owner, std::ptr::null_mut()) } == unsafe { GetCurrentThreadId() }
    }

    /// An app is pasting our files (`WM_RENDERFORMAT`).
    ///
    /// Into a file manager (Explorer, Total Commander, the desktop): the app
    /// gets an empty file list at once (it never waits, so Windows' paste
    /// timeout can't hit big copies) and DeskLink copies into that folder
    /// itself, with its own progress, overwrite questions and safe replace.
    ///
    /// Elsewhere (file dialogs, other apps) the destination is unknown:
    /// download into the cache now and hand Windows the real paths, while
    /// the pasting app waits.
    fn render_files(&mut self) {
        let (Some(p), Some(fetch)) = (self.pending.as_ref(), self.fetch.clone()) else {
            return;
        };
        let offer_id = p.offer.id;
        // Guard rails against apps that read the clipboard on their own: a
        // paste follows a key press or click, and never comes in the first
        // moments after the files appeared on the clipboard.
        let arrived_just_now = self.placeholder_at.is_some_and(|t| t.elapsed() < ARRIVAL_GRACE);
        if arrived_just_now || !crate::intent::recent(PASTE_INTENT_WINDOW) {
            if self.ignored_logged != Some(offer_id) {
                self.ignored_logged = Some(offer_id);
                log::info!("request for the files from {} ignored: no key press or click just before it (an app read the clipboard by itself)", p.peer_name);
            }
            return;
        }
        if let Some(folder) = ui::folder_for_window(ui::foreground_window()) {
            // Answer "no data" and leave the placeholder as it is: setting
            // data (or re-placing the placeholder) changes the clipboard, and
            // apps watching it ask again, which looked like new pastes.
            // Repeat requests while a copy of these files runs, or just
            // after one started, are the same paste.
            let busy = self.direct.as_ref().is_some_and(|d| {
                d.offer == offer_id
                    && (d.running.load(std::sync::atomic::Ordering::Acquire)
                        || d.started.elapsed() < DIRECT_COPY_QUIET)
            });
            if busy {
                log::debug!("repeat paste request ignored (copy running or just started)");
                return;
            }
            log::info!("paste into {}: copying from {} with DeskLink", folder.display(), p.peer_name);
            let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
            self.direct = Some(DirectCopy { offer: offer_id, started: std::time::Instant::now(), running: running.clone() });
            let req = crate::transfer::CopyRequest {
                offer: p.offer.clone(),
                source: Box::new(NetSource { fetch, peer: p.peer, offer: offer_id }),
                dest: folder,
                peer_name: p.peer_name.clone(),
                conflict: self.opts.conflict,
                max_file: self.opts.max_file_bytes,
                max_total: self.opts.max_total_bytes,
            };
            std::thread::Builder::new()
                .name("dl-copy".into())
                .spawn(move || {
                    let _com = ui::com_init();
                    crate::transfer::copy_with_dialog(req);
                    running.store(false, std::sync::atomic::Ordering::Release);
                })
                .expect("spawn copy thread");
            return;
        }
        // Just cancelled or failed: the pasting app retries right away; don't
        // start over (a deliberate paste later is fine).
        if self.declined.is_some_and(|(id, t)| id == offer_id && t.elapsed() < RETRY_COOLDOWN) {
            log::debug!("paste retry suppressed after cancel/failure");
            self.declined = Some((offer_id, std::time::Instant::now()));
            return;
        }
        let title = format!("DeskLink: files from {}", p.peer_name);
        if let Err(why) = files::check_limits(&p.offer, self.opts.max_file_bytes, self.opts.max_total_bytes) {
            log::warn!("paste refused: {why}");
            notify_error(title, format!("Not pasted: {why}.\n\nChange the limits in desklink.cfg ([clipboard] max_file_mb / max_total_mb)."));
            return;
        }
        // Named after the peer: Windows' copy dialog shows "from <peer>". One
        // pending offer at a time, and the folder is emptied first.
        let folder: String = p.peer_name.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
        let dest = self.cache.join(if folder.is_empty() { "peer".to_string() } else { folder });
        // Always start empty: leftovers of a cancelled attempt would make the
        // download rename "a" to "a (2)" (we never overwrite).
        let _ = std::fs::remove_dir_all(&dest);
        if let Err(e) = std::fs::create_dir_all(&dest) {
            notify_error(title, format!("Cannot create {}: {e}", dest.display()));
            return;
        }
        log::info!("pasting {} from {}", files::summary(&p.offer), p.peer_name);
        let started = std::time::Instant::now();
        let source = NetSource { fetch, peer: p.peer, offer: p.offer.id };
        // The cache folder is always empty: nothing can conflict there (the
        // pasting app handles conflicts in the real destination).
        let mut never_conflicts = files::Policy(files::Resolution::Rename);
        let result = match ui::ProgressDialog::start(&format!("Pasting from {}", p.peer_name)) {
            Some(mut progress) => files::download(&p.offer, &source, &dest, &mut progress, &mut never_conflicts),
            None => files::download(&p.offer, &source, &dest, &mut Unattended, &mut never_conflicts),
        };
        match result {
            Ok(created) => {
                log::info!("pasted {} in {:.1}s", files::summary(&p.offer), started.elapsed().as_secs_f32());
                if !set_hdrop(&created) {
                    log::warn!("could not hand the pasted files to Windows");
                }
            }
            Err(DownloadError::Cancelled) => {
                log::info!("paste cancelled");
                self.declined = Some((offer_id, std::time::Instant::now()));
                let _ = std::fs::remove_dir_all(&dest);
            }
            Err(e) => {
                log::warn!("paste failed: {e}");
                self.declined = Some((offer_id, std::time::Instant::now()));
                let _ = std::fs::remove_dir_all(&dest);
                notify_error(title, format!("Pasting stopped:\n\n{e}"));
            }
        }
    }
}

const DROPEFFECT_COPY: u32 = 1;

/// After a cancelled or failed paste, ignore further requests for the same
/// files this long (apps retry immediately when they get nothing).
const RETRY_COOLDOWN: Duration = Duration::from_secs(5);

struct NetSource {
    fetch: Fetch,
    peer: PeerId,
    offer: u64,
}

impl FileSource for NetSource {
    fn open(&self, index: u32) -> Result<Box<dyn Read>, String> {
        (self.fetch)(self.peer, self.offer, index, 0)
    }
}

struct Unattended;
impl files::Progress for Unattended {
    fn update(&mut self, _: u64, _: u64, _: &str) {}
    fn cancelled(&self) -> bool {
        false
    }
}

/// Show an error without blocking the clipboard thread (an app may be
/// waiting on it).
fn notify_error(title: String, text: String) {
    std::thread::spawn(move || {
        let _com = ui::com_init();
        ui::show_error(&title, &text);
    });
}

/// Hand `paths` to the clipboard as CF_HDROP (DROPFILES + double-NUL list).
/// Call while rendering (`WM_RENDERFORMAT`) or with the clipboard open.
fn set_hdrop(paths: &[PathBuf]) -> bool {
    set(CF_HDROP, &hdrop_bytes(paths))
}

/// CF_HDROP payload: DROPFILES header + double-NUL-terminated wide paths.
pub(crate) fn hdrop_bytes(paths: &[PathBuf]) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    // DROPFILES { pFiles: u32, pt: POINT, fNC: BOOL, fWide: BOOL } = 20 bytes.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&20u32.to_le_bytes());
    bytes.extend_from_slice(&[0u8; 12]); // pt, fNC
    bytes.extend_from_slice(&1u32.to_le_bytes()); // fWide
    for p in paths {
        for u in Path::new(p).as_os_str().encode_wide().chain(Some(0)) {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
    }
    bytes.extend_from_slice(&[0, 0]);
    bytes
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_RENDERFORMAT if wparam as u32 == CF_HDROP => {
            WORKER.with(|cell| {
                // Busy (re-entered from our own clipboard call): skip.
                if let Ok(mut w) = cell.try_borrow_mut()
                    && let Some(w) = w.as_mut()
                {
                    w.render_files();
                }
            });
            return 0;
        }
        WM_RENDERALLFORMATS => return 0, // never download on exit
        WM_DESTROYCLIPBOARD => {
            // Someone else owns the clipboard now: our files are gone from it.
            WORKER.with(|cell| {
                if let Ok(mut w) = cell.try_borrow_mut()
                    && let Some(w) = w.as_mut()
                {
                    w.pending = None;
                }
            });
            return 0;
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn trim_nul(b: &mut Vec<u8>) {
    if let Some(end) = b.iter().position(|&c| c == 0) {
        b.truncate(end);
    }
}

fn with_nul(b: &[u8]) -> Vec<u8> {
    let mut v = b.to_vec();
    v.push(0);
    v
}

/// Copy clipboard data of `fmt` (clipboard must be open).
fn get(fmt: u32) -> Option<Vec<u8>> {
    unsafe {
        if IsClipboardFormatAvailable(fmt) == 0 {
            return None;
        }
        let h: HANDLE = GetClipboardData(fmt);
        if h.is_null() {
            return None;
        }
        let p = GlobalLock(h) as *const u8;
        if p.is_null() {
            return None;
        }
        let data = std::slice::from_raw_parts(p, GlobalSize(h)).to_vec();
        GlobalUnlock(h);
        Some(data)
    }
}

/// Hand `bytes` to the clipboard as `fmt` (clipboard must be open and owned).
fn set(fmt: u32, bytes: &[u8]) -> bool {
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1));
        if h.is_null() {
            return false;
        }
        let p = GlobalLock(h) as *mut u8;
        if p.is_null() {
            GlobalFree(h);
            return false;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        GlobalUnlock(h);
        if SetClipboardData(fmt, h).is_null() {
            GlobalFree(h); // still ours on failure
            return false;
        }
        true // now owned by the system
    }
}

fn create_window() -> Result<HWND, String> {
    let class: Vec<u16> = "DeskLinkClipboard\0".encode_utf16().collect();
    let hinst = unsafe { GetModuleHandleW(std::ptr::null()) };
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: hinst,
        lpszClassName: class.as_ptr(),
        ..unsafe { std::mem::zeroed() }
    };
    unsafe { RegisterClassW(&wc) };
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            std::ptr::null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        Err(format!("clipboard window: {}", std::io::Error::last_os_error()))
    } else {
        Ok(hwnd)
    }
}

/// Paths of files copied in Explorer (clipboard must be open).
fn hdrop_paths() -> Option<Vec<PathBuf>> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::UI::Shell::{DragQueryFileW, HDROP};
    unsafe {
        if IsClipboardFormatAvailable(CF_HDROP) == 0 {
            return None;
        }
        let h = GetClipboardData(CF_HDROP) as HDROP;
        if h.is_null() {
            return None;
        }
        let count = DragQueryFileW(h, u32::MAX, std::ptr::null_mut(), 0);
        let mut paths = Vec::with_capacity(count as usize);
        for i in 0..count {
            let len = DragQueryFileW(h, i, std::ptr::null_mut(), 0) as usize;
            let mut buf = vec![0u16; len + 1];
            DragQueryFileW(h, i, buf.as_mut_ptr(), buf.len() as u32);
            paths.push(PathBuf::from(std::ffi::OsString::from_wide(&buf[..len])));
        }
        (!paths.is_empty()).then_some(paths)
    }
}
