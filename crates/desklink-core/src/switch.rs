//! The switching state machine.
//!
//! Every machine runs the same logic and can play both roles at once:
//! * director: its own keyboard/mouse drive a peer (`Focus::Remote`);
//! * client: a peer's input is injected locally (`controller`).
//!
//! Rules that keep the cursor from ever getting stuck:
//! * The director is the only authority on where its input goes. The client
//!   merely *requests* to give the cursor back.
//! * Nothing blocks on the peer. Switching is optimistic; a missing ack,
//!   a goodbye or a dead link returns the cursor to the director at once.
//! * The client releases every injected key/button whenever it loses its
//!   controller, for whatever reason.

use crate::geometry::{Park, Screens};
use desklink_proto::{Command, Edge, FileOffer, InputEvent, LockKeys, Message, Monitor};
use std::time::{Duration, Instant};

pub type PeerId = usize;

/// How long a director waits for `EnterAck` before taking the cursor back.
pub const ENTER_ACK_TIMEOUT: Duration = Duration::from_millis(1000);
/// How often a client repeats an unanswered `LeaveRequest`.
const LEAVE_RETRY: Duration = Duration::from_millis(250);

/// Platform and network operations the state machine drives.
pub trait Io {
    fn send(&mut self, peer: PeerId, msg: Message);
    /// Swallow local input and park the cursor at `anchor`.
    /// `carry_buttons`: held mouse buttons stay pressed on the peer (a drag).
    fn grab(&mut self, anchor: (i32, i32), carry_buttons: bool);
    fn ungrab(&mut self);
    /// Like `ungrab`, for a return from the peer: mouse buttons still held
    /// are released locally (a drag may continue here), not swallowed.
    fn ungrab_carrying(&mut self);
    /// Place the local cursor (no input event is generated).
    fn set_cursor(&mut self, p: (i32, i32));
    /// Current cursor position as the injector sees it.
    fn cursor_pos(&mut self) -> (i32, i32);
    /// Inject a move to an absolute position (client side).
    fn inject_move(&mut self, p: (i32, i32));
    /// Inject a key/button/wheel event (client side).
    fn inject(&mut self, ev: InputEvent);
    /// Release every key and button this machine injected.
    fn release_injected(&mut self);
    fn lock_keys(&mut self) -> LockKeys;
    fn hide_cursor(&mut self, why: HideReason);
    fn show_cursor(&mut self);
    /// Report the next physical local input as `Local::Activity`.
    fn watch_local_activity(&mut self, purpose: Watch);
    /// The user is moving from this machine to `peer`: send our clipboard
    /// (if it changed since we last exchanged with that peer).
    fn share_clipboard(&mut self, peer: PeerId);
    /// Run a user command on this machine.
    fn run_command(&mut self, cmd: Command);
    /// Is the left mouse button held right now (a drag may be in progress)?
    fn left_button_held(&mut self) -> bool;
    /// Find out in the background whether files are being dragged under the
    /// cursor; reported as `Local::DragCaught`. Either way the platform ends
    /// the local press safely (over its own drop zone, where releasing a drag
    /// does nothing). Found files are made fetchable for peers.
    fn start_drag_catch(&mut self);
    /// Move the parked cursor while grabbed.
    fn park(&mut self, anchor: (i32, i32));
    /// Continue `peer`'s drag of `files` here (it presses the button itself).
    fn start_drag(&mut self, peer: PeerId, files: FileOffer);
    /// Cancel a drag started by `start_drag`, if any.
    fn cancel_drag(&mut self);
    /// Does the program in front hold the mouse, as a game does while playing?
    fn game_holds_mouse(&mut self) -> bool;
}

/// Why local input is being watched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Watch {
    /// Bring back a hidden cursor (honours the reveal settings).
    Reveal,
    /// We are being controlled: any local mouse/keyboard use takes us back.
    Takeover,
}

/// Why the state machine wants this machine's cursor hidden; settings can
/// enable each case separately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HideReason {
    /// Our input now drives a peer; our cursor is parked.
    Directing,
    /// A peer stopped controlling us; our cursor sits where it left.
    Released,
}

/// Local input, already classified by the platform layer.
#[derive(Clone, Debug, PartialEq)]
pub enum Local {
    CursorMoved((i32, i32)),
    Input(InputEvent),
    /// The platform released the grab itself (return hotkey, safety valve).
    GrabReleased,
    /// Someone used this machine's own mouse/keyboard (after
    /// `Io::watch_local_activity`).
    Activity,
    /// A command hotkey: runs where the user is (forwarded while directing).
    Command(Command),
    /// Result of `Io::start_drag_catch`: the files being dragged, if any.
    DragCaught(Option<FileOffer>),
}

#[derive(Clone, Debug)]
pub struct PeerSlot {
    pub name: String,
    /// Which side of *this* machine the peer sits on (used while it has no
    /// `offset`: any point of that side leads to it, scaled along the side).
    pub edge: Edge,
    /// Where the peer's screens sit on the desk: added to its own screen
    /// coordinates gives ours. The cursor crosses exactly where screens touch.
    pub offset: Option<(i32, i32)>,
    /// Known while connected.
    pub screens: Option<Screens>,
    /// Temporarily excluded from edge switching.
    pub skip: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Focus {
    Local,
    Remote {
        peer: PeerId,
        session: u32,
        since: Instant,
        acked: bool,
        /// Where our cursor left (just inside our screens), to put it back if
        /// the peer vanishes or is taken over.
        back: (i32, i32),
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Controller {
    peer: PeerId,
    session: u32,
    leave_sent: Option<Instant>,
}

/// Where the cursor goes when it leaves a PC's screens.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Target {
    /// This PC, at that point.
    Local((i32, i32)),
    /// A peer, at that point of its own screens.
    Peer(PeerId, (i32, i32)),
}

/// One pixel inwards from a border point on `edge`, so the very next motion
/// back towards the edge (and only that) leaves again.
fn inward(p: (i32, i32), edge: Edge) -> (i32, i32) {
    match edge {
        Edge::Left => (p.0 + 1, p.1),
        Edge::Right => (p.0 - 1, p.1),
        Edge::Top => (p.0, p.1 + 1),
        Edge::Bottom => (p.0, p.1 - 1),
    }
}

pub struct Core {
    screens: Screens,
    peers: Vec<PeerSlot>,
    focus: Focus,
    controller: Option<Controller>,
    next_session: u32,
    /// `None`: leave the cursor where it crossed the edge.
    park: Option<Park>,
    /// Last (peer, session) told "you are not controlling me".
    nacked: Option<(PeerId, u32)>,
    /// Peer that last controlled us and left (our cursor is hidden for its
    /// sake until local use).
    released_by: Option<PeerId>,
    /// Crossing that started a background drag check.
    pending_catch: Option<Catch>,
    /// (peer, session) we last came back from: a drag the user carried back
    /// is announced with that session.
    returned_from: Option<(PeerId, u32)>,
    /// Peer mode: when a controlling peer's cursor leaves this PC, this PC's
    /// own layout says where it goes (the layout of the PC the cursor is on
    /// rules). Off on a client: its master's layout decides.
    own_layout_rules: bool,
    /// Crossings this close (px) to a corner of the screens don't switch
    /// (aiming at a close button or the Start button). `None`: they do.
    corner_guard: Option<i32>,
    /// While a game holds the mouse, the cursor stays here.
    fullscreen_guard: bool,
    /// False on a client: its own keyboard and mouse stay on it.
    can_direct: bool,
}

/// A background drag check (`Io::start_drag_catch`) in progress.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Catch {
    /// We are directing a peer in this session (our own drag going out).
    Out(u32),
    /// We are leaving `peer`'s control (its user drags our files back).
    Back(PeerId, u32),
}

impl Core {
    pub fn new(monitors: &[Monitor], peers: Vec<PeerSlot>) -> Self {
        Core {
            screens: Screens::new(monitors),
            peers,
            focus: Focus::Local,
            controller: None,
            next_session: 1,
            park: Some(Park::default()),
            nacked: None,
            released_by: None,
            pending_catch: None,
            returned_from: None,
            can_direct: true,
            own_layout_rules: true,
            corner_guard: None,
            fullscreen_guard: false,
        }
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn is_controlled(&self) -> bool {
        self.controller.is_some()
    }

    pub fn peers(&self) -> &[PeerSlot] {
        &self.peers
    }

    pub fn set_skip(&mut self, peer: PeerId, skip: bool) {
        if let Some(p) = self.peers.get_mut(peer) {
            p.skip = skip;
        }
    }

    /// May this PC's own keyboard and mouse move to other PCs? (Not on a
    /// client: only its master drives it.)
    pub fn set_can_direct(&mut self, can: bool) {
        self.can_direct = can;
    }

    /// Does this PC's own layout decide where a controlling peer's cursor goes
    /// when it leaves here? (Peer mode yes; a client follows its master.)
    pub fn set_corner_guard(&mut self, px: Option<i32>) {
        self.corner_guard = px;
    }

    /// Keep the cursor here while a game holds the mouse.
    pub fn set_fullscreen_guard(&mut self, on: bool) {
        self.fullscreen_guard = on;
    }

    pub fn set_own_layout_rules(&mut self, rules: bool) {
        self.own_layout_rules = rules;
    }

    /// Where a peer's screens sit on the desk (`None`: placed by side only).
    pub fn set_offset(&mut self, peer: PeerId, offset: Option<(i32, i32)>) {
        if let Some(p) = self.peers.get_mut(peer) {
            p.offset = offset;
        }
    }

    /// Which side of this PC a peer is on (used while it has no offset).
    pub fn set_edge(&mut self, peer: PeerId, edge: Edge) {
        if let Some(p) = self.peers.get_mut(peer) {
            p.edge = edge;
        }
    }

    pub fn set_park(&mut self, park: Option<Park>) {
        self.park = park;
    }

    pub fn set_monitors(&mut self, monitors: &[Monitor]) {
        self.screens = Screens::new(monitors);
    }

    // -- events -------------------------------------------------------------

    pub fn on_local(&mut self, io: &mut impl Io, ev: Local, now: Instant) {
        match (ev, self.focus) {
            (Local::CursorMoved(p), Focus::Local) => self.maybe_switch_out(io, p, now),
            (Local::Input(ev), Focus::Remote { peer, session, .. }) => {
                io.send(peer, Message::Input { session, event: ev });
            }
            (Local::GrabReleased, Focus::Remote { peer, session, .. }) => {
                // Return hotkey or safety valve: platform already ungrabbed.
                io.send(peer, Message::Leave { session });
                io.set_cursor(self.screens.primary_center());
                io.show_cursor();
                self.focus = Focus::Local;
                log::info!("returned to local (hotkey)");
            }
            (Local::Command(cmd), Focus::Remote { peer, .. }) => io.send(peer, Message::Command(cmd)),
            (Local::DragCaught(files), _) => self.drag_caught(io, files),
            (Local::Command(cmd), Focus::Local) => io.run_command(cmd),
            (Local::Activity, Focus::Local) => match self.controller.take() {
                // Someone is physically using this machine while a peer
                // controls it: the person at the machine wins.
                Some(c) => {
                    io.cancel_drag();
                    io.release_injected();
                    io.send(c.peer, Message::Takeover { session: c.session, local_use: true });
                    log::info!("local input: took this machine back from {}", self.peers[c.peer].name);
                }
                None => {
                    // Someone uses this machine directly now.
                    self.released_by = None;
                    io.show_cursor();
                }
            },
            _ => {}
        }
    }

    pub fn on_connected(&mut self, peer: PeerId, monitors: &[Monitor]) {
        if let Some(p) = self.peers.get_mut(peer) {
            p.screens = Some(Screens::new(monitors));
            log::info!("peer {} connected ({} monitors)", p.name, monitors.len());
        }
    }

    pub fn on_disconnected(&mut self, io: &mut impl Io, peer: PeerId) {
        if let Some(p) = self.peers.get_mut(peer) {
            p.screens = None;
            log::info!("peer {} disconnected", p.name);
        }
        if let Focus::Remote { peer: fp, .. } = self.focus
            && fp == peer
        {
            self.return_to_center(io);
        }
        let was_controlled = self.controller.is_some_and(|c| c.peer == peer);
        if was_controlled {
            self.controller = None;
            io.cancel_drag();
            io.release_injected();
        }
        // Our cursor sits at the edge the remote user came in or left
        // through (and is hidden if they left). That peer is gone: put the
        // cursor in the middle, where it's easy to find. An injected move,
        // not SetCursorPos: Windows redraws the cursor only on input.
        if was_controlled || self.released_by == Some(peer) {
            self.released_by = None;
            log::info!("{} is gone: cursor to the middle of the screen", self.peers[peer].name);
            io.inject_move(self.screens.primary_center());
        }
        // Nobody can be using this machine remotely through that peer any
        // more: a cursor hidden for its sake must come back now, not only
        // when someone touches the mouse.
        if self.controller.is_none() && self.focus == Focus::Local {
            io.show_cursor();
        }
    }

    pub fn on_message(&mut self, io: &mut impl Io, peer: PeerId, msg: Message, now: Instant) {
        match msg {
            // ---- we are the client ----
            Message::Enter { session, x, y } => {
                // One human, one place: if we were directing someone, that
                // ended (we must have missed the message saying so).
                if let Focus::Remote { peer: fp, back, .. } = self.focus {
                    log::warn!(
                        "entered by {} while directing {}: releasing our grab",
                        self.peers[peer].name,
                        self.peers[fp].name
                    );
                    self.return_local(io, back);
                }
                if let Some(old) = self.controller.take() {
                    log::warn!("new controller replaces peer {}", old.peer);
                    io.cancel_drag();
                    io.release_injected();
                }
                self.released_by = None;
                self.controller = Some(Controller { peer, session, leave_sent: None });
                io.inject_move(self.screens.clamp((x, y)));
                io.show_cursor();
                io.watch_local_activity(Watch::Takeover);
                let locks = io.lock_keys();
                io.send(peer, Message::EnterAck { session, locks });
            }
            Message::Input { session, event } => {
                let Some(c) = self.controller.filter(|c| c.peer == peer && c.session == session)
                else {
                    // The peer believes it controls us, but we're not in that
                    // session (a Leave/Takeover was lost, or we restarted).
                    // Tell it once per session; it releases its grab. This
                    // heals any disagreement at the first input event.
                    if self.nacked != Some((peer, session)) {
                        self.nacked = Some((peer, session));
                        log::warn!("{} sent input for session {session} we're not in; telling it to stop", self.peers[peer].name);
                        io.send(peer, Message::Takeover { session, local_use: false });
                    }
                    return;
                };
                match event {
                    InputEvent::MouseMove { dx, dy } => self.client_move(io, c, dx, dy, now),
                    other => io.inject(other),
                }
            }
            Message::Leave { session } => {
                if self.controller.is_some_and(|c| c.peer == peer && c.session == session) {
                    // The user went back to `peer`: our clipboard goes with them.
                    io.share_clipboard(peer);
                    self.drop_controller(io);
                }
            }

            // ---- we are the director ----
            Message::EnterAck { session: s, .. } => {
                if let Focus::Remote { peer: fp, session, ref mut acked, since, .. } = self.focus
                    && fp == peer
                    && s == session
                {
                    *acked = true;
                    log::info!(
                        "{} took over in {:.1} ms",
                        self.peers[peer].name,
                        now.duration_since(since).as_secs_f64() * 1000.0
                    );
                }
            }
            Message::LeaveRequest { session: s, edge, x, y, to } => {
                let Focus::Remote { peer: fp, session, back, .. } = self.focus else { return };
                if fp != peer || s != session {
                    return;
                }
                // The client's own layout said where (peer mode); otherwise ours.
                let target = match to {
                    Some(at) => Some(Target::Local(self.screens.clamp(at))),
                    // Our layout decides (a client of ours): mind its corners.
                    None if self.corner_guard.is_some_and(|n| self.peers[peer].screens.as_ref().is_some_and(|s| s.near_corner(edge, (x, y), n))) => None,
                    None => self.route(Some(peer), (x, y), edge),
                };
                match target {
                    Some(Target::Local(at)) => {
                        // A held button may be a drag of the peer's files
                        // coming back (`DragStart` follows): its release
                        // belongs here.
                        io.ungrab_carrying();
                        io.set_cursor(at);
                        io.show_cursor();
                        io.send(peer, Message::Leave { session });
                        self.focus = Focus::Local;
                        self.returned_from = Some((peer, session));
                    }
                    Some(Target::Peer(next, at)) if next != peer => {
                        // Straight on to another PC; our input stays grabbed.
                        io.send(peer, Message::Leave { session });
                        let session = self.enter(io, next, at);
                        self.focus = Focus::Remote { peer: next, session, since: now, acked: false, back };
                        log::info!("moved on from {} to {}", self.peers[peer].name, self.peers[next].name);
                    }
                    // Nothing there: the cursor stays where it is.
                    _ => {}
                }
            }
            Message::Takeover { session: s, local_use } => {
                if let Focus::Remote { peer: fp, session, back, .. } = self.focus
                    && fp == peer
                    && s == session
                {
                    io.ungrab();
                    io.set_cursor(back);
                    io.show_cursor();
                    self.focus = Focus::Local;
                    if local_use {
                        io.share_clipboard(peer);
                        // The user is at the peer now: our cursor is not in use.
                        io.hide_cursor(HideReason::Released);
                        io.watch_local_activity(Watch::Reveal);
                        log::info!("{} was taken over locally", self.peers[peer].name);
                    } else {
                        // States disagreed; our user is right here.
                        log::warn!("{} gave the keyboard and mouse back (not controlled, or it can't take input now)", self.peers[peer].name);
                    }
                }
            }
            Message::Goodbye { .. } => self.on_disconnected(io, peer),
            Message::Hello { .. } | Message::Heartbeat { .. } | Message::HeartbeatAck { .. } => {}
            // Applied by the platform layer directly.
            Message::Clipboard { .. } | Message::Layout { .. } => {}
            Message::DragStart { session, files } => {
                // The user drags files in from `peer`: continue the drag here
                // (it presses the button itself; the release comes from the peer).
                if self.controller.is_some_and(|c| c.peer == peer && c.session == session) {
                    io.start_drag(peer, files);
                } else if self.focus == Focus::Local && self.returned_from == Some((peer, session)) {
                    // We just came back from `peer` dragging its files: if
                    // the button is still down, continue the drag here.
                    if io.left_button_held() {
                        io.start_drag(peer, files);
                    } else {
                        log::info!("{} dragged files back, but the button is already released", self.peers[peer].name);
                    }
                } else {
                    log::info!("ignored a drag from {} (session {session} is over)", self.peers[peer].name);
                }
            }
            Message::Command(cmd) => {
                // Only the machine currently controlling us may run commands here.
                if self.controller.is_some_and(|c| c.peer == peer) {
                    io.run_command(cmd);
                }
            }
        }
    }

    /// This machine can't take the controlling peer's input (a screen that
    /// refuses it): give its keyboard and mouse back, so it isn't stuck.
    pub fn hand_back(&mut self, io: &mut impl Io, why: &str) {
        if let Some(c) = self.controller.take() {
            io.cancel_drag();
            io.release_injected();
            io.send(c.peer, Message::Takeover { session: c.session, local_use: false });
            log::warn!("{why}: gave the keyboard and mouse back to {}", self.peers[c.peer].name);
        }
    }

    pub fn on_tick(&mut self, io: &mut impl Io, now: Instant) {
        if let Focus::Remote { peer, acked: false, since, .. } = self.focus
            && now.duration_since(since) > ENTER_ACK_TIMEOUT
        {
            log::warn!("peer {peer} did not acknowledge enter; taking cursor back");
            self.return_to_center(io);
        }
    }

    /// Stop controlling and being controlled (exit, sleep, session lock).
    pub fn shutdown(&mut self, io: &mut impl Io) {
        if let Focus::Remote { peer, session, back, .. } = self.focus {
            io.send(peer, Message::Leave { session });
            self.return_local(io, back);
        }
        if self.controller.take().is_some() {
            io.release_injected();
        }
        io.show_cursor();
    }

    // -- internals ----------------------------------------------------------

    fn maybe_switch_out(&mut self, io: &mut impl Io, p: (i32, i32), now: Instant) {
        // While a peer controls us (or on a client), our own mouse stays local.
        if self.controller.is_some() || !self.can_direct {
            return;
        }
        // The hook reports where the cursor *would* go, before Windows clips
        // it to the screen: a fast push past the edge arrives off-screen.
        let hit = if self.screens.monitor_at(p).is_some() {
            self.screens.touched(p)
        } else {
            let from = self.screens.clamp(io.cursor_pos());
            self.screens.exit(from, p)
        };
        let Some((edge, at)) = hit else {
            return;
        };
        // Too close to a corner of our screens: no switch.
        if self.corner_guard.is_some_and(|n| self.screens.near_corner(edge, at, n)) {
            return;
        }
        let Some(Target::Peer(peer, entry)) = self.route(None, at, edge) else {
            return;
        };
        // A game holding the mouse (playing, not in its menus): stay in it.
        if self.fullscreen_guard && io.game_holds_mouse() {
            return;
        }
        let session = self.enter(io, peer, entry);
        io.hide_cursor(HideReason::Directing);
        // Crossing with the left button held: a file drag may be in progress.
        // Find out in the background (switching must not stall); the cursor
        // stays at the edge meanwhile, where the drop zone needs it, and is
        // parked once we know (see `Local::DragCaught`).
        let maybe_drag = io.left_button_held();
        let anchor = if maybe_drag {
            self.pending_catch = Some(Catch::Out(session));
            io.start_drag_catch();
            self.screens.clamp(io.cursor_pos())
        } else {
            self.park_anchor(io)
        };
        io.grab(anchor, maybe_drag);
        self.focus = Focus::Remote { peer, session, since: now, acked: false, back: inward(at, edge) };
        log::info!("switched to {} at {edge:?} ({}, {})", self.peers[peer].name, at.0, at.1);
    }

    /// Start a session on `peer` with the cursor at `at` (its coordinates).
    fn enter(&mut self, io: &mut impl Io, peer: PeerId, at: (i32, i32)) -> u32 {
        let session = self.next_session;
        self.next_session = self.next_session.wrapping_add(1).max(1);
        // The user moves to `peer`: our clipboard goes with them.
        io.share_clipboard(peer);
        io.send(peer, Message::Enter { session, x: at.0, y: at.1 });
        session
    }

    /// Where the cursor goes from `from` (`None`: this PC) when it pushes past
    /// `edge` at border point `at` (that PC's coordinates). Placed PCs meet
    /// exactly where their screens touch on the desk; a PC placed only by
    /// side takes any point of that side, scaled along it.
    fn route(&self, from: Option<PeerId>, at: (i32, i32), edge: Edge) -> Option<Target> {
        let placed: Vec<Option<(&Screens, (i32, i32))>> = std::iter::once(Some((&self.screens, (0, 0))))
            .chain(self.peers.iter().map(|p| match (&p.screens, p.offset) {
                (Some(s), Some(o)) if !p.skip => Some((s, o)),
                _ => None,
            }))
            .collect();
        let from_pc = from.map_or(0, |p| p + 1);
        if placed.get(from_pc).is_some_and(Option::is_some)
            && let Some((pc, entry)) = crate::geometry::across(&placed, from_pc, at, edge)
        {
            return Some(if pc == 0 { Target::Local(entry) } else { Target::Peer(pc - 1, entry) });
        }
        match from {
            None => {
                let peer = self
                    .peers
                    .iter()
                    .position(|s| s.offset.is_none() && s.edge == edge && s.screens.is_some() && !s.skip)?;
                let along = self.screens.fraction_at(edge, at);
                Some(Target::Peer(peer, self.peers[peer].screens.as_ref()?.entry_point(edge.opposite(), along)))
            }
            Some(peer) => {
                // Back from a peer placed by side, through the side facing us.
                let slot = self.peers.get(peer)?;
                if slot.offset.is_some() || edge != slot.edge.opposite() {
                    return None;
                }
                let along = slot.screens.as_ref()?.fraction_at(edge, at);
                Some(Target::Local(self.screens.entry_point(slot.edge, along)))
            }
        }
    }

    /// Where to park our cursor while directing (default: a corner). The hook
    /// reports unclipped targets, so motion is measured fine anywhere, and a
    /// corner keeps apps that draw their own cursor from showing it.
    fn park_anchor(&self, io: &mut impl Io) -> (i32, i32) {
        match self.park {
            Some(park) => self.screens.park_point(park),
            None => self.screens.clamp(io.cursor_pos()),
        }
    }

    /// The background drag check finished. Without files the button was held
    /// for something else (selecting text, ...); either way the platform
    /// already ended that press locally.
    fn drag_caught(&mut self, io: &mut impl Io, files: Option<FileOffer>) {
        match self.pending_catch.take() {
            Some(Catch::Out(s)) => {
                let Focus::Remote { peer, session, .. } = self.focus else {
                    return;
                };
                if s != session {
                    return; // stale result from an earlier crossing
                }
                if let Some(files) = files {
                    log::info!("dragging {} to {}", crate::files::summary(&files), self.peers[peer].name);
                    io.send(peer, Message::DragStart { session, files });
                }
                let anchor = self.park_anchor(io);
                io.park(anchor);
            }
            Some(Catch::Back(peer, session)) => {
                match files {
                    Some(files) => {
                        log::info!("dragging {} back to {}", crate::files::summary(&files), self.peers[peer].name);
                        io.send(peer, Message::DragStart { session, files });
                    }
                    None => log::info!("no files were being dragged"),
                }
                // The release held back when control ended (see
                // `drop_controller`) is safe now.
                if self.controller.is_none() {
                    io.release_injected();
                }
            }
            None => {}
        }
    }


    /// The machine we were directing is gone: take the cursor back to the
    /// middle of our primary monitor, where it's easy to find (the edge we
    /// left through means nothing any more).
    fn return_to_center(&mut self, io: &mut impl Io) {
        io.ungrab();
        io.set_cursor(self.screens.primary_center());
        io.show_cursor();
        self.focus = Focus::Local;
    }
    fn return_local(&mut self, io: &mut impl Io, back: (i32, i32)) {
        io.ungrab();
        io.set_cursor(self.screens.clamp(back));
        io.show_cursor();
        self.focus = Focus::Local;
    }

    /// Client side: our controller is gone. Release everything it pressed and
    /// hide the cursor until someone uses this machine directly.
    fn drop_controller(&mut self, io: &mut impl Io) {
        let gone = self.controller.take();
        self.released_by = gone.map(|c| c.peer);
        io.cancel_drag();
        // The user left with the button held: maybe dragging files out of
        // here. Check in the background (the cursor is still at the edge,
        // where the check needs it). Releasing the button here could drop
        // the files somewhere local; the check ends the press safely and the
        // rest is released when it reports (`drag_caught`).
        if let Some(c) = gone
            && self.pending_catch.is_none()
            && io.left_button_held()
        {
            log::info!("left with the button held: checking for dragged files");
            self.pending_catch = Some(Catch::Back(c.peer, c.session));
            io.start_drag_catch();
        } else {
            io.release_injected();
        }
        io.hide_cursor(HideReason::Released);
        io.watch_local_activity(Watch::Reveal);
    }

    fn client_move(&mut self, io: &mut impl Io, c: Controller, dx: i32, dy: i32, now: Instant) {
        let from = io.cursor_pos();
        let to = (from.0 + dx, from.1 + dy);
        io.inject_move(self.screens.clamp(to));
        // Pushing past any outer edge: the controller decides whether that
        // leads anywhere (back to it, or to another PC). Repeated while the
        // user keeps pushing, in case a request is lost.
        if let Some((edge, at)) = self.screens.exit(from, to)
            && c.leave_sent.is_none_or(|t| now.duration_since(t) >= LEAVE_RETRY)
        {
            // Too close to a corner of our screens: no switch.
            if self.corner_guard.is_some_and(|n| self.screens.near_corner(edge, at, n)) {
                return;
            }
            if self.fullscreen_guard && io.game_holds_mouse() {
                return;
            }
            // Peer mode: our own layout decides. Does this edge lead back to
            // the controller, and where on it? If not, it leads nowhere.
            let to = if self.own_layout_rules {
                match self.route(None, at, edge) {
                    Some(Target::Peer(p, entry)) if p == c.peer => Some(entry),
                    _ => return,
                }
            } else {
                None
            };
            io.send(c.peer, Message::LeaveRequest { session: c.session, edge, x: at.0, y: at.1, to });
            if let Some(c) = self.controller.as_mut() {
                c.leave_sent = Some(now);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desklink_proto::{Key, MouseButton};

    #[derive(Default)]
    struct FakeIo {
        fullscreen: bool,
        sent: Vec<(PeerId, Message)>,
        grabbed: bool,
        cursor: (i32, i32),
        injected: Vec<InputEvent>,
        releases: u32,
        hidden: bool,
        watching: bool,
        last_hide: Option<HideReason>,
        watch: Option<Watch>,
        shared_to: Vec<PeerId>,
        commands: Vec<Command>,
        carried: bool,
        carried_back: bool,
        left_held: bool,

        catches: u32,
        drags: Vec<FileOffer>,
        drag_cancels: u32,
        parked: bool,
    }

    impl Io for FakeIo {
        fn send(&mut self, peer: PeerId, msg: Message) {
            self.sent.push((peer, msg));
        }
        fn grab(&mut self, anchor: (i32, i32), carry_buttons: bool) {
            self.carried = carry_buttons;
            self.grabbed = true;
            self.cursor = anchor;
        }
        fn ungrab(&mut self) {
            self.grabbed = false;
        }
        fn ungrab_carrying(&mut self) {
            self.grabbed = false;
            self.carried_back = true;
        }
        fn set_cursor(&mut self, p: (i32, i32)) {
            self.cursor = p;
        }
        fn cursor_pos(&mut self) -> (i32, i32) {
            self.cursor
        }
        fn inject_move(&mut self, p: (i32, i32)) {
            self.cursor = p;
        }
        fn inject(&mut self, ev: InputEvent) {
            self.injected.push(ev);
        }
        fn release_injected(&mut self) {
            self.releases += 1;
        }
        fn lock_keys(&mut self) -> LockKeys {
            LockKeys::default()
        }
        fn hide_cursor(&mut self, why: HideReason) {
            self.last_hide = Some(why);
            self.hidden = true;
        }
        fn show_cursor(&mut self) {
            self.hidden = false;
            self.watching = false;
        }
        fn left_button_held(&mut self) -> bool {
            self.left_held
        }
        fn start_drag_catch(&mut self) {
            self.catches += 1;
        }
        fn park(&mut self, anchor: (i32, i32)) {
            self.cursor = anchor;
            self.parked = true;
        }
        fn start_drag(&mut self, _peer: PeerId, files: FileOffer) {
            self.drags.push(files);
        }
        fn cancel_drag(&mut self) {
            self.drag_cancels += 1;
        }
        fn game_holds_mouse(&mut self) -> bool {
            self.fullscreen
        }
        fn run_command(&mut self, cmd: Command) {
            self.commands.push(cmd);
        }
        fn share_clipboard(&mut self, peer: PeerId) {
            self.shared_to.push(peer);
        }
        fn watch_local_activity(&mut self, purpose: Watch) {
            self.watch = Some(purpose);
            self.watching = true;
        }
    }

    #[test]
    fn director_hides_parked_cursor_and_shows_on_return() {
        let t = Instant::now();
        let (mut d, mut io) = (director(), FakeIo::default());
        d.on_local(&mut io, Local::CursorMoved((0, 1080)), t);
        assert!(io.hidden);
        assert_eq!(io.cursor, (3839, 2159), "parked bottom-right of primary");
        assert!(!io.watching, "director's own input is forwarded, must not unhide");
        assert_eq!(io.last_hide, Some(HideReason::Directing));
        let s = session_of(&io);
        d.on_message(&mut io, 0, Message::LeaveRequest { session: s, edge: Edge::Right, x: 3839, y: 1080, to: None }, t);
        assert!(!io.hidden);
        assert_eq!(io.cursor.0, 1);
    }

    #[test]
    fn director_shows_cursor_on_every_way_back() {
        let t = Instant::now();
        for way in 0..4 {
            let (mut d, mut io) = (director(), FakeIo::default());
            d.on_local(&mut io, Local::CursorMoved((0, 1080)), t);
            assert!(io.hidden);
            match way {
                0 => d.on_local(&mut io, Local::GrabReleased, t),
                1 => d.on_disconnected(&mut io, 0),
                2 => d.on_tick(&mut io, t + ENTER_ACK_TIMEOUT * 2),
                _ => d.shutdown(&mut io),
            }
            assert!(!io.hidden, "way {way}");
        }
    }

    #[test]
    fn client_hides_when_left_and_shows_on_local_use() {
        let t = Instant::now();
        let (mut c, mut io) = (client(), FakeIo::default());
        c.on_message(&mut io, 0, Message::Enter { session: 1, x: 3838, y: 1080 }, t);
        assert!(!io.hidden);
        c.on_message(&mut io, 0, Message::Leave { session: 1 }, t);
        assert!(io.hidden && io.watching);
        assert_eq!(io.last_hide, Some(HideReason::Released));
        c.on_local(&mut io, Local::Activity, t);
        assert!(!io.hidden);
    }

    #[test]
    fn client_shows_cursor_when_director_vanishes() {
        let t = Instant::now();
        let (mut c, mut io) = (client(), FakeIo::default());
        c.on_message(&mut io, 0, Message::Enter { session: 1, x: 3838, y: 1080 }, t);
        c.on_disconnected(&mut io, 0);
        assert!(!io.hidden, "nobody controls us any more: cursor visible");
        assert_eq!(io.releases, 1);
        assert_eq!(io.cursor, (1920, 1080), "centered on the client too");
    }

    /// The reported case: go to the client and back (client hides its
    /// cursor), then the director disconnects: the client shows it again.
    #[test]
    fn released_cursor_comes_back_centered_when_director_disconnects() {
        let t = Instant::now();
        // The user's exact sequence: to the client, back, then the director
        // closes DeskLink (goodbye) or drops off (disconnect).
        for goodbye in [false, true] {
            let (mut c, mut io) = (client(), FakeIo { cursor: (100, 100), ..Default::default() });
            c.on_message(&mut io, 0, Message::Enter { session: 1, x: 3838, y: 1080 }, t);
            c.on_message(&mut io, 0, Message::Leave { session: 1 }, t);
            assert!(io.hidden, "hidden after the user went back");
            assert_eq!(io.cursor.0, 3838, "at the edge it came in through");
            if goodbye {
                c.on_message(&mut io, 0, Message::Goodbye { reason: desklink_proto::GoodbyeReason::Exit }, t);
            } else {
                c.on_disconnected(&mut io, 0);
            }
            assert!(!io.hidden, "visible again (goodbye={goodbye})");
            assert_eq!(io.cursor, (1920, 1080), "in the middle (goodbye={goodbye})");
        }
    }

    #[test]
    fn local_use_keeps_cursor_where_it_is_on_disconnect() {
        let t = Instant::now();
        let (mut c, mut io) = (client(), FakeIo::default());
        c.on_message(&mut io, 0, Message::Enter { session: 1, x: 3838, y: 1080 }, t);
        c.on_message(&mut io, 0, Message::Leave { session: 1 }, t);
        c.on_local(&mut io, Local::Activity, t); // someone uses its own mouse
        io.cursor = (500, 600);
        c.on_disconnected(&mut io, 0);
        assert_eq!(io.cursor, (500, 600), "not yanked away from the local user");
    }

    fn mon(x: i32, w: i32, h: i32, primary: bool) -> Monitor {
        Monitor { x, y: 0, width: w, height: h, scale_percent: 100, primary, name: String::new() }
    }

    fn mon_at(x: i32, y: i32, w: i32, h: i32, primary: bool) -> Monitor {
        Monitor { y, ..mon(x, w, h, primary) }
    }

    fn director() -> Core {
        let mut c = Core::new(
            &[mon(0, 3840, 2160, true), mon(3840, 1920, 1200, false)],
            vec![PeerSlot { name: "flashunreal".into(), edge: Edge::Left, offset: None, screens: None, skip: false }],
        );
        c.on_connected(0, &[mon(0, 3840, 2160, true)]);
        c
    }

    fn client() -> Core {
        let mut c = Core::new(
            &[mon(0, 3840, 2160, true)],
            vec![PeerSlot { name: "director".into(), edge: Edge::Right, offset: None, screens: None, skip: false }],
        );
        c.on_connected(0, &[mon(0, 3840, 2160, true), mon(3840, 1920, 1200, false)]);
        c
    }

    fn session_of(io: &FakeIo) -> u32 {
        io.sent
            .iter()
            .rev()
            .find_map(|(_, m)| match m {
                Message::Enter { session, .. } => Some(*session),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn full_round_trip_between_two_cores() {
        let t = Instant::now();
        let (mut d, mut dio) = (director(), FakeIo::default());
        let (mut c, mut cio) = (client(), FakeIo { cursor: (100, 100), ..Default::default() });

        // Director cursor touches its left edge.
        d.on_local(&mut dio, Local::CursorMoved((0, 1080)), t);
        assert!(dio.grabbed);
        let (_, enter) = dio.sent.last().cloned().unwrap();
        assert!(matches!(enter, Message::Enter { x: 3838, .. }), "entered at the client's right edge");

        // Client receives Enter: cursor appears at its right edge, acks.
        c.on_message(&mut cio, 0, enter, t);
        assert_eq!(cio.cursor.0, 3838);
        let (_, ack) = cio.sent.last().cloned().unwrap();
        d.on_message(&mut dio, 0, ack, t);
        assert!(matches!(d.focus(), Focus::Remote { acked: true, .. }));

        // Input flows; a key is pressed and held.
        let s = session_of(&dio);
        let key = InputEvent::Key { key: Key(0x04), down: true };
        d.on_local(&mut dio, Local::Input(key), t);
        let (_, m) = dio.sent.last().cloned().unwrap();
        c.on_message(&mut cio, 0, m, t);
        assert_eq!(cio.injected, vec![key]);

        // Mouse moves right past the client's right edge -> LeaveRequest.
        c.on_message(&mut cio, 0, Message::Input { session: s, event: InputEvent::MouseMove { dx: 10, dy: 0 } }, t);
        let (_, req) = cio.sent.last().cloned().unwrap();
        assert!(matches!(req, Message::LeaveRequest { edge: Edge::Right, .. }));

        // Director takes the cursor back at its left edge and confirms.
        d.on_message(&mut dio, 0, req, t);
        assert!(!dio.grabbed);
        assert_eq!(d.focus(), Focus::Local);
        assert_eq!(dio.cursor.0, 1);
        let (_, leave) = dio.sent.last().cloned().unwrap();
        c.on_message(&mut cio, 0, leave, t);
        assert!(!c.is_controlled());
        assert_eq!(cio.releases, 1, "held key released on leave");
    }

    #[test]
    fn peer_shutdown_returns_cursor_immediately() {
        let t = Instant::now();
        let (mut d, mut io) = (director(), FakeIo::default());
        d.on_local(&mut io, Local::CursorMoved((0, 500)), t);
        assert!(io.grabbed);
        d.on_message(&mut io, 0, Message::Goodbye { reason: desklink_proto::GoodbyeReason::Shutdown }, t);
        assert!(!io.grabbed);
        assert_eq!(d.focus(), Focus::Local);
        assert_eq!(io.cursor, (1920, 1080), "cursor in the middle of the primary monitor");
        // And no switching back to a gone peer.
        d.on_local(&mut io, Local::CursorMoved((0, 500)), t);
        assert!(!io.grabbed);
    }

    #[test]
    fn link_loss_returns_cursor() {
        let t = Instant::now();
        let (mut d, mut io) = (director(), FakeIo::default());
        d.on_local(&mut io, Local::CursorMoved((0, 500)), t);
        d.on_disconnected(&mut io, 0);
        assert!(!io.grabbed);
        assert_eq!(d.focus(), Focus::Local);
        assert_eq!(io.cursor, (1920, 1080), "centered");
    }

    #[test]
    fn missing_ack_returns_cursor() {
        let t = Instant::now();
        let (mut d, mut io) = (director(), FakeIo::default());
        d.on_local(&mut io, Local::CursorMoved((0, 500)), t);
        d.on_tick(&mut io, t + Duration::from_millis(500));
        assert!(io.grabbed, "still waiting");
        d.on_tick(&mut io, t + ENTER_ACK_TIMEOUT + Duration::from_millis(1));
        assert!(!io.grabbed);
        assert_eq!(io.cursor, (1920, 1080), "centered");
    }

    #[test]
    fn client_releases_keys_when_director_vanishes() {
        let t = Instant::now();
        let (mut c, mut io) = (client(), FakeIo::default());
        c.on_message(&mut io, 0, Message::Enter { session: 3, x: 3838, y: 1080 }, t);
        let down = InputEvent::MouseButton { button: MouseButton::Left, down: true };
        c.on_message(&mut io, 0, Message::Input { session: 3, event: down }, t);
        c.on_disconnected(&mut io, 0);
        assert_eq!(io.releases, 1);
        assert!(!c.is_controlled());
        // Stale input from the old session is ignored.
        c.on_message(&mut io, 0, Message::Input { session: 3, event: down }, t);
        assert_eq!(io.injected.len(), 1);
    }

    #[test]
    fn hotkey_return() {
        let t = Instant::now();
        let (mut d, mut io) = (director(), FakeIo::default());
        d.on_local(&mut io, Local::CursorMoved((0, 500)), t);
        let s = session_of(&io);
        d.on_local(&mut io, Local::GrabReleased, t);
        assert_eq!(d.focus(), Focus::Local);
        assert_eq!(io.sent.last().unwrap().1, Message::Leave { session: s });
    }

    #[test]
    fn skipped_or_disconnected_peer_is_not_entered() {
        let t = Instant::now();
        let (mut d, mut io) = (director(), FakeIo::default());
        d.set_skip(0, true);
        d.on_local(&mut io, Local::CursorMoved((0, 500)), t);
        assert!(!io.grabbed);
        // Right edge has no peer at all.
        d.set_skip(0, false);
        d.on_local(&mut io, Local::CursorMoved((5759, 500)), t);
        assert!(!io.grabbed);
    }

    #[test]
    fn fast_push_past_edge_switches_immediately() {
        let t = Instant::now();
        let (mut d, mut io) = (director(), FakeIo { cursor: (20, 1080), ..Default::default() });
        // Hook reports the unclipped target, 12 px left of the screen.
        d.on_local(&mut io, Local::CursorMoved((-12, 1080)), t);
        assert!(io.grabbed, "switched on the first off-screen event");
        let enter = io.sent.iter().find_map(|(_, m)| match m {
            Message::Enter { x, y, .. } => Some((*x, *y)),
            _ => None,
        });
        // Halfway down our left edge: halfway down the client's right edge.
        let (x, y) = enter.unwrap();
        assert_eq!(x, 3838);
        assert!((1078..=1081).contains(&y));
    }

    #[test]
    fn off_screen_point_past_a_side_without_peer_does_nothing() {
        let t = Instant::now();
        let (mut d, mut io) = (director(), FakeIo { cursor: (100, 5), ..Default::default() });
        d.on_local(&mut io, Local::CursorMoved((100, -9)), t); // top: no peer
        assert!(!io.grabbed);
    }

    #[test]
    fn without_parking_cursor_stays_at_the_edge() {
        let t = Instant::now();
        let (mut d, mut io) = (director(), FakeIo { cursor: (0, 700), ..Default::default() });
        d.set_park(None);
        d.on_local(&mut io, Local::CursorMoved((-5, 700)), t);
        assert!(io.grabbed);
        assert_eq!(io.cursor, (0, 700));
    }

    /// The reported bug: flashunreal (left) crossed into dpolyakov-dt (right)
    /// with its own mouse; then dpolyakov-dt's mouse must be able to take the
    /// machine back and cross left.
    #[test]
    fn a_pc_that_cannot_take_input_hands_it_back() {
        let t = Instant::now();
        let (mut left, mut lio) = (client(), FakeIo { cursor: (3830, 900), ..Default::default() });
        let (mut right, mut rio) = (director(), FakeIo { cursor: (2000, 1000), ..Default::default() });
        left.on_local(&mut lio, Local::CursorMoved((3845, 900)), t);
        let enter = lio.sent.last().unwrap().1.clone();
        right.on_message(&mut rio, 0, enter, t);
        assert!(right.is_controlled());

        // Its screensaver refuses input: it hands back at once.
        right.hand_back(&mut rio, "screensaver");
        assert!(!right.is_controlled());
        let back = rio.sent.last().unwrap().1.clone();
        assert!(matches!(back, Message::Takeover { local_use: false, .. }));
        left.on_message(&mut lio, 0, back, t);
        assert!(!lio.grabbed, "the user's own PC has its mouse again");
        assert!(!lio.hidden);
        assert_eq!(left.focus(), Focus::Local);
    }
    #[test]
    fn local_use_takes_controlled_machine_back() {
        let t = Instant::now();
        // flashunreal: peer on its right. dpolyakov-dt: peer on its left.
        let (mut left, mut lio) = (client(), FakeIo { cursor: (3830, 900), ..Default::default() });
        let (mut right, mut rio) = (director(), FakeIo { cursor: (2000, 1000), ..Default::default() });

        // flashunreal's mouse pushes past its right edge.
        left.on_local(&mut lio, Local::CursorMoved((3845, 900)), t);
        assert!(lio.grabbed);
        let enter = lio.sent.last().unwrap().1.clone();
        right.on_message(&mut rio, 0, enter, t);
        assert!(right.is_controlled());
        assert_eq!(rio.watch, Some(Watch::Takeover));

        // Someone moves dpolyakov-dt's own mouse.
        right.on_local(&mut rio, Local::Activity, t);
        assert!(!right.is_controlled());
        assert!(!rio.hidden, "the person at this PC keeps a visible cursor");
        let takeover = rio.sent.last().unwrap().1.clone();
        assert!(matches!(takeover, Message::Takeover { .. }));

        // flashunreal gives up at once and hides its cursor.
        left.on_message(&mut lio, 0, takeover, t);
        assert!(!lio.grabbed);
        assert_eq!(left.focus(), Focus::Local);
        assert!(lio.hidden);
        assert_eq!(lio.watch, Some(Watch::Reveal));

        // Now dpolyakov-dt's mouse crosses left into flashunreal.
        right.on_local(&mut rio, Local::CursorMoved((-4, 1000)), t);
        assert!(rio.grabbed);
        let enter = rio.sent.last().unwrap().1.clone();
        left.on_message(&mut lio, 0, enter, t);
        assert!(left.is_controlled());
        assert!(!lio.hidden);
    }

    /// The reported hang: right took itself back but the Takeover never
    /// reached left, which kept swallowing its own mouse.
    fn stuck_pair() -> (Core, FakeIo, Core, FakeIo) {
        let t = Instant::now();
        let (mut left, mut lio) = (client(), FakeIo { cursor: (3830, 900), ..Default::default() });
        let (mut right, mut rio) = (director(), FakeIo { cursor: (2000, 1000), ..Default::default() });
        left.on_local(&mut lio, Local::CursorMoved((3845, 900)), t);
        let enter = lio.sent.last().unwrap().1.clone();
        right.on_message(&mut rio, 0, enter, t);
        let ack = rio.sent.last().unwrap().1.clone();
        left.on_message(&mut lio, 0, ack, t);
        right.on_local(&mut rio, Local::Activity, t);
        rio.sent.clear(); // the Takeover is lost
        assert!(lio.grabbed && !right.is_controlled());
        (left, lio, right, rio)
    }

    #[test]
    fn lost_takeover_heals_at_first_mouse_move() {
        let t = Instant::now();
        let (mut left, mut lio, mut right, mut rio) = stuck_pair();
        // The user touches left's mouse: left forwards it to right...
        left.on_local(&mut lio, Local::Input(InputEvent::MouseMove { dx: 3, dy: 0 }), t);
        let input = lio.sent.last().unwrap().1.clone();
        right.on_message(&mut rio, 0, input.clone(), t);
        // ...which answers "you are not controlling me", once.
        let nack = rio.sent.last().unwrap().1.clone();
        assert!(matches!(nack, Message::Takeover { local_use: false, .. }));
        right.on_message(&mut rio, 0, input, t);
        assert_eq!(rio.sent.len(), 1, "not repeated for the same session");
        left.on_message(&mut lio, 0, nack, t);
        assert!(!lio.grabbed, "left releases its mouse");
        assert!(!lio.hidden, "left's user is at left: cursor visible");
        assert_eq!(left.focus(), Focus::Local);
    }

    #[test]
    fn entered_while_directing_releases_grab() {
        let t = Instant::now();
        let (mut left, mut lio, mut right, mut rio) = stuck_pair();
        // The user moves right's cursor onto left.
        right.on_local(&mut rio, Local::CursorMoved((-4, 1000)), t);
        let enter = rio.sent.last().unwrap().1.clone();
        left.on_message(&mut lio, 0, enter, t);
        assert!(!lio.grabbed, "left no longer swallows its own mouse");
        assert!(left.is_controlled());
        assert_eq!(left.focus(), Focus::Local);
    }

    #[test]
    fn clipboard_follows_the_user() {
        let t = Instant::now();
        let (mut d, mut dio) = (director(), FakeIo::default());
        let (mut c, mut cio) = (client(), FakeIo::default());
        // Director -> client: the director's clipboard goes to the client.
        d.on_local(&mut dio, Local::CursorMoved((-3, 900)), t);
        assert_eq!(dio.shared_to, vec![0]);
        let enter = dio.sent.last().unwrap().1.clone();
        c.on_message(&mut cio, 0, enter, t);
        assert!(cio.shared_to.is_empty(), "entering doesn't send the client's clipboard");
        // Back to the director: the client's clipboard goes with the user.
        let s = session_of(&dio);
        c.on_message(&mut cio, 0, Message::Leave { session: s }, t);
        assert_eq!(cio.shared_to, vec![0]);
    }

    #[test]
    fn clipboard_on_takeover_only_for_local_use() {
        let t = Instant::now();
        for (local_use, expect) in [(true, vec![0]), (false, vec![])] {
            let (mut d, mut io) = (director(), FakeIo::default());
            d.on_local(&mut io, Local::CursorMoved((-3, 900)), t);
            io.shared_to.clear();
            let s = session_of(&io);
            d.on_message(&mut io, 0, Message::Takeover { session: s, local_use }, t);
            assert_eq!(io.shared_to, expect, "local_use={local_use}");
        }
    }

    #[test]
    fn commands_run_where_the_user_is() {
        let t = Instant::now();
        // Local: runs here.
        let (mut d, mut io) = (director(), FakeIo::default());
        d.on_local(&mut io, Local::Command(Command::PasteFiles), t);
        assert_eq!(io.commands, vec![Command::PasteFiles]);

        // Directing: forwarded to the controlled machine, which runs it.
        let (mut d, mut dio) = (director(), FakeIo::default());
        let (mut c, mut cio) = (client(), FakeIo::default());
        d.on_local(&mut dio, Local::CursorMoved((-3, 900)), t);
        let enter = dio.sent.last().unwrap().1.clone();
        c.on_message(&mut cio, 0, enter, t);
        d.on_local(&mut dio, Local::Command(Command::PasteFiles), t);
        assert!(dio.commands.is_empty());
        let fwd = dio.sent.last().unwrap().1.clone();
        assert_eq!(fwd, Message::Command(Command::PasteFiles));
        c.on_message(&mut cio, 0, fwd.clone(), t);
        assert_eq!(cio.commands, vec![Command::PasteFiles]);

        // A peer that isn't controlling us can't run commands here.
        let (mut c2, mut io2) = (client(), FakeIo::default());
        c2.on_message(&mut io2, 0, fwd, t);
        assert!(io2.commands.is_empty());
    }


    fn one_file() -> FileOffer {
        FileOffer {
            id: 42,
            entries: vec![desklink_proto::FileEntry { path: "a.bin".into(), is_dir: false, size: 10, modified_ms: 0 }],
        }
    }

    #[test]
    fn dragging_files_across_continues_the_drag_on_the_client() {
        let t = Instant::now();
        let (mut d, mut dio) = (director(), FakeIo { left_held: true, cursor: (0, 900), ..Default::default() });
        let (mut c, mut cio) = (client(), FakeIo::default());
        d.on_local(&mut dio, Local::CursorMoved((-3, 900)), t);
        // Switched at once; the drag check runs in the background with the
        // cursor kept at the edge (not parked yet), button carried.
        assert!(dio.grabbed && dio.carried);
        assert_eq!(dio.catches, 1);
        assert!(!dio.parked);
        assert_eq!(dio.cursor, (0, 900));
        d.on_local(&mut dio, Local::DragCaught(Some(one_file())), t);
        assert!(dio.parked && dio.cursor == (3839, 2159), "parked once known");
        let msgs: Vec<Message> = dio.sent.iter().map(|(_, m)| m.clone()).collect();
        assert!(matches!(msgs[0], Message::Enter { .. }));
        assert!(matches!(msgs[1], Message::DragStart { .. }), "drag follows enter");
        for m in msgs {
            c.on_message(&mut cio, 0, m, t);
        }
        assert!(cio.injected.is_empty(), "the drag presses the button itself, on its own window");
        assert_eq!(cio.drags, vec![one_file()]);
    }

    #[test]
    fn no_drag_without_a_held_button_or_files() {
        let t = Instant::now();
        let (mut d, mut io) = (director(), FakeIo::default());
        d.on_local(&mut io, Local::CursorMoved((-3, 900)), t);
        assert_eq!(io.catches, 0, "not even checked");
        assert!(!io.carried);
        // Button held but nothing file-like dragged (e.g. selecting text):
        // just park as usual.
        let (mut d, mut io) = (director(), FakeIo { left_held: true, ..Default::default() });
        d.on_local(&mut io, Local::CursorMoved((-3, 900)), t);
        d.on_local(&mut io, Local::DragCaught(None), t);
        assert!(io.parked);
        assert!(!io.sent.iter().any(|(_, m)| matches!(m, Message::DragStart { .. })));
        // A late result after returning home is ignored.
        let (mut d, mut io) = (director(), FakeIo { left_held: true, ..Default::default() });
        d.on_local(&mut io, Local::CursorMoved((-3, 900)), t);
        d.on_local(&mut io, Local::GrabReleased, t);
        d.on_local(&mut io, Local::DragCaught(Some(one_file())), t);
        assert!(!io.sent.iter().any(|(_, m)| matches!(m, Message::DragStart { .. })));
    }


    #[test]
    fn drag_is_cancelled_when_the_user_leaves_or_the_link_drops() {
        let t = Instant::now();
        let start = |c: &mut Core, io: &mut FakeIo| {
            c.on_message(io, 0, Message::Enter { session: 1, x: 3838, y: 1080 }, t);
            c.on_message(io, 0, Message::DragStart { session: 1, files: one_file() }, t);
        };
        let (mut c, mut io) = (client(), FakeIo::default());
        start(&mut c, &mut io);
        c.on_message(&mut io, 0, Message::Leave { session: 1 }, t);
        assert_eq!(io.drag_cancels, 1, "cancelled before the button is released");
        let (mut c, mut io) = (client(), FakeIo::default());
        start(&mut c, &mut io);
        c.on_disconnected(&mut io, 0);
        assert_eq!(io.drag_cancels, 1);
        // A drag for another session is ignored.
        let (mut c, mut io) = (client(), FakeIo::default());
        c.on_message(&mut io, 0, Message::Enter { session: 1, x: 3838, y: 1080 }, t);
        c.on_message(&mut io, 0, Message::DragStart { session: 9, files: one_file() }, t);
        assert!(io.drags.is_empty());
    }

    #[test]
    fn dragging_files_back_continues_the_drag_on_the_director() {
        let t = Instant::now();
        let (mut d, mut dio) = (director(), FakeIo::default());
        let (mut c, mut cio) = (client(), FakeIo { cursor: (100, 100), ..Default::default() });
        d.on_local(&mut dio, Local::CursorMoved((0, 1080)), t);
        let (_, enter) = dio.sent.last().cloned().unwrap();
        c.on_message(&mut cio, 0, enter, t);
        let s = session_of(&dio);

        // The user drags files on the client to its edge facing the director.
        cio.left_held = true;
        c.on_message(&mut cio, 0, Message::Input { session: s, event: InputEvent::MouseMove { dx: 10, dy: 0 } }, t);
        assert_eq!(cio.catches, 0, "no drag check yet: maybe nothing is beyond this edge");
        let (_, req) = cio.sent.last().cloned().unwrap();
        assert!(matches!(req, Message::LeaveRequest { .. }));

        // The director takes the cursor back; the held button's release stays local.
        d.on_message(&mut dio, 0, req, t);
        assert!(dio.carried_back);
        let (_, leave) = dio.sent.last().cloned().unwrap();
        c.on_message(&mut cio, 0, leave, t);
        assert_eq!(cio.catches, 1, "really leaving with the button held: check for a drag");
        assert_eq!(cio.releases, 0, "no release (and accidental local drop) while checking");

        // The check finds files: the client announces them, then releases.
        c.on_local(&mut cio, Local::DragCaught(Some(one_file())), t);
        assert_eq!(cio.releases, 1);
        let (_, drag) = cio.sent.last().cloned().unwrap();
        assert!(matches!(drag, Message::DragStart { session, .. } if session == s));

        // Button still down on the director: the drag continues there.
        dio.left_held = true;
        d.on_message(&mut dio, 0, drag.clone(), t);
        assert_eq!(dio.drags.len(), 1);

        // Already let go (or a stale session): nothing starts.
        let (mut d2, mut dio2) = (director(), FakeIo::default());
        d2.on_message(&mut dio2, 0, drag, t);
        assert!(dio2.drags.is_empty());
    }

    #[test]
    fn pushing_past_a_dead_edge_keeps_a_selection_alone() {
        // Selecting text up to an edge with nothing beyond: no request goes
        // out (peer mode), and the director ignores one that says nothing is
        // there (a client following its master's layout).
        let t = Instant::now();
        let (mut d, mut dio) = (director(), FakeIo::default());
        let (mut c, mut cio) = (client(), FakeIo { cursor: (100, 100), left_held: true, ..Default::default() });
        d.on_local(&mut dio, Local::CursorMoved((0, 1080)), t);
        let (_, enter) = dio.sent.last().cloned().unwrap();
        c.on_message(&mut cio, 0, enter, t);
        let s = session_of(&dio);
        // The client's top edge: nothing above it.
        cio.cursor = (500, 0);
        let sent = cio.sent.len();
        c.on_message(&mut cio, 0, Message::Input { session: s, event: InputEvent::MouseMove { dx: 0, dy: -10 } }, t);
        assert_eq!(cio.sent.len(), sent, "its own layout has nothing above: no request");
        let d_sent = dio.sent.len();
        d.on_message(&mut dio, 0, Message::LeaveRequest { session: s, edge: Edge::Top, x: 500, y: 0, to: None }, t);
        assert_eq!(dio.sent.len(), d_sent, "nothing beyond on the director's desk either: no Leave");
        assert!(dio.grabbed);
        assert_eq!(cio.catches, 0);
        assert_eq!(cio.releases, 0);
    }


    #[test]
    fn corners_do_not_switch_when_guarded() {
        let t = Instant::now();
        let (mut d, mut io) = (director(), FakeIo::default());
        d.set_corner_guard(Some(15));
        // The very top of the 4K's left edge: aiming at a corner, no switch.
        d.on_local(&mut io, Local::CursorMoved((0, 3)), t);
        assert!(!io.grabbed);
        // Further down the edge: switches.
        d.on_local(&mut io, Local::CursorMoved((0, 1080)), t);
        assert!(io.grabbed);
    }
    #[test]
    fn games_holding_the_mouse_keep_the_cursor_when_guarded() {
        let t = Instant::now();
        let (mut d, mut io) = (director(), FakeIo { fullscreen: true, ..Default::default() });
        // Not guarded: the edge switches anyway.
        d.on_local(&mut io, Local::CursorMoved((0, 1080)), t);
        assert!(io.grabbed);

        let (mut d, mut io) = (director(), FakeIo { fullscreen: true, ..Default::default() });
        d.set_fullscreen_guard(true);
        d.on_local(&mut io, Local::CursorMoved((0, 1080)), t);
        assert!(!io.grabbed, "a game holds the mouse: stay");
        io.fullscreen = false;
        d.on_local(&mut io, Local::CursorMoved((0, 1080)), t);
        assert!(io.grabbed, "its menu, or another program: switches again");
    }
    #[test]
    fn in_peer_mode_the_layout_of_the_pc_the_cursor_is_on_rules() {
        // The user's two PCs disagree: the 4K says Flash is above it, Flash
        // says the 4K is to its right.
        let t = Instant::now();
        let four_k = [mon(0, 3840, 2160, true), mon(3840, 1920, 1200, false)];
        let flash = [mon(0, 3840, 2160, true)];
        let mut k = Core::new(&four_k, vec![PeerSlot { name: "flash".into(), edge: Edge::Top, offset: None, screens: None, skip: false }]);
        k.on_connected(0, &flash);
        let mut f = Core::new(&flash, vec![PeerSlot { name: "4k".into(), edge: Edge::Right, offset: None, screens: None, skip: false }]);
        f.on_connected(0, &four_k);
        let (mut kio, mut fio) = (FakeIo::default(), FakeIo { cursor: (100, 100), ..Default::default() });
        // On the 4K (its keyboard and mouse): up leads to Flash, entering at
        // Flash's bottom edge.
        k.on_local(&mut kio, Local::CursorMoved((1920, 0)), t);
        let (_, enter) = kio.sent.last().cloned().unwrap();
        f.on_message(&mut fio, 0, enter, t);
        assert_eq!(fio.cursor.1, 2158, "appears at Flash's bottom");
        let s = session_of(&kio);
        // On Flash: down leads nowhere by Flash's layout...
        fio.cursor = (1920, 2159);
        let sent = fio.sent.len();
        f.on_message(&mut fio, 0, Message::Input { session: s, event: InputEvent::MouseMove { dx: 0, dy: 10 } }, t);
        assert_eq!(fio.sent.len(), sent);
        // ...right leads to the 4K, landing on its left edge.
        fio.cursor = (3839, 1080);
        f.on_message(&mut fio, 0, Message::Input { session: s, event: InputEvent::MouseMove { dx: 10, dy: 0 } }, t);
        let (_, req) = fio.sent.last().cloned().unwrap();
        assert!(matches!(req, Message::LeaveRequest { edge: Edge::Right, to: Some(_), .. }));
        k.on_message(&mut kio, 0, req, t);
        assert!(!kio.grabbed, "back on the 4K");
        assert_eq!(kio.cursor.0, 1, "on its left edge, as Flash's layout says");
    }

    #[test]
    fn placed_pcs_meet_where_their_screens_touch_and_hop_between_clients() {
        let t = Instant::now();
        // Director: 4K + 1920x1200 to its right. "a" sits left of the 4K
        // (lower half only), "b" below the 1920x1200.
        let mut d = Core::new(
            &[mon_at(0, 0, 3840, 2160, true), mon_at(3840, 0, 1920, 1200, false)],
            vec![
                PeerSlot { name: "a".into(), edge: Edge::Left, offset: Some((-1920, 1080)), screens: None, skip: false },
                PeerSlot { name: "b".into(), edge: Edge::Bottom, offset: Some((3840, 1200)), screens: None, skip: false },
            ],
        );
        d.on_connected(0, &[mon_at(0, 0, 1920, 1080, true)]);
        d.on_connected(1, &[mon_at(0, 0, 1920, 1080, true)]);
        let mut io = FakeIo::default();
        // Upper half of the 4K's left edge: nothing there.
        d.on_local(&mut io, Local::CursorMoved((0, 500)), t);
        assert!(!io.grabbed);
        // Lower half: into "a" at the matching height.
        d.on_local(&mut io, Local::CursorMoved((0, 1500)), t);
        assert!(io.grabbed);
        assert!(matches!(io.sent.last().unwrap(), (0, Message::Enter { x: 1918, y: 420, .. })));
        let s = session_of(&io);
        d.on_message(&mut io, 0, Message::EnterAck { session: s, locks: LockKeys::default() }, t);
        // "a" pushes past its right edge high up: back on the 4K there.
        d.on_message(&mut io, 0, Message::LeaveRequest { session: s, edge: Edge::Right, x: 1919, y: 100, to: None }, t);
        assert!(!io.grabbed);
        assert_eq!(io.cursor, (1, 1180));
        // Over to "b" through the bottom of the 1920x1200, then from "b"
        // left onto the lower right part of the 4K.
        d.on_local(&mut io, Local::CursorMoved((4000, 1199)), t);
        assert!(matches!(io.sent.last().unwrap(), (1, Message::Enter { x: 160, y: 1, .. })));
        let s = session_of(&io);
        d.on_message(&mut io, 1, Message::LeaveRequest { session: s, edge: Edge::Left, x: 0, y: 500, to: None }, t);
        assert!(!io.grabbed);
        assert_eq!(io.cursor, (3838, 1700));
        // A hop from "a" straight to "b" when they touch: put "b" right below "a".
        d.set_offset(1, Some((-1920, 2160)));
        d.on_local(&mut io, Local::CursorMoved((0, 1500)), t);
        let s = session_of(&io);
        d.on_message(&mut io, 0, Message::LeaveRequest { session: s, edge: Edge::Bottom, x: 700, y: 1079, to: None }, t);
        assert!(io.grabbed, "still directing, now another PC");
        let n = io.sent.len();
        assert!(matches!(io.sent[n - 2], (0, Message::Leave { session }) if session == s));
        assert!(matches!(io.sent[n - 1], (1, Message::Enter { x: 700, y: 1, .. })));
        assert!(matches!(d.focus(), Focus::Remote { peer: 1, .. }));
    }

    #[test]
    fn leaving_without_files_releases_after_the_check() {
        let t = Instant::now();
        let (mut c, mut io) = (client(), FakeIo { cursor: (3838, 500), left_held: true, ..Default::default() });
        c.on_message(&mut io, 0, Message::Enter { session: 1, x: 3838, y: 1080 }, t);
        c.on_message(&mut io, 0, Message::Input { session: 1, event: InputEvent::MouseMove { dx: 10, dy: 0 } }, t);
        c.on_message(&mut io, 0, Message::Leave { session: 1 }, t);
        c.on_local(&mut io, Local::DragCaught(None), t);
        assert_eq!(io.releases, 1);
        assert!(!io.sent.iter().any(|(_, m)| matches!(m, Message::DragStart { .. })));
    }
    #[test]
    fn stale_takeover_is_ignored() {
        let t = Instant::now();
        let (mut d, mut io) = (director(), FakeIo::default());
        d.on_local(&mut io, Local::CursorMoved((0, 500)), t);
        let s = session_of(&io);
        d.on_message(&mut io, 0, Message::Takeover { session: s + 7, local_use: true }, t);
        assert!(io.grabbed, "takeover for another session must not release");
    }

    #[test]
    fn controlled_machine_does_not_switch_out() {
        let t = Instant::now();
        let (mut c, mut io) = (client(), FakeIo::default());
        c.on_message(&mut io, 0, Message::Enter { session: 1, x: 3838, y: 1080 }, t);
        // Client's own mouse touches its right edge (where the director is).
        c.on_local(&mut io, Local::CursorMoved((3839, 100)), t);
        assert!(!io.grabbed);
        assert_eq!(c.focus(), Focus::Local);
    }

    #[test]
    fn leave_request_is_rate_limited_and_motion_clamped() {
        let t = Instant::now();
        let (mut c, mut io) = (client(), FakeIo::default());
        c.on_message(&mut io, 0, Message::Enter { session: 1, x: 3838, y: 1080 }, t);
        let mv = Message::Input { session: 1, event: InputEvent::MouseMove { dx: 50, dy: 0 } };
        c.on_message(&mut io, 0, mv.clone(), t);
        c.on_message(&mut io, 0, mv.clone(), t + Duration::from_millis(10));
        let reqs = io.sent.iter().filter(|(_, m)| matches!(m, Message::LeaveRequest { .. })).count();
        assert_eq!(reqs, 1);
        assert_eq!(io.cursor.0, 3839, "clamped on screen");
        c.on_message(&mut io, 0, mv, t + LEAVE_RETRY);
        let reqs = io.sent.iter().filter(|(_, m)| matches!(m, Message::LeaveRequest { .. })).count();
        assert_eq!(reqs, 2, "retried");
    }
}
