//! Native dialogs and Explorer integration for file transfers.
//!
//! Everything here must run on a thread that called `com_init()` (STA) and
//! may block: callers use a dedicated thread per transfer.

use desklink_core::files::Progress;
use std::path::{Path, PathBuf};
use windows::Win32::Foundation::{HWND, MAX_PATH, POINT};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize, IServiceProvider,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Downloads, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog,
    IFolderView, IPersistFolder2, IProgressDialog, IShellBrowser, IShellItem, IShellWindows, IWebBrowserApp,
    KF_FLAG_DEFAULT, PROGDLG_AUTOTIME, PROGDLG_NORMAL, SHCreateItemFromParsingName,
    SHGetKnownFolderPath, SHGetPathFromIDListW, SID_STopLevelBrowser, SIGDN_FILESYSPATH, ShellWindows,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetClassNameW, GetForegroundWindow, IDNO, IDYES, IsWindowVisible, MB_ICONERROR,
    MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNOCANCEL, MessageBoxW,
};
use windows_core::{HSTRING, Interface, PCWSTR, PWSTR};

/// Keeps COM initialised (single-threaded apartment) for this thread.
pub struct ComGuard(());

pub fn com_init() -> ComGuard {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    ComGuard(())
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

/// The window in front, as a sendable handle (capture it immediately when a
/// hotkey fires, before any dialog takes the foreground).
pub fn foreground_window() -> isize {
    unsafe { GetForegroundWindow().0 as isize }
}

/// The folder shown by the Explorer window `hwnd` (its active tab), or the
/// Desktop if `hwnd` is the desktop.
pub fn folder_for_window(hwnd: isize) -> Option<PathBuf> {
    let hwnd = HWND(hwnd as *mut _);
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    let mut class = [0u16; 64];
    let n = unsafe { GetClassNameW(root, &mut class) } as usize;
    match String::from_utf16_lossy(&class[..n]).as_str() {
        "Progman" | "WorkerW" => known_folder(&FOLDERID_Desktop),
        "CabinetWClass" | "ExploreWClass" => explorer_folder(root),
        "TTOTAL_CMD" => total_commander_folder(root),
        _ => None,
    }
}

fn explorer_folder(root: HWND) -> Option<PathBuf> {
    unsafe {
        let windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER).ok()?;
        let count = windows.Count().ok()?;
        let mut fallback = None;
        for i in 0..count {
            let Ok(disp) = windows.Item(&VARIANT::from(i)) else { continue };
            let Ok(app) = disp.cast::<IWebBrowserApp>() else { continue };
            if app.HWND().map(|h| h.0 as isize) != Ok(root.0 as isize) {
                continue;
            }
            // Windows 11 tabs share one top-level window: pick the tab whose
            // view is visible.
            let Ok(sp) = app.cast::<IServiceProvider>() else { continue };
            let Ok(browser) = sp.QueryService::<IShellBrowser>(&SID_STopLevelBrowser) else { continue };
            let Ok(view) = browser.QueryActiveShellView() else { continue };
            let visible = view.GetWindow().is_ok_and(|w| IsWindowVisible(w).as_bool());
            let Some(path) = view_path(&view.cast::<IFolderView>().ok()?) else { continue };
            if visible {
                return Some(path);
            }
            fallback.get_or_insert(path);
        }
        fallback
    }
}



/// Is a file manager window (Total Commander, Explorer, the desktop) at
/// screen point (x, y)? Cheap: no COM, just the window class.
pub fn is_file_manager_at(x: i32, y: i32) -> bool {
    matches!(
        window_class_at(x, y).as_str(),
        "TTOTAL_CMD" | "CabinetWClass" | "ExploreWClass" | "Progman" | "WorkerW"
    )
}

/// Class name of the top-level window under the point ("" if none).
pub fn window_class_at(x: i32, y: i32) -> String {
    use windows::Win32::UI::WindowsAndMessaging::WindowFromPoint;
    let hit = unsafe { WindowFromPoint(POINT { x, y }) };
    if hit.is_invalid() {
        return String::new();
    }
    let root = unsafe { GetAncestor(hit, GA_ROOT) };
    let mut class = [0u16; 64];
    let n = unsafe { GetClassNameW(root, &mut class) } as usize;
    String::from_utf16_lossy(&class[..n])
}

/// OLE's own "copy" drag cursor (arrow with +), or the arrow if missing.
pub fn drag_copy_cursor() -> windows::Win32::UI::WindowsAndMessaging::HCURSOR {
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{IDC_ARROW, LoadCursorW};
    use windows_core::PCWSTR;
    static CURSOR: std::sync::OnceLock<isize> = std::sync::OnceLock::new();
    let h = *CURSOR.get_or_init(|| unsafe {
        // ole32.dll cursor resource 3 is the drag-copy cursor.
        GetModuleHandleW(windows_core::w!("ole32.dll"))
            .ok()
            .and_then(|m| LoadCursorW(Some(m.into()), PCWSTR(3 as *const u16)).ok())
            .or_else(|| LoadCursorW(None, IDC_ARROW).ok())
            .map_or(0, |c| c.0 as isize)
    });
    windows::Win32::UI::WindowsAndMessaging::HCURSOR(h as *mut _)
}

/// Make Total Commander re-read its panels (it doesn't always notice files
/// written by another process): cm_RereadSource via WM_USER+51.
pub fn refresh_total_commander() {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, PostMessageW, WM_USER};
    const CM_REREAD_SOURCE: usize = 540;
    let mut hwnd = HWND::default();
    unsafe {
        while let Ok(h) = FindWindowExW(None, Some(hwnd), windows_core::w!("TTOTAL_CMD"), None) {
            if h.is_invalid() {
                break;
            }
            let _ = PostMessageW(Some(h), WM_USER + 51, WPARAM(CM_REREAD_SOURCE), LPARAM(0));
            hwnd = h;
        }
    }
}
/// The folder a drop at screen point (x, y) would go into: the Total
/// Commander panel under it, the Explorer window's folder, or the desktop.
pub fn folder_at_point(x: i32, y: i32) -> Option<PathBuf> {
    use windows::Win32::UI::WindowsAndMessaging::WindowFromPoint;
    let hit = unsafe { WindowFromPoint(POINT { x, y }) };
    if hit.is_invalid() {
        return None;
    }
    let root = unsafe { GetAncestor(hit, GA_ROOT) };
    let mut class = [0u16; 64];
    let n = unsafe { GetClassNameW(root, &mut class) } as usize;
    match String::from_utf16_lossy(&class[..n]).as_str() {
        "TTOTAL_CMD" => total_commander_panel_at(root, hit).or_else(|| total_commander_folder(root)),
        _ => folder_for_window(root.0 as isize),
    }
}

/// TC panel (left/right file list) containing window `hit`, by its path label.
fn total_commander_panel_at(tc: HWND, hit: HWND) -> Option<PathBuf> {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowTextW, IsChild, SendMessageW, WM_USER};
    unsafe {
        let ask = |w: usize| HWND(SendMessageW(tc, WM_USER + 50, Some(WPARAM(w)), Some(LPARAM(0))).0 as *mut _);
        for (list, label) in [(1usize, 9usize), (2, 10)] {
            let l = ask(list);
            if !l.is_invalid() && (l == hit || IsChild(l, hit).as_bool()) {
                let mut buf = [0u16; 1024];
                let n = GetWindowTextW(ask(label), &mut buf) as usize;
                return tc_path_from_label(&String::from_utf16_lossy(&buf[..n]));
            }
        }
        None
    }
}
/// Total Commander's active panel folder (TC 9+): `WM_USER+50` with 1000
/// returns the active side (1 = left, 2 = right); with 9/10 it returns the
/// left/right path label, whose text is like `C:\dir\*.*`.
fn total_commander_folder(tc: HWND) -> Option<PathBuf> {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowTextW, SendMessageW, WM_USER};
    unsafe {
        let ask = |w: usize| SendMessageW(tc, WM_USER + 50, Some(WPARAM(w)), Some(LPARAM(0))).0;
        let side = match ask(1000) {
            s @ (1 | 2) => s as usize,
            _ => return None,
        };
        let label = HWND(ask(8 + side) as *mut _);
        if label.is_invalid() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let n = GetWindowTextW(label, &mut buf) as usize;
        tc_path_from_label(&String::from_utf16_lossy(&buf[..n]))
    }
}

/// `C:\dir\*.*` -> `C:\dir`; `C:\*.*` -> `C:\`. Virtual locations (FTP,
/// plugins, archives) aren't real folders: `None`.
fn tc_path_from_label(text: &str) -> Option<PathBuf> {
    let text = text.trim();
    let dir = match text.rfind('\\') {
        Some(i) if text[i + 1..].contains(['*', '?']) || text[i + 1..].is_empty() => &text[..=i],
        _ => text,
    };
    let dir = if dir.len() > 3 { dir.trim_end_matches('\\') } else { dir };
    let p = PathBuf::from(dir);
    p.is_dir().then_some(p)
}

fn view_path(view: &IFolderView) -> Option<PathBuf> {
    unsafe {
        let folder: IPersistFolder2 = view.GetFolder().ok()?;
        let pidl = folder.GetCurFolder().ok()?;
        let mut buf = [0u16; MAX_PATH as usize];
        let ok = SHGetPathFromIDListW(pidl, &mut buf).as_bool();
        CoTaskMemFree(Some(pidl as *const _));
        ok.then(|| PathBuf::from(from_wide_nul(&buf)))
    }
}

fn known_folder(id: &windows_core::GUID) -> Option<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s.map(PathBuf::from)
    }
}

pub fn downloads_folder() -> PathBuf {
    known_folder(&FOLDERID_Downloads).unwrap_or_else(|| PathBuf::from("."))
}

fn from_wide_nul(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Standard "Select Folder" dialog.
pub fn pick_folder(initial: &Path, title: &str) -> Option<PathBuf> {
    unsafe {
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let opts = dlg.GetOptions().ok()?;
        dlg.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        let _ = dlg.SetTitle(&HSTRING::from(title));
        if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(initial.as_os_str()), None) {
            let _ = dlg.SetFolder(&item);
        }
        dlg.Show(None).ok()?; // Err when cancelled
        let item = dlg.GetResult().ok()?;
        let p: PWSTR = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s.map(PathBuf::from)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Confirm {
    Copy,
    ChooseFolder,
    Cancel,
}

pub fn confirm(title: &str, text: &str) -> Confirm {
    let r = message_box(title, text, MB_YESNOCANCEL | MB_ICONQUESTION);
    match r {
        x if x == IDYES.0 => Confirm::Copy,
        x if x == IDNO.0 => Confirm::ChooseFolder,
        _ => Confirm::Cancel,
    }
}

pub fn show_error(title: &str, text: &str) {
    message_box(title, text, MB_OK | MB_ICONERROR);
}

pub fn show_info(title: &str, text: &str) {
    message_box(title, text, MB_OK | MB_ICONINFORMATION);
}

fn message_box(title: &str, text: &str, style: windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_STYLE) -> i32 {
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(text),
            &HSTRING::from(title),
            style | MB_TOPMOST | MB_SETFOREGROUND,
        )
        .0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictChoice {
    Replace,
    KeepBoth,
    Skip,
    Cancel,
}

/// "File already exists" question. Returns the choice and whether to apply
/// it to all remaining conflicts. `owner`: window to center on (e.g. the
/// progress dialog), 0 for none.
pub fn ask_conflict(owner: isize, existing: &Path, incoming_size: u64, incoming_modified_ms: u64) -> (ConflictChoice, bool) {
    use windows::Win32::UI::Controls::{
        TASKDIALOG_BUTTON, TASKDIALOGCONFIG, TDCBF_CANCEL_BUTTON, TDF_ALLOW_DIALOG_CANCELLATION,
        TDF_POSITION_RELATIVE_TO_WINDOW, TDF_USE_COMMAND_LINKS,
    };
    const REPLACE: i32 = 101;
    const KEEP: i32 = 102;
    const SKIP: i32 = 103;

    let name = existing.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (old_size, old_time) = std::fs::metadata(existing)
        .map(|m| (m.len(), m.modified().ok().map(format_time).unwrap_or_default()))
        .unwrap_or_default();
    let new_time = format_time(std::time::UNIX_EPOCH + std::time::Duration::from_millis(incoming_modified_ms));
    let size = desklink_core::files::human_size;

    let title = HSTRING::from("DeskLink: file already exists");
    let main = HSTRING::from(format!("\"{name}\" already exists in this folder"));
    let content = HSTRING::from(format!(
        "{}\n\nExisting: {}, {old_time}\nIncoming: {}, {new_time}",
        existing.parent().map(|p| p.display().to_string()).unwrap_or_default(),
        size(old_size),
        size(incoming_size),
    ));
    let texts = [
        HSTRING::from("Replace the file\nThe existing file is replaced once the new one has fully arrived"),
        HSTRING::from("Keep both\nThe new file gets a number added to its name"),
        HSTRING::from("Skip this file\nKeep the existing file"),
    ];
    let buttons = [
        TASKDIALOG_BUTTON { nButtonID: REPLACE, pszButtonText: PCWSTR(texts[0].as_ptr()) },
        TASKDIALOG_BUTTON { nButtonID: KEEP, pszButtonText: PCWSTR(texts[1].as_ptr()) },
        TASKDIALOG_BUTTON { nButtonID: SKIP, pszButtonText: PCWSTR(texts[2].as_ptr()) },
    ];
    let verify = HSTRING::from("Do this for all remaining conflicts");
    let owner = HWND(owner as *mut _);
    let config = TASKDIALOGCONFIG {
        cbSize: size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: owner,
        dwFlags: TDF_USE_COMMAND_LINKS | TDF_ALLOW_DIALOG_CANCELLATION | TDF_POSITION_RELATIVE_TO_WINDOW,
        dwCommonButtons: TDCBF_CANCEL_BUTTON,
        pszWindowTitle: PCWSTR(title.as_ptr()),
        pszMainInstruction: PCWSTR(main.as_ptr()),
        pszContent: PCWSTR(content.as_ptr()),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: KEEP,
        pszVerificationText: PCWSTR(verify.as_ptr()),
        ..Default::default()
    };
    let mut pressed = 0i32;
    let mut all = windows_core::BOOL(0);
    let ok = with_common_controls_v6(|| unsafe {
        task_dialog_indirect(&config, &mut pressed, &mut all)
    });
    if !ok {
        // No task dialog available: fall back to a plain question.
        let text = format!(
            "\"{name}\" already exists.\n\nYes — replace it\nNo — keep both (number the new one)\nCancel — skip it"
        );
        return match confirm("DeskLink: file already exists", &text) {
            Confirm::Copy => (ConflictChoice::Replace, false),
            Confirm::ChooseFolder => (ConflictChoice::KeepBoth, false),
            Confirm::Cancel => (ConflictChoice::Skip, false),
        };
    }
    let choice = match pressed {
        REPLACE => ConflictChoice::Replace,
        KEEP => ConflictChoice::KeepBoth,
        SKIP => ConflictChoice::Skip,
        _ => ConflictChoice::Cancel,
    };
    (choice, all.as_bool())
}



/// Whether the task dialog can be shown (Common Controls v6 found).
pub fn task_dialog_available() -> bool {
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    with_common_controls_v6(|| unsafe {
        LoadLibraryW(windows_core::w!("comctl32.dll"))
            .ok()
            .and_then(|lib| GetProcAddress(lib, windows_core::s!("TaskDialogIndirect")))
            .is_some()
    })
}

/// `TaskDialogIndirect`, resolved at run time. It exists only in Common
/// Controls v6; importing it statically makes Windows refuse to start an exe
/// without a v6 manifest. Must be called with the v6 context active.
unsafe fn task_dialog_indirect(
    config: &windows::Win32::UI::Controls::TASKDIALOGCONFIG,
    pressed: &mut i32,
    verification: &mut windows_core::BOOL,
) -> bool {
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    type TaskDialogIndirectFn = unsafe extern "system" fn(
        *const windows::Win32::UI::Controls::TASKDIALOGCONFIG,
        *mut i32,
        *mut i32,
        *mut windows_core::BOOL,
    ) -> windows_core::HRESULT;
    unsafe {
        let Ok(lib) = LoadLibraryW(windows_core::w!("comctl32.dll")) else { return false };
        let Some(proc_) = GetProcAddress(lib, windows_core::s!("TaskDialogIndirect")) else {
            return false;
        };
        let f: TaskDialogIndirectFn = std::mem::transmute(proc_);
        f(config, pressed, std::ptr::null_mut(), verification).is_ok()
    }
}
fn format_time(t: std::time::SystemTime) -> String {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) else { return String::new() };
    // FILETIME: 100 ns ticks since 1601.
    let ticks = d.as_nanos() as u64 / 100 + 116_444_736_000_000_000;
    let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    let (mut utc, mut local) = (SYSTEMTIME::default(), SYSTEMTIME::default());
    unsafe {
        if FileTimeToSystemTime(&ft, &mut utc).is_err() || SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).is_err() {
            return String::new();
        }
    }
    format!("{:04}-{:02}-{:02} {:02}:{:02}", local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute)
}

/// Task dialogs need Common Controls v6, which an exe without a manifest
/// doesn't get. Activate it at runtime around the call.
fn with_common_controls_v6<R>(f: impl FnOnce() -> R) -> R {
    use std::sync::OnceLock;
    use windows::Win32::System::ApplicationInstallationAndServicing::{
        ACTCTXW, ActivateActCtx, CreateActCtxW, DeactivateActCtx,
    };
    static CTX: OnceLock<Option<isize>> = OnceLock::new();
    let ctx = *CTX.get_or_init(|| {
        const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency><dependentAssembly>
    <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0"
      processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
  </dependentAssembly></dependency>
</assembly>"#;
        let path = std::env::temp_dir().join("DeskLink").join("comctl6.manifest");
        std::fs::create_dir_all(path.parent()?).ok()?;
        std::fs::write(&path, MANIFEST).ok()?;
        let src = HSTRING::from(path.as_os_str());
        let act = ACTCTXW { cbSize: size_of::<ACTCTXW>() as u32, lpSource: PCWSTR(src.as_ptr()), ..Default::default() };
        unsafe { CreateActCtxW(&act).ok().map(|h| h.0 as isize) }
    });
    let mut cookie = 0usize;
    let active = ctx.is_some_and(|h| unsafe { ActivateActCtx(Some(windows::Win32::Foundation::HANDLE(h as *mut _)), &mut cookie).is_ok() });
    let r = f();
    if active {
        unsafe {
            let _ = DeactivateActCtx(0, cookie);
        }
    }
    r
}

/// CLSID_ProgressDialog (shlobj.h); not exported by the `windows` crate.
const CLSID_PROGRESS_DIALOG: windows_core::GUID = windows_core::GUID::from_u128(0xF8383852_FCD3_11d1_A6B9_006097DF5BD4);

/// Windows' own copy-progress dialog (progress bar, time left, Cancel).
pub struct ProgressDialog {
    dlg: IProgressDialog,
    started: std::time::Instant,
    last_stats: std::time::Instant,
    files_left: usize,
}

impl ProgressDialog {
    /// Shows the dialog in front of the user's window without stealing
    /// focus (Windows forbids background apps from doing that): it is owned
    /// by the window in front, and owned windows stay above their owner.
    pub fn start(title: &str) -> Option<ProgressDialog> {
        unsafe {
            let dlg: IProgressDialog = CoCreateInstance(&CLSID_PROGRESS_DIALOG, None, CLSCTX_INPROC_SERVER).ok()?;
            let _ = dlg.SetTitle(&HSTRING::from(title));
            let owner = GetForegroundWindow();
            let owner = (!owner.is_invalid()).then_some(owner);
            dlg.StartProgressDialog(owner, None, PROGDLG_NORMAL | PROGDLG_AUTOTIME, None).ok()?;
            let now = std::time::Instant::now();
            Some(ProgressDialog { dlg, started: now, last_stats: now - std::time::Duration::from_secs(1), files_left: 0 })
        }
    }

    /// The dialog window (to own follow-up questions), or 0.
    pub fn hwnd(&self) -> isize {
        use windows::Win32::System::Ole::IOleWindow;
        unsafe { self.dlg.cast::<IOleWindow>().and_then(|w| w.GetWindow()).map_or(0, |h| h.0 as isize) }
    }

    fn line(&self, n: u32, text: &str) {
        unsafe {
            let _ = self.dlg.SetLine(n, &HSTRING::from(text), true, None);
        }
    }
}

impl Progress for ProgressDialog {
    fn update(&mut self, done: u64, total: u64, _current: &str) {
        // Line 2: files remaining and speed, refreshed twice a second.
        if self.last_stats.elapsed() >= std::time::Duration::from_millis(500) {
            self.last_stats = std::time::Instant::now();
            let secs = self.started.elapsed().as_secs_f64();
            let mut stats = format!("Files remaining: {}", self.files_left);
            if secs > 0.3 && done > 0 {
                let speed = (done as f64 / secs) as u64;
                stats.push_str(&format!("   ·   {}/s", desklink_core::files::human_size(speed)));
            }
            self.line(2, &stats);
        }
        unsafe {
            let _ = self.dlg.SetProgress64(done, total.max(1));
        }
    }

    fn file_started(&mut self, path: &str, files_left: usize) {
        self.files_left = files_left;
        let name = path.rsplit('/').next().unwrap_or(path);
        self.line(1, &format!("Copying {name}"));
        // Refresh line 2 on the next update.
        self.last_stats = std::time::Instant::now() - std::time::Duration::from_secs(1);
    }

    fn cancelled(&self) -> bool {
        unsafe { self.dlg.HasUserCancelled().as_bool() }
    }
}

impl Drop for ProgressDialog {
    fn drop(&mut self) {
        unsafe {
            let _ = self.dlg.StopProgressDialog();
        }
    }
}


#[cfg(test)]
mod tests {
    use super::tc_path_from_label;
    use std::path::PathBuf;

    #[test]
    fn conflict_dialog_is_available_at_runtime() {
        assert!(super::task_dialog_available());
    }

    #[test]
    fn total_commander_labels() {
        let tmp = std::env::temp_dir();
        let t = tmp.to_string_lossy().trim_end_matches('\\').to_string();
        assert_eq!(tc_path_from_label(&format!(r"{t}\*.*")), Some(PathBuf::from(&t)));
        assert_eq!(tc_path_from_label(&format!(r"{t}\*.txt")), Some(PathBuf::from(&t)));
        assert_eq!(tc_path_from_label(r"C:\*.*"), Some(PathBuf::from(r"C:\")));
        assert_eq!(tc_path_from_label(r"\\\FTP\server\*.*"), None);
        assert_eq!(tc_path_from_label(r"Z:\does-not-exist-desklink\*.*"), None);
    }
}

/// Put plain text on the clipboard (e.g. "Copy all" in the log).
pub fn copy_text(text: &str) -> bool {
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    const CF_UNICODETEXT: u32 = 13;
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    // Another program may hold the clipboard for a moment.
    if !(0..10).any(|_| unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 || {
        std::thread::sleep(std::time::Duration::from_millis(20));
        false
    }) {
        return false;
    }
    let ok = unsafe {
        EmptyClipboard();
        let mem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2);
        let p = GlobalLock(mem) as *mut u16;
        if p.is_null() {
            false
        } else {
            std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
            GlobalUnlock(mem);
            if SetClipboardData(CF_UNICODETEXT, mem).is_null() {
                GlobalFree(mem);
                false
            } else {
                true // the clipboard owns the memory now
            }
        }
    };
    unsafe { CloseClipboard() };
    ok
}
