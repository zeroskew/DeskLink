//! `desklink.cfg`: machine name, peers and their side, cursor, hotkeys, log.

use desklink_proto::Edge;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// Defaults owned by other crates, repeated here so the settings window
// doesn't link the network stack or the input hooks; the daemon checks
// they match.
pub const DEFAULT_PORT: u16 = 24801;
pub const DEFAULT_DRIFT_MIN_PX: u32 = 16;
pub const DEFAULT_DRIFT_WINDOW_MS: u32 = 500;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    #[serde(default = "default_name")]
    pub name: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub role: Role,
    /// Run with administrator rights, so the other PC's keyboard and mouse
    /// also work in elevated programs' windows.
    #[serde(default = "default_true")]
    pub run_as_admin: bool,
    /// Windows priority of the DeskLink process (changes live).
    #[serde(default)]
    pub priority: Priority,
    #[serde(default)]
    pub cursor: CursorSettings,
    #[serde(default)]
    pub local_activity: LocalActivity,
    #[serde(default)]
    pub clipboard: ClipboardSettings,
    #[serde(default)]
    pub hotkeys: Hotkeys,
    #[serde(default)]
    pub log: LogSettings,
    #[serde(default)]
    pub security: SecuritySettings,
    #[serde(default)]
    pub updates: UpdateSettings,
    #[serde(default, rename = "peer")]
    pub peers: Vec<Peer>,
}




#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PasteMode {
    /// Virtual files: the target app asks about conflicts first and pulls
    /// only the files it keeps, in one progress dialog. The app writes the
    /// destination directly, so cancelling a Replace loses the old file.
    Virtual,
    /// Download into a temp folder when pasted (DeskLink's progress window:
    /// files remaining, speed, time left), then the app copies locally.
    /// Cancelling the transfer never touches the destination. Works in any
    /// app that pastes files.
    Download,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum OnConflict {
    /// Replace / Keep both / Skip, with "do this for all".
    Ask,
    Overwrite,
    /// Keep both: the new file gets " (2)".
    Rename,
    Skip,
}

impl OnConflict {
    /// For the copy engine: `None` means ask.
    pub fn resolution(self) -> Option<desklink_core::files::Resolution> {
        use desklink_core::files::Resolution;
        match self {
            OnConflict::Ask => None,
            OnConflict::Overwrite => Some(Resolution::Overwrite),
            OnConflict::Rename => Some(Resolution::Rename),
            OnConflict::Skip => Some(Resolution::Skip),
        }
    }
}
/// How files copied on another machine are pasted here.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FilesMode {
    /// Paste with Ctrl+V in any app; files transfer only when pasted.
    Auto,
    /// Ask where to save them as soon as you arrive.
    Dialog,
}

/// Shared clipboard: when the user moves to another machine, this machine's
/// clipboard goes with them (if it changed). Both machines must enable it.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ClipboardSettings {
    pub enabled: bool,
    /// Plain text, HTML and RTF.
    pub text: bool,
    pub images: bool,
    /// Larger clipboards are not shared (uncompressed size).
    pub max_size_mb: u32,
    /// Never share content that password managers mark as private.
    pub respect_private: bool,
    /// Share files copied in Explorer.
    pub files: bool,
    pub files_mode: FilesMode,
    /// Largest single file sent or received, in MB (0 = unlimited).
    pub max_file_mb: u64,
    /// Largest copy (all files at once) sent or received, in MB (0 = unlimited).
    pub max_total_mb: u64,
    /// When a file already exists where the paste-files hotkey (or the
    /// dialog mode) saves. With Ctrl+V the pasting app asks instead.
    pub on_conflict: OnConflict,
    /// How Ctrl+V pastes files from another machine.
    pub paste_mode: PasteMode,
}

impl Default for ClipboardSettings {
    fn default() -> Self {
        ClipboardSettings {
            enabled: true,
            text: true,
            images: true,
            max_size_mb: 64,
            respect_private: true,
            files: true,
            files_mode: FilesMode::Auto,
            max_file_mb: 0,
            max_total_mb: 0,
            on_conflict: OnConflict::Ask,
            paste_mode: PasteMode::Download,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct LogSettings {
    /// Also write the log to a text file.
    pub file: bool,
    /// error | warn | info | debug | trace
    pub level: String,
    /// File name or path; relative paths are next to the config file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Rotate to `<file>.1` beyond this size (0 = never).
    pub max_size_mb: u32,
}

impl Default for LogSettings {
    fn default() -> Self {
        LogSettings { file: false, level: "info".into(), path: None, max_size_mb: 10 }
    }
}

/// Who may talk to this PC. Off: any PC that knows our identity's pin.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct SecuritySettings {
    pub enabled: bool,
    /// Networks such as "192.168.1.0/24"; a bare address is just that one.
    pub networks: Vec<String>,
    /// PC names or addresses.
    pub allowed: Vec<String>,
}

/// Self-update from the GitHub releases.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct UpdateSettings {
    /// Look for a new release by itself (a minute after starting, then
    /// every `check_every_hours`).
    pub auto_check: bool,
    pub check_every_hours: u32,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        UpdateSettings { auto_check: true, check_every_hours: 12 }
    }
}

/// What happens to this machine's cursor while it isn't the one in use.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct CursorSettings {
    pub method: HideMethod,
    /// Where this machine parks its cursor while controlling another one.
    pub park: ParkAt,
    /// Hide while this machine's keyboard/mouse control another machine.
    pub hide_while_directing: bool,
    /// Hide after another machine stops controlling this one.
    pub hide_after_released: bool,
    /// Local mouse movement brings a hidden cursor back.
    pub reveal_on_mouse: bool,
    /// Local key press brings a hidden cursor back.
    pub reveal_on_keyboard: bool,
    /// Corners of the screens lead to other PCs too.
    pub switch_at_corners: bool,
    /// Pixels at each corner that don't switch (unless `switch_at_corners`).
    pub corner_size: u32,
    /// Leave a game holding the mouse (pointer hidden while playing): to
    /// another PC, or another screen of this one.
    pub switch_in_fullscreen: bool,
}

impl Default for CursorSettings {
    fn default() -> Self {
        CursorSettings {
            method: HideMethod::Transparent,
            park: ParkAt::BottomRight,
            hide_while_directing: true,
            hide_after_released: true,
            reveal_on_mouse: true,
            reveal_on_keyboard: true,
            switch_at_corners: false,
            corner_size: 30,
            switch_in_fullscreen: false,
        }
    }
}

/// What counts as someone using this machine directly: it takes the machine
/// back from a controlling peer and reveals a hidden cursor.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct LocalActivity {
    /// Micro-drift protection: net mouse movement (physical pixels) needed
    /// within `window_ms`.
    /// 0 = any movement counts.
    pub min_mouse_move: u32,
    pub window_ms: u32,
    /// Does typing on a controlled PC's own keyboard take it back? Off: the
    /// keys work there and the controlling PC keeps the cursor.
    pub keyboard_takes_over: bool,
    /// Do clicks and the wheel on a controlled PC's own mouse take it back?
    /// (Moving it always does.)
    pub clicks_take_over: bool,
}

impl Default for LocalActivity {
    fn default() -> Self {
        LocalActivity {
            min_mouse_move: DEFAULT_DRIFT_MIN_PX,
            window_ms: DEFAULT_DRIFT_WINDOW_MS,
            keyboard_takes_over: false,
            clicks_take_over: false,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum HideMethod {
    /// Swap the system cursors for a transparent one (apps that draw their
    /// own cursor may still show it, hence parking in a corner).
    Transparent,
    /// Don't hide; only park the cursor at `park`.
    ParkOnly,
    /// Leave the cursor visible where it crossed the edge (no hiding, no
    /// parking).
    None,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ParkAt {
    BottomRight,
    BottomLeft,
    TopRight,
    TopLeft,
    Center,
}

impl From<ParkAt> for desklink_core::geometry::Park {
    fn from(p: ParkAt) -> Self {
        use desklink_core::geometry::Park;
        match p {
            ParkAt::BottomRight => Park::BottomRight,
            ParkAt::BottomLeft => Park::BottomLeft,
            ParkAt::TopRight => Park::TopRight,
            ParkAt::TopLeft => Park::TopLeft,
            ParkAt::Center => Park::Center,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Hotkeys {
    /// Take the keyboard and mouse back to this machine.
    #[serde(default = "default_return")]
    pub return_to_local: String,
    /// Paste files copied on another machine into the folder in front of you
    /// (asks first). Acts on the machine you are currently using.
    #[serde(default = "default_paste_files")]
    pub paste_files: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Hotkeys { return_to_local: default_return(), paste_files: default_paste_files() }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl From<Side> for Edge {
    fn from(s: Side) -> Edge {
        match s {
            Side::Left => Edge::Left,
            Side::Right => Edge::Right,
            Side::Top => Edge::Top,
            Side::Bottom => Edge::Bottom,
        }
    }
}

impl std::str::FromStr for Side {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "left" => Ok(Side::Left),
            "right" => Ok(Side::Right),
            "top" | "up" => Ok(Side::Top),
            "bottom" | "down" => Ok(Side::Bottom),
            _ => Err(format!("unknown side '{s}' (left, right, top, bottom)")),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Peer {
    /// The peer's machine name (as it reports itself).
    pub name: String,
    /// Host name or IP to dial, optionally `:port`. Defaults to `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// Which side of *this* machine the peer's screens are on.
    pub side: Side,
    /// Certificate fingerprint, pinned automatically on first connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Learned automatically: where the peer was last reached (IP:port).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ip: Option<String>,
    /// Where its screens sit on this PC's desk (added to its own screen
    /// coordinates), set in the Layout tab. Unset: anywhere on `side`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<[i32; 2]>,
    /// Learned automatically: its screens, for the Layout tab.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub monitors: Vec<desklink_proto::Monitor>,
    /// Added but not put on the layout yet: not reachable until it is.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unplaced: bool,
}

impl Peer {
    pub fn new(name: impl Into<String>, side: Side) -> Peer {
        Peer { name: name.into(), address: None, side, fingerprint: None, last_ip: None, position: None, monitors: Vec::new(), unplaced: false }
    }

    /// Added to the list, not yet put on the layout.
    pub fn unplaced(name: impl Into<String>) -> Peer {
        Peer { unplaced: true, ..Peer::new(name, Side::Left) }
    }
}

/// How DeskLink competes with other programs for the CPU. Every mouse move
/// waits for it, so it should never wait its turn behind a busy program.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Priority {
    Normal,
    #[default]
    AboveNormal,
    /// Ahead of everything but the system's own real-time work.
    High,
}

/// How this PC works with the others.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Every PC sets up its own neighbours; any PC's keyboard and mouse can
    /// move to the others.
    #[default]
    Peer,
    /// This PC's keyboard and mouse drive all the others; it holds the whole
    /// layout and finds clients on the network.
    Master,
    /// Driven by a master only; it announces itself on the network and
    /// accepts the first master that connects (then only that one).
    Client,
}

fn default_true() -> bool {
    true
}

fn default_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "desklink".into()).to_lowercase()
}
fn default_port() -> u16 {
    DEFAULT_PORT
}
fn default_paste_files() -> String {
    "LCtrl+LAlt+V".into()
}
fn default_return() -> String {
    "LCtrl+LAlt+Pause".into()
}

const HEADER: &str = "\
# DeskLink configuration ([sections], key = value).
# DeskLink rewrites this file (it pins peer fingerprints and fills in new
# options with their defaults). The comments are written by DeskLink; your
# own comments are not kept.
#
# Add a peer (or run: desklink add-peer <name> <left|right|top|bottom>):
#   [[peer]]
#   name = \"flashunreal\"
#   side = \"left\"
";

/// Comments written above settings whose meaning or values aren't obvious:
/// (section, key, comment). The section of `[[peer]]` entries is "peer".
const KEY_DOCS: &[(&str, &str, &str)] = &[
    ("", "name", "This machine's name as other machines see it (default: the computer name)."),
    ("", "port", "UDP port DeskLink uses. Must be the same on all machines."),
    ("", "run_as_admin", "Run DeskLink with administrator rights (asks when it starts), so the other
PC's keyboard and mouse also work in admin windows (Task Manager,
installers). Dragging files in from normal programs then
stops working: Windows blocks it. true | false"),
    ("", "role", "How this PC works with the others:
  peer   = each PC sets up its own neighbours (any PC's keyboard and mouse
           can move to the others)
  master = this PC's keyboard and mouse drive all the others; it holds the
           whole layout and finds clients on the network
  client = driven by a master only; it announces itself on the network
           and accepts the first master that connects (then only that one)"),
    ("cursor", "method", "How this machine's cursor is hidden while it isn't being used:\n  transparent = hide it (and park it, see `park`)\n  park-only   = keep it visible, parked at `park`\n  none        = keep it visible where it crossed the edge"),
    ("cursor", "park", "Where the cursor waits while you control another machine:\n  bottom-right | bottom-left | top-right | top-left | center"),
    ("cursor", "hide_while_directing", "Hide it while this machine's keyboard/mouse control another machine."),
    ("cursor", "hide_after_released", "Hide it after another machine stops controlling this one."),
    ("cursor", "reveal_on_mouse", "A hidden cursor comes back when this machine's own mouse moves..."),
    ("cursor", "reveal_on_keyboard", "...or when a key is pressed on its own keyboard."),
    ("cursor", "switch_at_corners", "Let the cursor cross to another PC in the corners of the screens too.
Off: the last `corner_size` pixels at each outer corner don't switch, so
aiming at a close button or the Start button never does by accident."),
    ("cursor", "corner_size", "Size of those corners, in pixels."),
    ("cursor", "switch_in_fullscreen", "Let the cursor leave a game that holds the mouse (hides the pointer while
you play, like Unreal's Game mode). Off: meanwhile the cursor stays in the
game: it neither crosses to another PC nor slides onto this PC's other
screens. In the game's menus (pointer shown) it moves freely."),
    ("local_activity", "min_mouse_move", "Micro-drift protection. Mouse movement on this machine's own mouse counts\nas \"someone is using this machine\" (takes it back from a controlling\nmachine, reveals the cursor) only after moving this many physical pixels\nwithin `window_ms`. 0 = any movement."),
    ("local_activity", "window_ms", "Time window for `min_mouse_move`, in milliseconds."),
    ("local_activity", "keyboard_takes_over", "Typing on this machine's own keyboard while another machine controls it\ntakes it back. Off: the keys just work here and the cursor stays with the\ncontrolling machine (moving its own mouse still takes it back)."),
    ("local_activity", "clicks_take_over", "Clicks and the wheel on this machine's own mouse while another machine\ncontrols it take it back. Off: they work here and the cursor stays with the\ncontrolling machine (moving the mouse still takes it back)."),
    ("clipboard", "enabled", "Share the clipboard with other machines (enable it on all of them)."),
    ("clipboard", "max_size_mb", "Largest text/image clipboard shared, in MB (files: see max_file_mb)."),
    ("clipboard", "respect_private", "Never share content that password managers mark as private."),
    ("clipboard", "files", "Share files copied in Explorer / Total Commander."),
    ("clipboard", "files_mode", "How files copied on another machine are pasted here:\n  auto   = with Ctrl+V in any app\n  dialog = ask where to save them as soon as you arrive"),
    ("clipboard", "max_file_mb", "Largest single file sent or received, in MB. 0 = unlimited."),
    ("clipboard", "max_total_mb", "Largest copy (all files at once) sent or received, in MB. 0 = unlimited."),
    ("clipboard", "on_conflict", "When a file already exists where DeskLink copies files (Ctrl+V into\nExplorer, Total Commander or the desktop, and the paste-files hotkey):\n  ask | overwrite | rename (keep both) | skip"),
    ("clipboard", "paste_mode", "How Ctrl+V pastes files from another machine:\n  download = into Explorer, Total Commander or the desktop DeskLink copies\n             them itself (progress window, overwrite questions, the old file\n             is only replaced once the new one fully arrived); other apps\n             get them after DeskLink downloads them\n  virtual  = the app pulls them directly (the app asks about conflicts,\n             but cancelling a Replace loses the old file)"),
    ("hotkeys", "return_to_local", "Take the keyboard and mouse back to this machine.\nFormat: modifiers + key, e.g. LCtrl+LAlt+Pause. Modifiers: LCtrl RCtrl LShift\nRShift LAlt RAlt LWin RWin (Ctrl/Shift/Alt/Win = left). Keys: A-Z 0-9 F1-F24\nPause ScrollLock Insert Delete Home End PageUp PageDown Left Right Up Down\nEsc Space Tab Enter PrintScreen."),
    ("hotkeys", "paste_files", "Paste files copied on another machine into the folder in front of you\n(Explorer, Total Commander, desktop; asks first). Acts on the machine\nyou are using."),
    ("log", "file", "Also write the log to desklink.log next to this file."),
    ("log", "level", "error | warn | info | debug | trace"),
    ("log", "path", "Log file name or full path (default: desklink.log)."),
    ("log", "max_size_mb", "Start a new log file (old one kept as .1) beyond this size, in MB. 0 = never."),
    ("security", "enabled", "Only let the networks and PCs below talk to this PC. true | false"),
    ("security", "networks", "Networks allowed to connect, e.g. [\"192.168.1.0/24\"] (a bare address is
just that one). Empty = any network."),
    ("security", "allowed", "PCs allowed to connect, by name or address, e.g. [\"flashunreal\", \"192.168.1.50\"].
Empty = any PC (on the allowed networks)."),
    ("updates", "auto_check", "Look for a new DeskLink release on GitHub by itself (a minute after it
starts, then every `check_every_hours`). A new one is downloaded in the
background and installed when you pick \"Restart to update\" in the tray
menu or exit DeskLink from the tray. true | false"),
    ("updates", "check_every_hours", "Hours between automatic checks. 0 = only when DeskLink starts."),
    ("peer", "name", "The other machine's name."),
    ("peer", "address", "Host name or IP address, optionally :port (default: the name)."),
    ("peer", "side", "Where its screens are relative to this machine: left | right | top | bottom"),
    ("peer", "fingerprint", "Its identity, pinned on first connection. Delete this line to pair\nagain (e.g. after DeskLink was reinstalled on that machine)."),
    ("peer", "last_ip", "Learned automatically: where it was last reached. DeskLink connects here\nfirst, because looking up a name on a home network can take many seconds."),
    ("peer", "unplaced", "Added but not put on the layout yet (drag it there in the Layout tab):
the cursor can't reach it until then."),
    ("peer", "position", "Where its screens sit on this PC's desk: x, y added to its own screen
coordinates (set by dragging it in the Layout tab). Without it, any point of
`side` leads to it."),
];

/// Insert `KEY_DOCS` comments above the matching keys (first occurrence
/// per section, so a long peer list isn't repeated).
fn annotate(body: &str) -> String {
    let mut out = String::with_capacity(body.len() * 2);
    let mut section = String::new();
    let mut seen: Vec<(String, String)> = Vec::new();
    for line in body.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            section = t.trim_matches(|c| c == '[' || c == ']').to_string();
        } else if let Some((key, _)) = t.split_once('=') {
            let key = key.trim();
            let id = (section.clone(), key.to_string());
            if let Some((_, _, doc)) = KEY_DOCS.iter().find(|(s, k, _)| *s == section && *k == key)
                && !seen.contains(&id)
            {
                seen.push(id);
                for l in doc.lines() {
                    out.push_str("# ");
                    out.push_str(l);
                    out.push('\n');
                }
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

pub const CONFIG_FILE: &str = "desklink.cfg";

/// Where DeskLink keeps its files.
#[derive(Clone, Debug)]
pub struct Paths {
    pub dir: PathBuf,
    pub config: PathBuf,
    pub cert: PathBuf,
    pub key: PathBuf,
}

impl Paths {
    /// Next to the executable (portable), unless that folder isn't writable
    /// (e.g. Program Files), then `%APPDATA%\DeskLink`. `--config` picks the
    /// config file explicitly and keeps the other files beside it.
    pub fn resolve(config_override: Option<PathBuf>) -> Paths {
        let (dir, config) = match config_override {
            Some(c) => {
                let dir = c.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf);
                (dir.unwrap_or_else(|| PathBuf::from(".")), c)
            }
            None => {
                let dir = exe_dir().filter(|d| writable(d)).unwrap_or_else(appdata_dir);
                let config = dir.join(CONFIG_FILE);
                (dir, config)
            }
        };
        Paths { cert: dir.join("desklink.cert"), key: dir.join("desklink.key"), dir, config }
    }

    /// First run of the portable layout: bring over the config and, crucially,
    /// the machine identity from the old `%APPDATA%` location. A fresh
    /// certificate would change our fingerprint and peers would reject us.
    pub fn migrate_legacy(&self) -> Vec<String> {
        let old = appdata_dir();
        let mut done = Vec::new();
        // desklink.toml was renamed to desklink.cfg (same format).
        let toml = self.config.with_extension("toml");
        if self.config.extension().is_some_and(|e| e == "cfg")
            && toml.exists()
            && !self.config.exists()
            && std::fs::rename(&toml, &self.config).is_ok()
        {
            done.push(format!("renamed {} -> {}", toml.display(), self.config.display()));
        }
        for (from, to) in [
            (old.join("config.toml"), &self.config),
            (old.join("cert.der"), &self.cert),
            (old.join("key.der"), &self.key),
        ] {
            if from != *to && from.exists() && !to.exists() && std::fs::copy(&from, to).is_ok() {
                done.push(format!("migrated {} -> {}", from.display(), to.display()));
            }
        }
        done
    }
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

fn appdata_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("DeskLink")
}

fn writable(dir: &Path) -> bool {
    let probe = dir.join(".desklink-write-test");
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}


/// Size assumed for a PC's screen until DeskLink has seen its real ones.
pub const UNKNOWN_SCREEN: (i32, i32) = (1920, 1080);

/// x, y, w, h of each screen (a placeholder when unknown).
fn screen_rects(monitors: &[desklink_proto::Monitor]) -> Vec<(i32, i32, i32, i32)> {
    if monitors.is_empty() {
        return vec![(0, 0, UNKNOWN_SCREEN.0, UNKNOWN_SCREEN.1)];
    }
    monitors.iter().map(|m| (m.x, m.y, m.width, m.height)).collect()
}

/// (x0, y0, x1, y1) around `rects`.
fn screen_bounds(rects: &[(i32, i32, i32, i32)]) -> (i32, i32, i32, i32) {
    rects.iter().fold((i32::MAX, i32::MAX, i32::MIN, i32::MIN), |b, r| {
        (b.0.min(r.0), b.1.min(r.1), b.2.max(r.0 + r.2), b.3.max(r.1 + r.3))
    })
}

/// `at` moved along its side so the screens (bounds `own`, placed at `at`)
/// are centred on the screen they are attached to: the one of `local` whose
/// edge they touch, sharing the most of it. Unchanged if they touch none.
fn centred(side: Side, at: (i32, i32), own: &(i32, i32, i32, i32), local: &[(i32, i32, i32, i32)]) -> (i32, i32) {
    let (left, top, right, bottom) = (at.0 + own.0, at.1 + own.1, at.0 + own.2, at.1 + own.3);
    let vertical = matches!(side, Side::Left | Side::Right);
    let shared = |f: &&(i32, i32, i32, i32)| {
        if vertical {
            bottom.min(f.1 + f.3) - top.max(f.1)
        } else {
            right.min(f.0 + f.2) - left.max(f.0)
        }
    };
    let attached = local
        .iter()
        .filter(|f| match side {
            Side::Left => f.0 == right,
            Side::Right => f.0 + f.2 == left,
            Side::Top => f.1 == bottom,
            Side::Bottom => f.1 + f.3 == top,
        })
        .filter(|f| shared(f) > 0)
        .max_by_key(shared);
    match attached {
        Some(f) if vertical => (at.0, at.1 + (f.1 + f.3 / 2) - (top + bottom) / 2),
        Some(f) => (at.0 + (f.0 + f.2 / 2) - (left + right) / 2, at.1),
        None => at,
    }
}
impl Config {
    /// A peer's screens became known or changed (it connected): keep the
    /// edge facing this PC where the user put it. Returns whether anything
    /// changed.
    pub fn update_peer_monitors(&mut self, i: usize, monitors: Vec<desklink_proto::Monitor>, local: &[desklink_proto::Monitor]) -> bool {
        let Some(peer) = self.peers.get_mut(i) else { return false };
        if peer.monitors == monitors {
            return false;
        }
        if let Some([x, y]) = peer.position {
            let old = screen_bounds(&screen_rects(&peer.monitors));
            let new = screen_bounds(&screen_rects(&monitors));
            // The facing edge stays; along it, the centre stays.
            let mid = |a: i32, b: i32| (a + b) / 2;
            let (mut x, mut y) = match peer.side {
                Side::Left => (x + old.2 - new.2, y + mid(old.1, old.3) - mid(new.1, new.3)),
                Side::Right => (x + old.0 - new.0, y + mid(old.1, old.3) - mid(new.1, new.3)),
                Side::Top => (x + mid(old.0, old.2) - mid(new.0, new.2), y + old.3 - new.3),
                Side::Bottom => (x + mid(old.0, old.2) - mid(new.0, new.2), y + old.1 - new.1),
            };
            // Centred on the screen it is attached to.
            (x, y) = centred(peer.side, (x, y), &new, &screen_rects(local));
            peer.position = Some([x, y]);
        }
        peer.monitors = monitors;
        true
    }

    /// Placed peers sit centred on the screen they are attached to; those
    /// whose screens cover this PC's (e.g. placed before their
    /// real size was known) move out along their side until clear. Returns
    /// whether anything moved.
    pub fn fix_overlaps(&mut self, local: &[desklink_proto::Monitor]) -> bool {
        let fixed = screen_rects(local);
        let mut moved = false;
        for peer in &mut self.peers {
            let Some([mut x, mut y]) = peer.position else { continue };
            let own = screen_rects(&peer.monitors);
            let mut pushed = false;
            for _ in 0..8 {
                let placed: Vec<_> = own.iter().map(|r| (r.0 + x, r.1 + y, r.2, r.3)).collect();
                let hits: Vec<_> = placed
                    .iter()
                    .flat_map(|m| fixed.iter().filter(move |f| m.0 < f.0 + f.2 && f.0 < m.0 + m.2 && m.1 < f.1 + f.3 && f.1 < m.1 + m.3).map(move |f| (m, f)))
                    .collect();
                if hits.is_empty() {
                    break;
                }
                match peer.side {
                    Side::Left => x += hits.iter().map(|(m, f)| f.0 - (m.0 + m.2)).min().unwrap(),
                    Side::Right => x += hits.iter().map(|(m, f)| f.0 + f.2 - m.0).max().unwrap(),
                    Side::Top => y += hits.iter().map(|(m, f)| f.1 - (m.1 + m.3)).min().unwrap(),
                    Side::Bottom => y += hits.iter().map(|(m, f)| f.1 + f.3 - m.1).max().unwrap(),
                }
                pushed = true;
            }
            // Always centred on the screen it is attached to.
            let at = centred(peer.side, (x, y), &screen_bounds(&own), &fixed);
            if pushed || peer.position != Some([at.0, at.1]) {
                peer.position = Some([at.0, at.1]);
                moved = true;
            }
        }
        moved
    }

    /// Every setting at its default.
    pub fn defaults() -> Config {
        toml::from_str("").expect("all fields have defaults")
    }

    pub fn load_or_default(path: &Path) -> Result<Config, String> {
        match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let c = Config::defaults();
                c.save(path)?;
                Ok(c)
            }
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    fn render(&self) -> Result<String, String> {
        let body = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        Ok(format!("{HEADER}\n{}", annotate(&body)))
    }

    /// Same settings (compared as they are written to the file).
    pub fn same_as(&self, other: &Config) -> bool {
        self.render().ok() == other.render().ok()
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        // Write-then-rename so a crash never leaves a truncated config.
        let tmp = path.with_extension("cfg.tmp");
        std::fs::write(&tmp, self.render()?).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())
    }

    /// Rewrite the file only if it differs, e.g. to show newly added options
    /// with their defaults. Returns whether it was written.
    pub fn save_if_changed(&self, path: &Path) -> Result<bool, String> {
        let new = self.render()?;
        if std::fs::read_to_string(path).is_ok_and(|old| old == new) {
            return Ok(false);
        }
        self.save(path).map(|_| true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    fn mon(x: i32, y: i32, w: i32, h: i32) -> desklink_proto::Monitor {
        desklink_proto::Monitor { x, y, width: w, height: h, scale_percent: 100, primary: x == 0 && y == 0, name: String::new() }
    }

    #[test]
    fn real_screens_keep_the_edge_facing_us() {
        let mut c = Config::defaults();
        // Placed left of us while its size was unknown (1920x1080): its
        // right edge touches our left edge at x = 0.
        c.peers.push(Peer { position: Some([-1920, 540]), ..Peer::new("flash", Side::Left) });
        let local = [mon(0, 0, 3840, 2160), mon(3840, 0, 1920, 1200)];
        assert!(c.update_peer_monitors(0, vec![mon(0, 0, 3840, 2160)], &local));
        assert_eq!(c.peers[0].position, Some([-3840, 0]), "right edge still at x = 0, centre kept: level with the 4K");
        assert!(!c.update_peer_monitors(0, vec![mon(0, 0, 3840, 2160)], &local), "unchanged");
        // Dragged a bit low while unknown (the user's case): nearly level, so level.
        let mut c = Config::defaults();
        c.peers.push(Peer { position: Some([-1920, 776]), ..Peer::new("flash", Side::Left) });
        assert!(c.update_peer_monitors(0, vec![mon(0, 0, 3840, 2160)], &local));
        assert_eq!(c.peers[0].position, Some([-3840, 0]));
    }

    #[test]
    fn overlapping_peers_move_out_along_their_side() {
        let local = [mon(0, 0, 3840, 2160), mon(3840, 0, 1920, 1200)];
        let mut c = Config::defaults();
        // The broken case: a 4K peer left of us, placed for a 1080p one.
        c.peers.push(Peer { position: Some([-1920, 540]), monitors: vec![mon(0, 0, 3840, 2160)], ..Peer::new("flash", Side::Left) });
        c.peers.push(Peer { position: Some([4000, 1000]), monitors: vec![mon(0, 0, 1920, 1080)], ..Peer::new("below", Side::Bottom) });
        // Attached but off-centre (the user's saved layout): centred.
        c.peers.push(Peer { position: Some([-3840, 776]), monitors: vec![mon(0, 0, 3840, 2160)], ..Peer::new("left", Side::Left) });
        assert!(c.fix_overlaps(&local));
        assert_eq!(c.peers[0].position, Some([-3840, 0]), "clear of the 4K, centred on it");
        assert_eq!(c.peers[1].position, Some([3840, 1200]), "below the 1920x1200, centred on it");
        assert_eq!(c.peers[2].position, Some([-3840, 0]));
        assert!(!c.fix_overlaps(&local), "nothing left to fix");
    }
    #[test]
    fn rendered_config_is_commented_and_parses_back() {
        let mut c: Config = toml::from_str("").unwrap();
        c.peers.push(Peer {
            address: Some("10.0.0.2".into()),
            fingerprint: Some("ab".into()),
            last_ip: Some("10.0.0.2:24801".into()),
            position: Some([-1920, 540]),
            monitors: vec![desklink_proto::Monitor { x: 0, y: 0, width: 1920, height: 1080, scale_percent: 100, primary: true, name: "DISPLAY1".into() }],
            ..Peer::new("a", Side::Left)
        });
        c.peers.push(Peer { unplaced: true, ..Peer::new("b", Side::Right) });
        let text = c.render().unwrap();
        assert!(text.contains("# How Ctrl+V pastes files from another machine:"), "{text}");
        assert!(text.contains("#   bottom-right | bottom-left | top-right | top-left | center"));
        assert_eq!(text.matches("# The other machine's name.").count(), 1, "peer docs once");
        // Comments are ignored when reading it back.
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.render().unwrap(), text);
        // Every documented key exists in the rendered defaults (catches typos).
        let body = &text[HEADER.len()..]; // the header has an example [[peer]]
        for (s, k, _) in KEY_DOCS {
            if (*s == "log" && *k == "path") || (*s == "peer" && *k == "unplaced") {
                continue; // only written when set
            }
            let found = if s.is_empty() {
                text.split("\n[").next().unwrap().contains(&format!("\n{k} = "))
            } else {
                let header = if *s == "peer" { "[[peer]]".to_string() } else { format!("[{s}]") };
                body.split(&header).nth(1).unwrap_or("").split("\n[").next().unwrap_or("").contains(&format!("\n{k} = "))
            };
            assert!(found, "documented key {s}.{k} not found");
        }
    }
}
