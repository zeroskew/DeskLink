//! Dragging files from one machine to another.
//!
//! Source side (`DragCatcher`): when the cursor reaches the edge with the
//! left button held, an invisible drop zone appears under it. The app being
//! dragged from (Explorer, Total Commander) offers it the drag; we read the
//! file list, then end the local drag over our zone so nothing is dropped
//! locally. The drag then continues on the peer.
//!
//! Target side (`start_drag`): a real OLE drag runs under the injected
//! cursor, so windows show normal drag feedback. On release over a file
//! manager we copy into that folder ourselves (same dialog as Ctrl+V);
//! elsewhere the target app pulls the files as virtual files.

use crate::clipboard::{Fetch, PeerId};
use crate::{transfer, ui, vfiles};
use crossbeam_channel::{Receiver, Sender, unbounded};
use desklink_core::files::{FileSource, Resolution};
use desklink_proto::FileOffer;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{
    DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, HWND, LPARAM, LRESULT, POINT, POINTL, S_OK,
    WPARAM,
};
use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, DoDragDrop, IDropSource, IDropSource_Impl, IDropTarget,
    IDropTarget_Impl, OleInitialize, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, LWA_ALPHA,
    MSG, PeekMessageW, PostThreadMessageW, PM_REMOVE, RegisterClassW, SW_SHOWNOACTIVATE,
    HWND_TOPMOST, SW_HIDE, SWP_NOACTIVATE, SetLayeredWindowAttributes, SetWindowPos, ShowWindow, TranslateMessage, WM_APP, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows_core::{BOOL, HRESULT, Ref, implement, w};

const CF_HDROP: u16 = 15;
const WM_APP_CATCH: u32 = WM_APP + 20;

// ---------------------------------------------------------------------------
// Source side
// ---------------------------------------------------------------------------

type CatchResult = Option<Vec<PathBuf>>;
type CatchRequest = (Duration, Sender<CatchResult>);

/// Learns which files are being dragged when the cursor reaches the edge.
/// Works in the background: `begin` returns at once, the answer arrives on
/// `results()`.
pub struct DragCatcher {
    requests: Sender<CatchRequest>,
    results_tx: Sender<CatchResult>,
    results: Receiver<CatchResult>,
    tid: u32,
}

impl DragCatcher {
    pub fn start() -> Result<DragCatcher, String> {
        let (tx, rx) = unbounded::<CatchRequest>();
        let (results_tx, results) = unbounded();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("dl-dragcatch".into())
            .spawn(move || {
                let _com = ui::com_init();
                let _ = unsafe { OleInitialize(None) };
                register_class();
                let _ = ready_tx.send(unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() });
                let mut msg = MSG::default();
                while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
                    if msg.hwnd.is_invalid() && msg.message == WM_APP_CATCH {
                        while let Ok((timeout, reply)) = rx.try_recv() {
                            let _ = reply.send(catch_at(timeout));
                        }
                        continue;
                    }
                    unsafe {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let tid = ready_rx.recv().map_err(|_| "drag catcher thread died".to_string())?;
        Ok(DragCatcher { requests: tx, results_tx, results, tid })
    }

    /// Check whether files are being dragged under the cursor (ending that
    /// local drag); the answer arrives on `results()` within about `timeout`.
    pub fn begin(&self, timeout: Duration) {
        if self.requests.send((timeout, self.results_tx.clone())).is_ok() {
            unsafe {
                let _ = PostThreadMessageW(self.tid, WM_APP_CATCH, WPARAM(0), LPARAM(0));
            }
        }
    }

    pub fn results(&self) -> &Receiver<CatchResult> {
        &self.results
    }
}

#[derive(Default)]
struct CatchState {
    /// `Some` once the drag entered our zone (empty: not a file drag).
    paths: Option<Vec<PathBuf>>,
    dropped: bool,
}

#[implement(IDropTarget)]
struct DropZone {
    state: Rc<RefCell<CatchState>>,
}

impl IDropTarget_Impl for DropZone_Impl {
    fn DragEnter(
        &self,
        pdataobj: Ref<IDataObject>,
        _: MODIFIERKEYS_FLAGS,
        _: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows_core::Result<()> {
        let paths = pdataobj.as_ref().map(hdrop_paths).unwrap_or_default();
        self.state.borrow_mut().paths = Some(paths);
        if let Some(e) = unsafe { pdweffect.as_mut() } {
            *e = DROPEFFECT_COPY;
        }
        Ok(())
    }

    fn DragOver(&self, _: MODIFIERKEYS_FLAGS, _: &POINTL, pdweffect: *mut DROPEFFECT) -> windows_core::Result<()> {
        if let Some(e) = unsafe { pdweffect.as_mut() } {
            *e = DROPEFFECT_COPY;
        }
        Ok(())
    }

    fn DragLeave(&self) -> windows_core::Result<()> {
        Ok(())
    }

    /// The local drag ends here, deliberately with no effect.
    fn Drop(&self, _: Ref<IDataObject>, _: MODIFIERKEYS_FLAGS, _: &POINTL, pdweffect: *mut DROPEFFECT) -> windows_core::Result<()> {
        self.state.borrow_mut().dropped = true;
        if let Some(e) = unsafe { pdweffect.as_mut() } {
            *e = DROPEFFECT_NONE;
        }
        Ok(())
    }
}

fn hdrop_paths(obj: &IDataObject) -> Vec<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let fe = FORMATETC {
        cfFormat: CF_HDROP,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let Ok(mut medium) = (unsafe { obj.GetData(&fe) }) else { return Vec::new() };
    let mut out = Vec::new();
    unsafe {
        let h = HDROP(medium.u.hGlobal.0);
        let n = DragQueryFileW(h, u32::MAX, None);
        for i in 0..n {
            let len = DragQueryFileW(h, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            DragQueryFileW(h, i, Some(&mut buf));
            out.push(PathBuf::from(std::ffi::OsString::from_wide(&buf[..len])));
        }
        ReleaseStgMedium(&mut medium);
    }
    out
}

fn register_class() {
    unsafe extern "system" fn proc_(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        unsafe { DefWindowProcW(h, m, w, l) }
    }
    let wc = WNDCLASSW {
        lpfnWndProc: Some(proc_),
        hInstance: unsafe { GetModuleHandleW(None) }.unwrap_or_default().into(),
        lpszClassName: w!("DeskLinkDropZone"),
        ..Default::default()
    };
    unsafe { RegisterClassW(&wc) };
}

/// Pump messages until `done()` or the deadline.
fn pump_until(deadline: Instant, done: impl Fn() -> bool) -> bool {
    let mut msg = MSG::default();
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    done()
}

/// Put a `2 * half` square window under the cursor again (it may have moved
/// since the window was placed); returns the cursor position.
fn center_on_cursor(hwnd: HWND, half: i32) -> POINT {
    let mut cur = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut cur);
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), cur.x - half, cur.y - half, half * 2, half * 2, SWP_NOACTIVATE);
    }
    cur
}

fn catch_at(timeout: Duration) -> Option<Vec<PathBuf>> {
    const HALF: i32 = 64;
    // Around the real cursor (the hook's point can lie past the screen edge).
    let mut cur = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut cur);
    }
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("DeskLinkDropZone"),
            w!(""),
            WS_POPUP,
            cur.x - HALF,
            cur.y - HALF,
            HALF * 2,
            HALF * 2,
            None,
            None,
            None,
            None,
        )
    }
    .ok()?;
    let state = Rc::new(RefCell::new(CatchState::default()));
    let target: IDropTarget = DropZone { state: state.clone() }.into();
    let result = (|| {
        unsafe {
            // Nearly transparent but still hit-testable (alpha 0 isn't).
            SetLayeredWindowAttributes(hwnd, Default::default(), 1, LWA_ALPHA).ok()?;
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            RegisterDragDrop(hwnd, &target).ok()?;
        }
        // The drag source only re-checks what is under the cursor on mouse
        // input. The user's own mouse already drives the peer, so feed it
        // real (injected) 1-pixel moves onto our zone and back. The cursor
        // may still be settling at the edge (a controlled machine places it
        // there right after asking to leave): keep the zone under it.
        let cur = center_on_cursor(hwnd, HALF);
        let dy = if cur.y > 0 { -1 } else { 1 };
        let deadline = Instant::now() + timeout;
        let mut nudge = Instant::now();
        while Instant::now() < deadline && state.borrow().paths.is_none() {
            if Instant::now() >= nudge {
                center_on_cursor(hwnd, HALF);
                crate::inject::raw_move(0, dy);
                pump_until(Instant::now() + Duration::from_millis(15), || false);
                crate::inject::raw_move(0, -dy);
                nudge = Instant::now() + Duration::from_millis(100);
            }
            pump_until(Instant::now() + Duration::from_millis(15), || state.borrow().paths.is_some());
        }
        let paths = state.borrow().paths.clone().unwrap_or_default();
        // End the local press over our zone in every case: a drag drops here
        // with no effect, and a plain press (text selection, ...) ends
        // normally. Never anywhere else, where it could drop files.
        center_on_cursor(hwnd, HALF);
        pump_until(Instant::now() + Duration::from_millis(15), || false);
        crate::inject::raw_button(desklink_proto::MouseButton::Left, false);
        let wait = if paths.is_empty() { 50 } else { 300 };
        pump_until(Instant::now() + Duration::from_millis(wait), || state.borrow().dropped);
        (!paths.is_empty()).then_some(paths)
    })();
    unsafe {
        let _ = RevokeDragDrop(hwnd);
        let _ = DestroyWindow(hwnd);
    }
    result
}

// ---------------------------------------------------------------------------
// Target side
// ---------------------------------------------------------------------------

pub struct DragOptions {
    pub peer: PeerId,
    pub peer_name: String,
    pub fetch: Fetch,
    pub conflict: Option<Resolution>,
    pub max_file: u64,
    pub max_total: u64,
}

/// Start an OLE drag of `offer` under the cursor. It presses the left button
/// itself on its own anchor window; the release comes from the peer through
/// the `Injector` (see `Injector::mark_held`). Returns a flag that cancels it.
pub fn start_drag(offer: FileOffer, opts: DragOptions) -> Arc<AtomicBool> {
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    std::thread::Builder::new()
        .name("dl-drag".into())
        .spawn(move || {
            let _com = ui::com_init();
            let _ = unsafe { OleInitialize(None) };
            run_drag(offer, opts, flag);
        })
        .expect("spawn drag thread");
    cancel
}

/// What a drag offers targets: file paths in our cache folder (what file
/// managers and apps expect from a file drag). They only become real files
/// if the drop goes to an app; file-manager drops are copied by DeskLink.
#[implement(IDataObject)]
struct DragFiles {
    hdrop: Vec<u8>,
    drop_effect: u16,
}

impl windows::Win32::System::Com::IDataObject_Impl for DragFiles_Impl {
    fn GetData(&self, f: *const FORMATETC) -> windows_core::Result<windows::Win32::System::Com::STGMEDIUM> {
        let f = unsafe { f.as_ref() }.ok_or(windows::Win32::Foundation::DV_E_FORMATETC)?;
        if f.tymed & TYMED_HGLOBAL.0 as u32 != 0 {
            if f.cfFormat == CF_HDROP {
                return vfiles::medium_hglobal(&self.hdrop);
            }
            if f.cfFormat == self.drop_effect {
                return vfiles::medium_hglobal(&DROPEFFECT_COPY.0.to_le_bytes());
            }
        }
        Err(windows::Win32::Foundation::DV_E_FORMATETC.into())
    }

    fn GetDataHere(&self, _: *const FORMATETC, _: *mut windows::Win32::System::Com::STGMEDIUM) -> windows_core::Result<()> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }

    fn QueryGetData(&self, f: *const FORMATETC) -> HRESULT {
        match unsafe { f.as_ref() } {
            Some(f) if f.tymed & TYMED_HGLOBAL.0 as u32 != 0 && (f.cfFormat == CF_HDROP || f.cfFormat == self.drop_effect) => S_OK,
            _ => windows::Win32::Foundation::DV_E_FORMATETC,
        }
    }

    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        if let Some(o) = unsafe { out.as_mut() } {
            o.ptd = std::ptr::null_mut();
        }
        windows::Win32::Foundation::E_NOTIMPL
    }

    fn SetData(&self, _: *const FORMATETC, m: *const windows::Win32::System::Com::STGMEDIUM, release: BOOL) -> windows_core::Result<()> {
        if release.as_bool() && !m.is_null() {
            unsafe { ReleaseStgMedium(m as *mut _) };
        }
        Ok(())
    }

    fn EnumFormatEtc(&self, dir: u32) -> windows_core::Result<windows::Win32::System::Com::IEnumFORMATETC> {
        if dir != windows::Win32::System::Com::DATADIR_GET.0 as u32 {
            return Err(windows::Win32::Foundation::E_NOTIMPL.into());
        }
        let fe = |cf: u16| FORMATETC { cfFormat: cf, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 };
        unsafe { windows::Win32::UI::Shell::SHCreateStdEnumFmtEtc(&[fe(CF_HDROP), fe(self.drop_effect)]) }
    }

    fn DAdvise(&self, _: *const FORMATETC, _: u32, _: Ref<windows::Win32::System::Com::IAdviseSink>) -> windows_core::Result<u32> {
        Err(windows::Win32::Foundation::OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _: u32) -> windows_core::Result<()> {
        Err(windows::Win32::Foundation::OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows_core::Result<windows::Win32::System::Com::IEnumSTATDATA> {
        Err(windows::Win32::Foundation::OLE_E_ADVISENOTSUPPORTED.into())
    }
}

/// Everything needed to finish the drop when the button is released.
struct DropContext {
    offer: FileOffer,
    opts: DragOptions,
    /// Where the promised files go if an app (not a file manager) gets them.
    cache: PathBuf,
}

#[implement(IDropSource)]
struct DragSource {
    cancel: Arc<AtomicBool>,
    /// Released over a file manager: copy there ourselves.
    copy_to: Rc<RefCell<Option<PathBuf>>>,
    decided: Cell<bool>,
    ctx: Rc<DropContext>,
    /// Last (target effect, over a file manager) logged; only changes are.
    last_feedback: Cell<Option<(u32, bool)>>,
}

impl IDropSource_Impl for DragSource_Impl {
    fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
        if self.cancel.load(Ordering::Acquire) || fescapepressed.as_bool() {
            return DRAGDROP_S_CANCEL;
        }
        if grfkeystate.0 & MK_LBUTTON.0 != 0 {
            return S_OK; // still dragging
        }
        if self.decided.replace(true) {
            return DRAGDROP_S_CANCEL;
        }
        let mut pt = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut pt);
        }
        match ui::folder_at_point(pt.x, pt.y) {
            Some(folder) => {
                log::info!("released over folder {}", folder.display());
                *self.copy_to.borrow_mut() = Some(folder);
                DRAGDROP_S_CANCEL
            }
            None => {
                // An app: make the promised files real first (nothing was
                // transferred before the release), then let it have them.
                log::info!("released over an app at ({}, {}): fetching the files first", pt.x, pt.y);
                // The real files overwrite the stand-ins (folders merge).
                let c = &self.ctx;
                let ok = transfer::copy_with_dialog_result(transfer::CopyRequest {
                    source: Box::new(NetSource { fetch: c.opts.fetch.clone(), peer: c.opts.peer, offer: c.offer.id }),
                    offer: c.offer.clone(),
                    dest: c.cache.clone(),
                    peer_name: c.opts.peer_name.clone(),
                    conflict: Some(Resolution::Overwrite),
                    max_file: c.opts.max_file,
                    max_total: c.opts.max_total,
                });
                if ok { DRAGDROP_S_DROP } else { DRAGDROP_S_CANCEL }
            }
        }
    }

    fn GiveFeedback(&self, effect: DROPEFFECT) -> HRESULT {
        // File managers may refuse our (not yet real) paths and show "no";
        // we copy into them ourselves on release, so show "copy" there.
        let mut pt = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut pt);
        }
        let fm = ui::is_file_manager_at(pt.x, pt.y);
        if self.last_feedback.replace(Some((effect.0, fm))) != Some((effect.0, fm)) {
            log::info!("drag over {:?} at ({}, {}): target effect {}", ui::window_class_at(pt.x, pt.y), pt.x, pt.y, effect.0);
        }
        if fm {
            unsafe {
                windows::Win32::UI::WindowsAndMessaging::SetCursor(Some(ui::drag_copy_cursor()));
            }
            return S_OK;
        }
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

struct NetSource {
    fetch: Fetch,
    peer: PeerId,
    offer: u64,
}

impl FileSource for NetSource {
    fn open(&self, index: u32) -> Result<Box<dyn std::io::Read>, String> {
        (self.fetch)(self.peer, self.offer, index, 0)
    }
}

/// A drag in progress on this thread, started from our anchor window.
struct PendingDrag {
    data: IDataObject,
    source: IDropSource,
    result: Option<HRESULT>,
}

thread_local! {
    static DRAG: RefCell<Option<PendingDrag>> = const { RefCell::new(None) };
}

/// Window procedure of the anchor window: the drag must start from a button
/// press on a window of this thread, so it owns the mouse until release.
unsafe extern "system" fn anchor_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::WM_LBUTTONDOWN;
    if m == WM_LBUTTONDOWN {
        // Out of the way, so targets under the cursor see the drag.
        unsafe {
            let _ = ShowWindow(h, SW_HIDE);
        }
        // Take the objects out so no RefCell borrow spans the modal loop.
        let objs = DRAG.with_borrow(|d| d.as_ref().map(|d| (d.data.clone(), d.source.clone())));
        if let Some((data, source)) = objs {
            let mut effect = DROPEFFECT_NONE;
            let hr = unsafe { DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect) };
            DRAG.with_borrow_mut(|d| {
                if let Some(d) = d {
                    d.result = Some(hr);
                }
            });
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(h, m, w, l) }
}

fn run_drag(offer: FileOffer, opts: DragOptions, cancel: Arc<AtomicBool>) {
    // Big enough that the press lands on it even while the user keeps moving.
    const SIZE: i32 = 400;
    // Targets see paths in our cache; see `DragFiles`.
    let folder_name: String = opts.peer_name.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    // A fresh folder per drag: reusing one fails while something (the shell,
    // a file manager) still holds the old one open, and a deleted folder
    // that is still open can't be created again. Older ones are removed
    // after the drag, when they can be. The last part stays the peer's name,
    // which is what copy dialogs show as the source.
    let root = std::env::temp_dir().join("DeskLink").join("drag");
    let this_drag = root.join(offer.id.to_string());
    let cache = this_drag.join(folder_name);
    let promised: Vec<PathBuf> = offer.top_level().map(|e| cache.join(e.path.replace('/', "\\"))).collect();
    // File managers refuse a drag of paths that don't exist (stop cursor),
    // so put empty stand-ins there. They never get the drop itself: over a
    // file manager we cancel and copy ourselves, and for an app the real
    // files replace them before the drop.
    if let Err(err) = std::fs::create_dir_all(&cache) {
        log::warn!("drag: folder {}: {err}", cache.display());
    }
    for (e, path) in offer.top_level().zip(&promised) {
        let made = if e.is_dir { std::fs::create_dir_all(path) } else { std::fs::File::create(path).map(drop) };
        if let Err(err) = made {
            log::warn!("drag: stand-in {}: {err}", path.display());
        }
    }
    let data: IDataObject = DragFiles { hdrop: crate::clipboard::hdrop_bytes(&promised), drop_effect: vfiles::Formats::register().drop_effect }.into();
    let copy_to = Rc::new(RefCell::new(None));
    let ctx = Rc::new(DropContext { offer, opts, cache });
    let source: IDropSource = DragSource { cancel: cancel.clone(), copy_to: copy_to.clone(), decided: Cell::new(false), ctx: ctx.clone(), last_feedback: Cell::new(None) }.into();
    DRAG.set(Some(PendingDrag { data, source, result: None }));

    // A tiny, invisible window right under the cursor; press the button on
    // it (the release will come from the peer) to start the drag there.
    let wc = WNDCLASSW {
        lpfnWndProc: Some(anchor_proc),
        hInstance: unsafe { GetModuleHandleW(None) }.unwrap_or_default().into(),
        lpszClassName: w!("DeskLinkDragAnchor"),
        ..Default::default()
    };
    unsafe { RegisterClassW(&wc) };
    let mut cur = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut cur);
    }
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("DeskLinkDragAnchor"),
            w!(""),
            WS_POPUP,
            cur.x - SIZE / 2,
            cur.y - SIZE / 2,
            SIZE,
            SIZE,
            None,
            None,
            None,
            None,
        )
    };
    let Ok(hwnd) = hwnd else {
        log::warn!("drag: could not create the anchor window");
        return;
    };
    unsafe {
        let _ = SetLayeredWindowAttributes(hwnd, Default::default(), 1, LWA_ALPHA);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        // Centre it on the cursor again right before pressing.
        let _ = GetCursorPos(&mut cur);
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), cur.x - SIZE / 2, cur.y - SIZE / 2, SIZE, SIZE, SWP_NOACTIVATE);
    }
    log::info!("drag of {} from {} started", desklink_core::files::summary(&ctx.offer), ctx.opts.peer_name);
    crate::inject::raw_button(desklink_proto::MouseButton::Left, true);

    // The press reaches the anchor, which runs the drag (a modal loop that
    // returns on release, Esc or cancel). Give up if it never arrives.
    // While the drag runs we are inside anchor_proc (this closure is not
    // polled), so if no result after 3 s the press never arrived.
    pump_until(Instant::now() + Duration::from_secs(3), || {
        let done = DRAG.with_borrow(|d| d.as_ref().is_some_and(|d| d.result.is_some()));
        done || cancel.load(Ordering::Acquire)
    });
    let result = DRAG.take().and_then(|d| d.result);
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    let folder = copy_to.borrow_mut().take();

    match (result, folder) {
        (Some(_), Some(dest)) => {
            log::info!("dropped into {}", dest.display());
            let c = &ctx;
            transfer::copy_with_dialog(transfer::CopyRequest {
                source: Box::new(NetSource { fetch: c.opts.fetch.clone(), peer: c.opts.peer, offer: c.offer.id }),
                offer: c.offer.clone(),
                dest,
                peer_name: c.opts.peer_name.clone(),
                conflict: c.opts.conflict,
                max_file: c.opts.max_file,
                max_total: c.opts.max_total,
            });
        }
        (Some(hr), None) if hr == DRAGDROP_S_DROP => log::info!("dropped into an app"),
        (Some(_), None) => log::info!("drag cancelled"),
        (None, _) if cancel.load(Ordering::Acquire) => log::warn!("drag cancelled before it started"),
        (None, _) => log::warn!("drag did not start (the button press didn't reach DeskLink's window)"),
    }
    // Earlier drags' folders (an app may still be reading this one's files).
    if let Ok(old) = std::fs::read_dir(&root) {
        for dir in old.flatten().filter(|d| d.path() != this_drag) {
            let _ = std::fs::remove_dir_all(dir.path());
        }
    }
}
