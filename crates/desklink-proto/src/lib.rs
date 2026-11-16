//! DeskLink wire protocol.
//!
//! Everything here is platform-neutral: keys are USB HID usages, mouse motion is
//! relative counts, and edge crossings are expressed as a fraction along the edge
//! so they survive differing resolutions and DPI scales between peers.

use serde::{Deserialize, Serialize};

/// Bumped on incompatible changes. Peers negotiate features via `Hello::caps`,
/// so minor additions should not require a bump.
pub const PROTOCOL_VERSION: u16 = 3;

/// USB HID usage ID on the Keyboard/Keypad page (0x07).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key(pub u16);

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    /// Relative motion in device counts (after the sender's pointer ballistics).
    MouseMove { dx: i32, dy: i32 },
    MouseButton { button: MouseButton, down: bool },
    /// Wheel delta, 120 per notch (Windows convention); dx is horizontal tilt.
    Wheel { dx: i32, dy: i32 },
    Key { key: Key, down: bool },
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub fn opposite(self) -> Edge {
        match self {
            Edge::Left => Edge::Right,
            Edge::Right => Edge::Left,
            Edge::Top => Edge::Bottom,
            Edge::Bottom => Edge::Top,
        }
    }
}

/// A monitor in the peer's virtual desktop, in physical pixels.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Monitor {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    /// Windows-style scale, e.g. 150 for 150%.
    pub scale_percent: u16,
    pub primary: bool,
    pub name: String,
}

/// How a PC works with the others. Peers connect only to peers; a master
/// only to clients, a client only to a master.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Role {
    #[default]
    Peer,
    Master,
    Client,
}

impl Role {
    pub fn connects_to(self, other: Role) -> bool {
        matches!((self, other), (Role::Peer, Role::Peer) | (Role::Master, Role::Client) | (Role::Client, Role::Master))
    }

    pub fn name(self) -> &'static str {
        match self {
            Role::Peer => "peer",
            Role::Master => "master",
            Role::Client => "client",
        }
    }

    pub fn from_name(name: &str) -> Option<Role> {
        match name {
            "peer" => Some(Role::Peer),
            "master" => Some(Role::Master),
            "client" => Some(Role::Client),
            _ => None,
        }
    }

    /// Why a PC in this role refuses one in `other` (for logs).
    pub fn refusal(self, other: Role) -> &'static str {
        match self {
            Role::Peer => "a peer connects only to other peers",
            Role::Master if other == Role::Master => "two masters can't drive each other",
            Role::Master => "a master connects only to clients",
            Role::Client => "a client connects only to a master",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct LockKeys {
    pub caps: bool,
    pub num: bool,
    pub scroll: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Message {
    /// First message on every connection, both directions.
    Hello {
        version: u16,
        caps: u32,
        name: String,
        monitors: Vec<Monitor>,
        os: Os,
        role: Role,
    },
    /// Sent every heartbeat interval by both sides; also carries liveness of the
    /// input path (the receiver answers with the same seq).
    Heartbeat { seq: u32 },
    HeartbeatAck { seq: u32 },
    /// Sent as early as possible on shutdown/logoff/sleep/exit so the peer can
    /// reclaim the cursor immediately instead of waiting for a timeout.
    Goodbye { reason: GoodbyeReason },
    /// Director -> client: the cursor enters you at `x`, `y` (your own screen
    /// coordinates; the director knows your screens from `Hello`).
    /// `session` increments on every enter so stale messages can be discarded.
    Enter { session: u32, x: i32, y: i32 },
    /// Client -> director: acknowledged, with the client's lock-key state.
    EnterAck { session: u32, locks: LockKeys },
    /// Client -> director: the cursor is pushing past my `edge` at `x`, `y`
    /// (the last point on my screens). The director decides where it goes:
    /// back to itself, to another PC, or nowhere (it stays here).
    /// The director is the sole authority on who owns the cursor.
    /// `to`: where the cursor lands on the director (its coordinates), when
    /// the client's own layout decides (peer mode: the layout of the PC the
    /// cursor is on rules).
    LeaveRequest { session: u32, edge: Edge, x: i32, y: i32, to: Option<(i32, i32)> },
    /// Director -> client: you no longer own the cursor. Client releases all
    /// keys/buttons it injected.
    Leave { session: u32 },
    Input { session: u32, event: InputEvent },
    /// Client -> director: stop directing me now; I am not (or no longer) in
    /// `session`. `local_use`: someone is using my own mouse/keyboard (so
    /// the director's user went away); otherwise our states disagreed (a
    /// message was lost) and the director's user is still at the director.
    Takeover { session: u32, local_use: bool },
    /// The sender's clipboard, sent when the user moves to the receiver.
    /// Travels on a separate low-priority stream (see `desklink-net`).
    Clipboard { items: Vec<ClipItem>, files: Option<FileOffer> },
    /// Director -> client: run a user command (e.g. a hotkey pressed on the
    /// director's keyboard while it controls the client) on the client.
    Command(Command),
    /// Director -> client, right after `Enter`: the user is dragging these
    /// files across with the left button held; continue the drag there.
    DragStart { session: u32, files: FileOffer },
    /// Master -> client: the whole layout, for showing (clients don't route;
    /// kept in memory only). Sent on connect and whenever it changes.
    Layout { pcs: Vec<LayoutPc> },
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClipFormat {
    /// UTF-8 text.
    Text,
    /// Windows "HTML Format" (UTF-8 with offset header).
    Html,
    /// Rich Text Format.
    Rtf,
    /// Device-independent bitmap (BITMAPINFO + pixels), as Windows CF_DIB.
    Dib,
    /// PNG file bytes.
    Png,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ClipItem {
    pub format: ClipFormat,
    pub data: Vec<u8>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoodbyeReason {
    Exit,
    Shutdown,
    Logoff,
    Sleep,
    Disabled,
}

pub fn encode(msg: &Message) -> Result<Vec<u8>, postcard::Error> {
    postcard::to_stdvec(msg)
}

pub fn decode(bytes: &[u8]) -> Result<Message, postcard::Error> {
    postcard::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let msgs = [
            Message::Input {
                session: 7,
                event: InputEvent::MouseMove { dx: -3, dy: 12 },
            },
            Message::Input {
                session: 7,
                event: InputEvent::Key { key: Key(0x04), down: true },
            },
            Message::Enter { session: 1, x: -1919, y: 540 },
            Message::Goodbye { reason: GoodbyeReason::Shutdown },
        ];
        for m in msgs {
            let bytes = encode(&m).unwrap();
            assert_eq!(decode(&bytes).unwrap(), m);
        }
    }

    #[test]
    fn mouse_move_is_tiny() {
        let bytes = encode(&Message::Input {
            session: 1,
            event: InputEvent::MouseMove { dx: 5, dy: -5 },
        })
        .unwrap();
        assert!(bytes.len() <= 6, "{} bytes", bytes.len());
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Windows,
    MacOs,
    Linux,
    Other,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else if cfg!(target_os = "linux") {
            Os::Linux
        } else {
            Os::Other
        }
    }
}


/// One PC in a master's layout: its screens, and where they sit on the
/// master's desk (`x`, `y` added to its own screen coordinates).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct LayoutPc {
    pub name: String,
    pub master: bool,
    pub x: i32,
    pub y: i32,
    /// Empty while its real screens are unknown.
    pub monitors: Vec<Monitor>,
}
/// Files copied on the sender. Contents are fetched on demand, by index,
/// with `FileRequest`; the receiver can only ask for entries in the offer.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct FileOffer {
    /// Changes with every copy; requests for an old offer are refused.
    pub id: u64,
    pub entries: Vec<FileEntry>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    /// Relative path with `/` separators; top-level items have no `/`.
    /// Parents always precede their children.
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// Last modification, milliseconds since the Unix epoch.
    pub modified_ms: u64,
}

impl FileOffer {
    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().filter(|e| !e.is_dir).map(|e| e.size).sum()
    }

    pub fn file_count(&self) -> usize {
        self.entries.iter().filter(|e| !e.is_dir).count()
    }

    /// Top-level names, for showing the user what was copied.
    pub fn top_level(&self) -> impl Iterator<Item = &FileEntry> {
        self.entries.iter().filter(|e| !e.path.contains('/'))
    }
}

/// First frame on a file stream (receiver -> sender).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct FileRequest {
    pub offer: u64,
    pub index: u32,
    pub offset: u64,
}

/// Sender's answer; on `Ok` the raw file bytes follow until the stream ends.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum FileResponse {
    Ok { remaining: u64 },
    Err(String),
}

/// User commands that act on the machine the user is currently on.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Paste files copied on another machine into the folder in front of
    /// the user (asks first).
    PasteFiles,
}
