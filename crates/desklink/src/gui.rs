//! Settings window (`desklink settings`): edits desklink.cfg. Runs as its own
//! process so a graphics-driver problem can never take input sharing down.

use desklink_config::{Config, FilesMode, HideMethod, OnConflict, ParkAt, Paths, Peer, Role, Side};
use crate::layout::{Desk, PlacedPeer, Rect};
use desklink_proto::Monitor;
use sha2::{Digest, Sha256};
use slint::{Model, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::rc::Rc;

slint::include_modules!();

/// Space the layout is fitted into (logical px; the canvas is about this big).
const CANVAS: (f32, f32) = (860.0, 470.0);

const SIDES: [Side; 4] = [Side::Left, Side::Right, Side::Top, Side::Bottom];
const CONFLICTS: [OnConflict; 4] = [OnConflict::Ask, OnConflict::Overwrite, OnConflict::Rename, OnConflict::Skip];
// In the order the window shows them.
const PARKS: [ParkAt; 5] = [ParkAt::TopLeft, ParkAt::TopRight, ParkAt::BottomLeft, ParkAt::BottomRight, ParkAt::Center];
const METHODS: [HideMethod; 3] = [HideMethod::Transparent, HideMethod::ParkOnly, HideMethod::None];
const ROLES: [Role; 3] = [Role::Peer, Role::Master, Role::Client];

fn index_of<T: PartialEq>(all: &[T], v: &T) -> i32 {
    all.iter().position(|x| x == v).unwrap_or(0) as i32
}

/// `ab12-cd34-ef56-7890`, as DeskLink prints fingerprints.
fn short_fingerprint(hex: &str) -> String {
    hex.as_bytes().chunks(4).take(4).map(|c| std::str::from_utf8(c).unwrap_or("")).collect::<Vec<_>>().join("-")
}

fn own_fingerprint(paths: &Paths) -> String {
    match std::fs::read(&paths.cert) {
        Ok(der) => short_fingerprint(&Sha256::digest(&der).iter().map(|b| format!("{b:02x}")).collect::<String>()),
        Err(_) => "created when DeskLink first runs".into(),
    }
}

fn keycaps(hotkey: &str) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(hotkey.split('+').map(|k| SharedString::from(k.trim())).collect::<Vec<_>>()))
}

/// Canvas = offset + desk * scale (the desk is in physical pixels).
#[derive(Clone, Copy, Default)]
struct View {
    scale: f32,
    ox: f32,
    oy: f32,
}

struct Gui {
    paths: Paths,
    /// This PC's monitors (names for the tiles).
    local: Vec<Monitor>,
    /// The layout as shown (maybe not applied yet).
    desk: Desk,
    view: View,
    monitors: Rc<VecModel<Mon>>,
    slots: Rc<VecModel<Slot>>,
    /// Client: the layout its master pushed (in memory only).
    pushed: Option<Vec<desklink_proto::LayoutPc>>,
    /// Client: the master it was adopted by, if any (connected or not).
    known_master: Option<String>,
    /// The role the Layout tab was drawn for.
    role: Role,
    /// Peer being dragged and how far so far (canvas pixels).
    drag: Option<(usize, f32, f32)>,
}

impl Gui {
    /// Re-read the file (DeskLink itself writes it too: pinned fingerprints,
    /// learned addresses), apply `edit`, save. Returns the saved config.
    fn update(&self, state: &State, edit: impl FnOnce(&mut Config) -> Result<(), String>) -> Option<Config> {
        let result = Config::load_or_default(&self.paths.config).and_then(|mut cfg| {
            edit(&mut cfg)?;
            cfg.save_if_changed(&self.paths.config)?;
            Ok(cfg)
        });
        match result {
            Ok(cfg) => {
                status(state, "Saved.", false);
                Some(cfg)
            }
            Err(e) => {
                status(state, &e, true);
                None
            }
        }
    }

    /// Pick up screens DeskLink learned since the window opened (PCs that
    /// connected meanwhile), so a drop uses their real size.
    fn refresh_monitors(&mut self) {
        let Ok(cfg) = Config::load_or_default(&self.paths.config) else { return };
        for p in &mut self.desk.peers {
            if let Some(q) = cfg.peers.iter().find(|q| q.name == p.name && !q.monitors.is_empty()) {
                p.monitors = q.monitors.clone();
            }
        }
    }

    /// Show the layout from the config (dropping unapplied changes).
    fn reset_layout(&mut self, state: &State, cfg: &Config) {
        // As DeskLink will run it: peers placed before their real size was
        // known moved clear of our screens.
        let mut cfg = cfg.clone();
        cfg.fix_overlaps(&self.local);
        self.desk = Desk::new(&self.local, cfg.peers.iter().map(PlacedPeer::from_config).collect());
        self.show_layout(state, &cfg);
    }

    /// Draw `self.desk`, fitted to the canvas, with "add a PC" spots on free
    /// sides. A client shows its master's layout instead (read-only).
    fn show_layout(&mut self, state: &State, cfg: &Config) {
        self.role = cfg.role;
        state.set_layout_readonly(cfg.role == Role::Client);
        if cfg.role == Role::Client {
            let me = cfg.name.clone();
            // A client keeps its master in its list (adopted on first connect).
            self.known_master = cfg.peers.first().map(|p| p.name.clone());
            self.show_pushed(state, &me);
            return;
        }
        let taken = |side: Side| self.desk.peers.iter().any(|p| !p.unplaced && p.side == side);
        let free: Vec<(usize, Rect)> = SIDES.iter().enumerate().filter(|(_, s)| !taken(**s)).map(|(i, s)| (i, self.desk.free_spot(*s))).collect();
        let others = if cfg.role == Role::Master { "Client" } else { "Peer" };
        let mut tiles = Vec::new();
        for (m, r) in self.local.iter().zip(&self.desk.local) {
            tiles.push(Tile { monitor: Some(m.clone()), owner: String::new(), local: true, badge: "This PC", rect: *r });
        }
        for (i, p) in self.desk.peers.iter().enumerate() {
            for (k, r) in self.desk.peer_rects(i).into_iter().enumerate() {
                tiles.push(Tile { monitor: p.monitors.get(k).cloned(), owner: p.name.clone(), local: false, badge: others, rect: r });
            }
        }
        self.draw(tiles, &free);
        state.set_layout_pcs(ModelRc::new(VecModel::from(
            self.desk.peers.iter().map(|p| LayoutItem { name: p.name.clone().into(), placed: !p.unplaced }).collect::<Vec<_>>(),
        )));
        state.set_layout_note("".into());
        let mut applied = cfg.clone();
        applied.fix_overlaps(&self.local);
        let applied: Vec<PlacedPeer> = applied.peers.iter().map(PlacedPeer::from_config).collect();
        let key = |p: &PlacedPeer| (p.name.clone(), p.side, p.position, p.unplaced);
        state.set_layout_dirty(applied.iter().map(key).ne(self.desk.peers.iter().map(key)));
    }

    /// Client: the layout its master pushed (in memory only), read-only.
    fn show_pushed(&mut self, state: &State, me: &str) {
        state.set_layout_dirty(false);
        let Some(pcs) = self.pushed.clone().filter(|p| !p.is_empty()) else {
            // Until then, just this PC's own screens.
            let mine: Vec<Tile> = self.local.iter().map(|m| Tile { monitor: Some(m.clone()), owner: String::new(), local: true, badge: "This PC", rect: (m.x, m.y, m.width, m.height) }).collect();
            self.draw(mine, &[]);
            let note = match &self.known_master {
                Some(m) => format!("Only the master, {m}, can change the layout. It isn't connected right now."),
                None => "Only the master can change the layout. Waiting for a master to connect.".into(),
            };
            state.set_layout_note(note.into());
            state.set_layout_lock(match &self.known_master {
                Some(m) => format!("Read-only  ·  only {m} (the master) can change it"),
                None => "Read-only  ·  only the master can change it".into(),
            }.into());
            return;
        };
        let master = pcs.iter().find(|p| p.master).map(|p| p.name.clone()).unwrap_or_default();
        let mut tiles = Vec::new();
        for pc in &pcs {
            let mine = pc.name.eq_ignore_ascii_case(me);
            let badge = if mine { "This PC" } else if pc.master { "Master" } else { "Client" };
            let rects: Vec<Rect> = if pc.monitors.is_empty() {
                vec![(0, 0, desklink_config::UNKNOWN_SCREEN.0, desklink_config::UNKNOWN_SCREEN.1)]
            } else {
                pc.monitors.iter().map(|m| (m.x, m.y, m.width, m.height)).collect()
            };
            for (k, r) in rects.into_iter().enumerate() {
                let monitor = pc.monitors.get(k).cloned();
                tiles.push(Tile { monitor, owner: if mine { String::new() } else { pc.name.clone() }, local: mine, badge, rect: (r.0 + pc.x, r.1 + pc.y, r.2, r.3) });
            }
        }
        self.draw(tiles, &[]);
        state.set_layout_note(format!("Arranged by {master} (the master): only it can change the layout.").into());
        state.set_layout_lock(format!("Read-only  ·  only {master} (the master) can change it").into());
    }

    /// Fit `tiles` and `free` spots into the canvas and show them.
    fn draw(&mut self, tiles: Vec<Tile>, free: &[(usize, Rect)]) {
        let all = || tiles.iter().map(|t| &t.rect).chain(free.iter().map(|(_, r)| r));
        let x0 = all().map(|r| r.0).min().unwrap_or(0) as f32;
        let y0 = all().map(|r| r.1).min().unwrap_or(0) as f32;
        let x1 = all().map(|r| r.0 + r.2).max().unwrap_or(1) as f32;
        let y1 = all().map(|r| r.1 + r.3).max().unwrap_or(1) as f32;
        let scale = (CANVAS.0 * 0.9 / (x1 - x0)).min(CANVAS.1 * 0.9 / (y1 - y0)).min(0.12);
        self.view = View { scale, ox: (CANVAS.0 - (x1 - x0) * scale) / 2.0 - x0 * scale, oy: (CANVAS.1 - (y1 - y0) * scale) / 2.0 - y0 * scale };
        let v = self.view;
        let place = |r: &Rect| (v.ox + r.0 as f32 * v.scale, v.oy + r.1 as f32 * v.scale, r.2 as f32 * v.scale, r.3 as f32 * v.scale);
        let mons: Vec<Mon> = tiles
            .iter()
            .map(|t| {
                let (name, res) = match &t.monitor {
                    Some(m) => (monitor_name(m), format!("{} x {}  ·  {}%", m.width, m.height, m.scale_percent)),
                    None => ("Screens".into(), "size shown once connected".into()),
                };
                let (x, y, w, h) = place(&t.rect);
                Mon { name: name.into(), res: res.into(), owner: t.owner.clone().into(), local: t.local, badge: t.badge.into(), x, y, w, h, selected: false }
            })
            .collect();
        self.monitors.set_vec(mons);
        // Free spots: where a PC from the list can be dropped.
        let label = |_: Side| "Drop a PC here";
        self.slots.set_vec(
            free.iter()
                .map(|(i, r)| {
                    let (x, y, w, h) = place(r);
                    Slot { side: *i as i32, label: label(SIDES[*i]).into(), x, y, w, h, occupied: false, target: false }
                })
                .collect::<Vec<_>>(),
        );
    }
}

/// One screen tile on the canvas (desk coordinates).
struct Tile {
    /// `None`: a PC whose screens aren't known yet.
    monitor: Option<Monitor>,
    owner: String,
    local: bool,
    badge: &'static str,
    rect: Rect,
}

fn monitor_name(m: &Monitor) -> String {
    m.name.trim_start_matches(r"\\.\").to_string()
}

/// The header line, plus a short toast at the bottom so a save is noticed.
fn status(state: &State, text: &str, error: bool) {
    // The header has room for one line; the toast shows it all.
    state.set_status(text.lines().next().unwrap_or("").into());
    state.set_status_error(error);
    state.set_toast(text.into());
    state.set_toast_error(error);
    state.set_toast_shown(true);
    let shown = TOAST.with(|t| {
        t.set(t.get().wrapping_add(1));
        t.get()
    });
    let weak = APP.with_borrow(|a| a.clone());
    let hide_after = if error { 5000 } else { 2500 };
    slint::Timer::single_shot(std::time::Duration::from_millis(hide_after), move || {
        // Only the latest toast hides itself.
        if TOAST.with(|t| t.get()) == shown
            && let Some(app) = weak.as_ref().and_then(|w| w.upgrade())
        {
            app.global::<State>().set_toast_shown(false);
        }
    });
}

thread_local! {
    /// The window, for timers that outlive a callback.
    static APP: RefCell<Option<slint::Weak<App>>> = const { RefCell::new(None) };
    /// Latest toast and latest keystroke in a text box (newer ones win).
    static TOAST: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static TYPED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// Latest hover, so a tooltip only appears for the control still hovered.
    static HOVER: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// The window state, for news from other threads (a client's pushed layout).
    static GUI: RefCell<Option<Rc<RefCell<Gui>>>> = const { RefCell::new(None) };
}

fn number(text: &str, what: &str) -> Result<u64, String> {
    text.trim().parse().map_err(|_| format!("{what}: '{text}' is not a whole number"))
}

/// Copy the window's settings into `cfg`.
fn read_settings(s: &State, cfg: &mut Config) -> Result<(), String> {
    let c = &mut cfg.clipboard;
    c.enabled = s.get_clip_enabled();
    c.text = s.get_clip_text();
    c.images = s.get_clip_images();
    c.respect_private = s.get_clip_private();
    c.files = s.get_clip_files();
    c.files_mode = if s.get_files_mode() == 1 { FilesMode::Dialog } else { FilesMode::Auto };
    c.on_conflict = CONFLICTS[s.get_on_conflict().clamp(0, 3) as usize];
    c.max_size_mb = u32::try_from(number(&s.get_max_clip_mb(), "Largest text or image")?).map_err(|_| "Largest text or image: too large")?;
    c.max_file_mb = number(&s.get_max_file_mb(), "Largest file")?;
    c.max_total_mb = number(&s.get_max_total_mb(), "Largest copy")?;
    let k = &mut cfg.cursor;
    k.method = METHODS[s.get_hide_method().clamp(0, 2) as usize];
    k.park = PARKS[s.get_park().clamp(0, 4) as usize];
    k.hide_while_directing = s.get_hide_directing();
    k.hide_after_released = s.get_hide_released();
    k.reveal_on_mouse = s.get_reveal_mouse();
    k.reveal_on_keyboard = s.get_reveal_keyboard();
    k.switch_at_corners = s.get_switch_at_corners();
    k.corner_size = number(&s.get_corner_size(), "Corner size")? as u32;
    k.switch_in_fullscreen = s.get_switch_in_fullscreen();
    let a = &mut cfg.local_activity;
    a.min_mouse_move = number(&s.get_min_move(), "Minimum mouse move")? as u32;
    a.window_ms = number(&s.get_window_ms(), "Within")? as u32;
    cfg.log.file = s.get_log_file();
    cfg.role = ROLES[s.get_role().clamp(0, 2) as usize];
    cfg.run_as_admin = s.get_run_as_admin();
    cfg.security.enabled = s.get_sec_enabled();
    cfg.updates.auto_check = s.get_auto_update();
    Ok(())
}

fn show_settings(s: &State, cfg: &Config, paths: &Paths) {
    s.set_pc_name(cfg.name.clone().into());
    s.set_pc_port(cfg.port.to_string().into());
    s.set_pc_fingerprint(own_fingerprint(paths).into());
    s.set_config_path(paths.config.display().to_string().into());
    let c = &cfg.clipboard;
    s.set_clip_enabled(c.enabled);
    s.set_clip_text(c.text);
    s.set_clip_images(c.images);
    s.set_clip_private(c.respect_private);
    s.set_clip_files(c.files);
    s.set_files_mode(if c.files_mode == FilesMode::Dialog { 1 } else { 0 });
    s.set_on_conflict(index_of(&CONFLICTS, &c.on_conflict));
    s.set_max_clip_mb(c.max_size_mb.to_string().into());
    s.set_max_file_mb(c.max_file_mb.to_string().into());
    s.set_max_total_mb(c.max_total_mb.to_string().into());
    let k = &cfg.cursor;
    s.set_hide_method(index_of(&METHODS, &k.method));
    s.set_park(index_of(&PARKS, &k.park));
    s.set_hide_directing(k.hide_while_directing);
    s.set_hide_released(k.hide_after_released);
    s.set_reveal_mouse(k.reveal_on_mouse);
    s.set_reveal_keyboard(k.reveal_on_keyboard);
    s.set_switch_at_corners(k.switch_at_corners);
    s.set_corner_size(k.corner_size.to_string().into());
    s.set_switch_in_fullscreen(k.switch_in_fullscreen);
    s.set_min_move(cfg.local_activity.min_mouse_move.to_string().into());
    s.set_window_ms(cfg.local_activity.window_ms.to_string().into());
    s.set_hk_return(keycaps(&cfg.hotkeys.return_to_local));
    s.set_hk_paste(keycaps(&cfg.hotkeys.paste_files));
    s.set_log_file(cfg.log.file);
    s.set_role(index_of(&ROLES, &cfg.role));
    s.set_run_as_admin(cfg.run_as_admin);
    s.set_sec_enabled(cfg.security.enabled);
    s.set_auto_update(cfg.updates.auto_check);
}

/// The Updates card, from the running DeskLink's update status.
fn show_update(s: &State, status: &crate::update::Status) {
    use crate::update::{Status, mb};
    let (kind, text) = match status {
        Status::Idle => (0, "Up to date as far as DeskLink knows.".to_string()),
        Status::Checking => (1, "Checking GitHub for a new release…".to_string()),
        Status::UpToDate(tag) => (0, format!("Up to date. The latest release is {tag}.")),
        Status::Downloading { tag, done, total } => (
            1,
            if *total > 0 { format!("Downloading {tag}: {} of {}…", mb(*done), mb(*total)) } else { format!("Downloading {tag}: {}…", mb(*done)) },
        ),
        Status::Ready(tag) => (2, format!("{tag} is downloaded. It installs when DeskLink restarts.")),
        Status::Failed(e) => (3, format!("Couldn't check for updates: {e}.")),
        Status::NotRunning => (4, "DeskLink isn't running: it checks for updates while it runs.".to_string()),
    };
    s.set_update_kind(kind);
    s.set_update_text(text.into());
}

/// Follow the running DeskLink's update status through its pipe; reconnects
/// when it (re)starts.
fn follow_update(app: slint::Weak<App>) {
    use crate::update::Status;
    use std::io::{BufRead, BufReader};
    let deliver = move |status: Status| {
        let app = app.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = app.upgrade() {
                show_update(&app.global::<State>(), &status);
            }
        });
    };
    let _ = std::thread::Builder::new().name("dl-update-ui".into()).spawn(move || {
        let mut said_not_running = false;
        loop {
            let Ok(pipe) = desklink_win::pipe::connect("update") else {
                if !said_not_running {
                    deliver(Status::NotRunning);
                    said_not_running = true;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            };
            said_not_running = false;
            for line in BufReader::new(pipe).lines() {
                let Ok(line) = line else { break };
                deliver(Status::parse(&line));
            }
        }
    });
}

/// The Security tab: the lists, this PC's network, and which of the PCs in
/// Devices the rules would turn away.
fn show_security(s: &State, cfg: &Config) {
    use desklink_net::access::{Access, Subnet};
    let sec = &cfg.security;
    let strings = |v: &[String]| ModelRc::new(VecModel::from(v.iter().map(SharedString::from).collect::<Vec<_>>()));
    s.set_sec_networks(strings(&sec.networks));
    s.set_sec_allowed(strings(&sec.allowed));
    let suggested = local_network().filter(|n| !sec.networks.iter().any(|m| m == n)).unwrap_or_default();
    s.set_sec_suggested(suggested.into());
    let access = Access { networks: sec.networks.iter().filter_map(|n| n.parse::<Subnet>().ok()).collect(), allowed: sec.allowed.clone() };
    let missing: Vec<String> = cfg
        .peers
        .iter()
        .filter(|p| {
            let ip = [p.last_ip.as_deref(), p.address.as_deref()].into_iter().flatten().find_map(|a| {
                a.parse::<std::net::SocketAddr>().map(|a| a.ip()).or_else(|_| a.parse::<std::net::IpAddr>()).ok()
            });
            match ip {
                Some(ip) => access.check(&p.name, ip).is_err(),
                // Address unknown yet: only the name can tell.
                None => !sec.allowed.is_empty() && !sec.allowed.iter().any(|a| a.eq_ignore_ascii_case(&p.name)),
            }
        })
        .map(|p| p.name.clone())
        .collect();
    s.set_sec_missing(strings(&missing));
}

/// This PC's IPv4 network, assuming a home network's /24 (the route to the
/// internet tells which address is ours; nothing is sent).
fn local_network() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:53").ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_unspecified() && !ip.is_loopback() => {
            let [a, b, c, _] = ip.octets();
            Some(format!("{a}.{b}.{c}.0/24"))
        }
        _ => None,
    }
}

fn show_devices(s: &State, cfg: &Config, paths: &Paths) {
    let mut list = vec![Device {
        name: cfg.name.clone().into(),
        address: format!("port {}", cfg.port).into(),
        side: "".into(),
        fingerprint: own_fingerprint(paths).into(),
        local: true,
    }];
    for p in &cfg.peers {
        let address = p.address.clone().or_else(|| p.last_ip.clone()).unwrap_or_else(|| "found by name".into());
        let fingerprint = p.fingerprint.as_deref().map(short_fingerprint).unwrap_or_else(|| "not paired yet".into());
        list.push(Device {
            name: p.name.clone().into(),
            address: address.into(),
            // Sides are a layout detail now; say whether it is placed.
            side: (if p.unplaced { "Not on the layout" } else { "On the layout" }).into(),
            fingerprint: fingerprint.into(),
            local: false,
        });
    }
    s.set_devices(ModelRc::new(VecModel::from(list)));
    show_security(s, cfg);
}

/// The window title; also how a second `desklink settings` finds the window.
const TITLE: &str = "DeskLink";

/// `page`: the tab to open on (layout, devices, settings, security, log).
pub fn run(paths: &Paths, page: Option<&str>) -> Result<(), String> {
    let Some(_one_window) = desklink_win::instance::settings_lock(TITLE) else {
        return Ok(()); // already open: brought to the front
    };
    let page = match page {
        Some("devices") => 1,
        Some("settings") => 2,
        Some("security") => 3,
        Some("log") => 4,
        _ => 0,
    };
    show(paths.clone(), page).map_err(|e| format!("settings window: {e}"))
}

/// What the log follower tells the window.
enum LogNews {
    Connected,
    Lines(Vec<String>),
    NotRunning,
}

fn log_level(line: &str) -> i32 {
    // `[2026-10-30T17:39:35.539Z WARN  target] message`
    match line.split_whitespace().nth(1) {
        Some("ERROR") => 2,
        Some("WARN") => 1,
        Some("DEBUG" | "TRACE") => 3,
        _ => 0,
    }
}

/// Apply news from the log follower to the window (UI thread).
fn show_log_news(app: &App, news: LogNews) {
    let s = app.global::<State>();
    let lines = s.get_log_lines();
    let Some(model) = lines.as_any().downcast_ref::<VecModel<LogLine>>() else { return };
    match news {
        LogNews::Connected => {
            model.set_vec(Vec::new()); // the backlog is sent again
            s.set_log_status(format!("The running DeskLink's last {} lines, live.", crate::logging::MEMORY_LINES).into());
        }
        LogNews::NotRunning => s.set_log_status("DeskLink isn't running.".into()),
        LogNews::Lines(new) => {
            let new = new.into_iter().map(|l| LogLine { level: log_level(&l), text: l.into() });
            if model.row_count() + new.len() > crate::logging::MEMORY_LINES {
                let mut all: Vec<LogLine> = model.iter().chain(new).collect();
                let extra = all.len().saturating_sub(crate::logging::MEMORY_LINES);
                all.drain(..extra);
                model.set_vec(all);
            } else {
                model.extend(new);
            }
            s.set_log_version(s.get_log_version().wrapping_add(1));
        }
    }
}

/// Follow the running DeskLink's log through its pipe; reconnects when it
/// (re)starts. Lines are handed to the window in batches.
fn follow_log(app: slint::Weak<App>) {
    use std::io::{BufRead, BufReader};
    use std::sync::{Arc, Mutex};
    let pending: Arc<Mutex<Vec<String>>> = Arc::default();
    let deliver = move |news: LogNews| {
        let app = app.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = app.upgrade() {
                show_log_news(&app, news);
            }
        });
    };
    let _ = std::thread::Builder::new().name("dl-log".into()).spawn(move || {
        let mut said_not_running = false;
        loop {
            let Ok(pipe) = desklink_win::pipe::connect("log") else {
                if !said_not_running {
                    deliver(LogNews::NotRunning);
                    said_not_running = true;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            };
            said_not_running = false;
            deliver(LogNews::Connected);
            for line in BufReader::new(pipe).lines() {
                let Ok(line) = line else { break };
                let mut p = pending.lock().unwrap();
                p.push(line);
                if p.len() == 1 {
                    // First of a batch: flush once the UI gets to it.
                    let pending = pending.clone();
                    let deliver = deliver.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(30));
                        let batch = std::mem::take(&mut *pending.lock().unwrap());
                        deliver(LogNews::Lines(batch));
                    });
                }
            }
        }
    });
}

fn select_backend() -> Result<(), slint::PlatformError> {
    // Windows draws a light title bar unless the window asks for the dark theme.
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("femtovg".into())
        .with_winit_window_attributes_hook(|a| a.with_theme(Some(slint::winit_030::winit::window::Theme::Dark)))
        .select()
}

/// The update window's text for a status: kind (see `UpdateWindow`),
/// headline, detail, progress (below 0: waiting).
fn update_view(status: &crate::update::Status) -> (i32, String, String, f32) {
    use crate::update::{Status, mb};
    match status {
        Status::Idle | Status::Checking => (1, "Checking for updates".into(), "Asking GitHub for the latest release…".into(), -1.0),
        Status::Downloading { tag, done, total } => (
            1,
            format!("Downloading {tag}"),
            if *total > 0 { format!("{} of {}", mb(*done), mb(*total)) } else { mb(*done) },
            if *total > 0 { *done as f32 / *total as f32 } else { -1.0 },
        ),
        Status::UpToDate(tag) => (0, "DeskLink is up to date".into(), format!("The latest release is {tag}."), 0.0),
        Status::Ready(tag) => (
            2,
            format!("DeskLink {tag} is ready to install"),
            "Restart now: DeskLink exits, installs it and starts again (a few seconds without sharing). Later: it installs when you exit DeskLink from the tray.".into(),
            0.0,
        ),
        Status::Failed(e) => {
            let mut c = e.chars();
            let e: String = c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default();
            (3, "Couldn't check for updates".into(), format!("{e}."), 0.0)
        }
        Status::NotRunning => (4, "DeskLink isn't running".into(), "Start DeskLink first: it checks for updates while it runs.".into(), 0.0),
    }
}

fn show_update_view(w: &UpdateWindow, status: &crate::update::Status) {
    let (kind, headline, detail, progress) = update_view(status);
    w.set_kind(kind);
    w.set_headline(headline.into());
    w.set_detail(detail.into());
    w.set_progress(progress);
}

/// The update window (`desklink update`, from the tray's "Check for
/// updates"): asks the running DeskLink to check, follows the check and the
/// download through its "update" pipe, and passes "Restart now" on. Closes
/// when DeskLink exits (it must not hold the exe while it's replaced).
pub fn run_update() -> Result<(), String> {
    use crate::update::Status;
    const TITLE: &str = "DeskLink updates";
    let Some(_one_window) = desklink_win::instance::update_window_lock(TITLE) else {
        return Ok(()); // already open: brought to the front
    };
    let err = |e: slint::PlatformError| format!("update window: {e}");
    select_backend().map_err(err)?;
    let w = UpdateWindow::new().map_err(err)?;
    w.set_version(crate::update::VERSION.into());

    let weak = w.as_weak();
    let deliver = move |status: Status| {
        let weak = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                show_update_view(&w, &status);
            }
        });
    };
    let _ = std::thread::Builder::new().name("dl-update-window".into()).spawn(move || {
        use std::io::{BufRead, BufReader};
        // DeskLink may be starting: give it a moment before saying it isn't running.
        let mut pipe = None;
        for _ in 0..10 {
            if let Ok(p) = desklink_win::pipe::connect("update") {
                pipe = Some(p);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        let Some(pipe) = pipe else {
            deliver(Status::NotRunning);
            return;
        };
        let mut first = true;
        for line in BufReader::new(pipe).lines() {
            let Ok(line) = line else { break };
            let status = Status::parse(&line);
            if first {
                first = false;
                // Opening the window means "check now", unless a check runs
                // or an update is already waiting.
                if !matches!(status, Status::Checking | Status::Downloading { .. } | Status::Ready(_)) {
                    desklink_win::instance::request_update_check();
                    deliver(Status::Checking);
                    continue;
                }
            }
            deliver(status);
        }
        // DeskLink exited (or restarts to update): go with it.
        let _ = slint::invoke_from_event_loop(|| {
            let _ = slint::quit_event_loop();
        });
    });

    let weak = w.as_weak();
    w.on_check_again(move || {
        let Some(w) = weak.upgrade() else { return };
        if desklink_win::instance::request_update_check() {
            show_update_view(&w, &Status::Checking);
        } else {
            show_update_view(&w, &Status::NotRunning);
        }
    });
    w.on_restart(move || {
        desklink_win::instance::request_update_apply();
        let _ = slint::quit_event_loop();
    });
    w.on_dismiss(move || {
        let _ = slint::quit_event_loop();
    });
    w.run().map_err(err)
}

fn show(paths: Paths, page: i32) -> Result<(), slint::PlatformError> {
    select_backend()?;

    let app = App::new()?;
    app.set_page(page);
    let state = app.global::<State>();

    let cfg = match Config::load_or_default(&paths.config) {
        Ok(c) => c,
        Err(e) => {
            status(&state, &format!("Can't read the config: {e}"), true);
            Config::defaults()
        }
    };
    if !state.get_status_error() {
        state.set_status(format!("Settings of {}", cfg.name).into());
    }
    show_settings(&state, &cfg, &paths);
    show_devices(&state, &cfg, &paths);
    state.set_version(crate::update::VERSION.into());
    show_update(&state, &crate::update::Status::NotRunning);
    follow_update(app.as_weak());
    {
        let weak = app.as_weak();
        state.on_update_check(move || {
            let Some(app) = weak.upgrade() else { return };
            if !desklink_win::instance::request_update_check() {
                show_update(&app.global::<State>(), &crate::update::Status::NotRunning);
            }
        });
        let weak = app.as_weak();
        state.on_update_apply(move || {
            let Some(app) = weak.upgrade() else { return };
            if !desklink_win::instance::request_update_apply() {
                status(&app.global::<State>(), "DeskLink isn't running, so it can't install the update.", true);
            }
        });
    }

    let monitors = Rc::new(VecModel::<Mon>::default());
    state.set_monitors(ModelRc::from(monitors.clone()));
    let slots = Rc::new(VecModel::<Slot>::default());
    state.set_slots(ModelRc::from(slots.clone()));
    let local = desklink_win::display::monitors();
    let gui = Rc::new(RefCell::new(Gui {
        paths,
        desk: Desk::new(&local, Vec::new()),
        local,
        view: View::default(),
        monitors,
        slots,
        drag: None,
        pushed: None,
        known_master: None,
        role: Role::Peer,
    }));
    gui.borrow_mut().reset_layout(&state, &cfg);

    APP.set(Some(app.as_weak()));
    // Tooltips appear after hovering a moment, for the control still hovered.
    let weak = app.as_weak();
    state.on_tip_hover(move |text, x, y, w, h| {
        let n = HOVER.with(|c| {
            c.set(c.get().wrapping_add(1));
            c.get()
        });
        let weak = weak.clone();
        slint::Timer::single_shot(std::time::Duration::from_millis(450), move || {
            if HOVER.with(|c| c.get()) == n
                && let Some(app) = weak.upgrade()
            {
                let s = app.global::<State>();
                s.set_tip_text(text.clone());
                s.set_tip_x(x);
                s.set_tip_y(y);
                s.set_tip_w(w);
                s.set_tip_h(h);
                s.set_tip_shown(true);
            }
        });
    });
    let weak = app.as_weak();
    state.on_tip_leave(move || {
        HOVER.with(|c| c.set(c.get().wrapping_add(1)));
        if let Some(app) = weak.upgrade() {
            app.global::<State>().set_tip_shown(false);
        }
    });

    // Start with Windows lives in the registry, not in desklink.cfg.
    state.set_start_with_windows(desklink_win::autostart::is_enabled());
    let weak = app.as_weak();
    state.on_start_with_windows_changed(move |on| {
        let Some(app) = weak.upgrade() else { return };
        let s = app.global::<State>();
        match desklink_win::autostart::set(on, s.get_run_as_admin()) {
            Ok(()) if on => status(&s, "DeskLink will start when you sign in to Windows.", false),
            Ok(()) => status(&s, "DeskLink won't start with Windows.", false),
            Err(e) => {
                s.set_start_with_windows(desklink_win::autostart::is_enabled());
                status(&s, &e, true);
            }
        }
    });
    // Run as administrator: saved like other settings; DeskLink restarts
    // itself elevated. Start with Windows switches to the matching method.
    show_elevation(&state);
    let weak = app.as_weak();
    state.on_run_as_admin_changed(move |on| {
        let Some(app) = weak.upgrade() else { return };
        let s = app.global::<State>();
        s.invoke_changed();
        if desklink_win::autostart::is_enabled()
            && let Err(e) = desklink_win::autostart::set(true, on)
        {
            status(&s, &format!("Start with Windows not updated: {e}"), true);
            return;
        }
        if on && !desklink_win::elevation::is_elevated() {
            status(&s, "DeskLink restarts with administrator rights (Windows asks for permission).", false);
        } else if !on {
            status(&s, "DeskLink runs without administrator rights from its next start.", false);
        }
    });
    // Typing in a text box: save once typing pauses (Enter saves at once).
    let weak = app.as_weak();
    state.on_typed(move || {
        let n = TYPED.with(|t| {
            t.set(t.get().wrapping_add(1));
            t.get()
        });
        let weak = weak.clone();
        slint::Timer::single_shot(std::time::Duration::from_millis(700), move || {
            if TYPED.with(|t| t.get()) == n
                && let Some(app) = weak.upgrade()
            {
                app.global::<State>().invoke_changed();
            }
        });
    });

    let weak = app.as_weak();
    let g = gui.clone();
    state.on_changed(move || {
        let Some(app) = weak.upgrade() else { return };
        let s = app.global::<State>();
        let saved = g.borrow().update(&s, |cfg| read_settings(&s, cfg));
        // The role changed: the Layout tab follows (otherwise unapplied
        // layout changes stay).
        if let Some(cfg) = saved.filter(|c| c.role != g.borrow().role) {
            g.borrow_mut().reset_layout(&s, &cfg);
        }
    });

    let weak = app.as_weak();
    let g = gui.clone();
    state.on_add_peer(move || {
        let Some(app) = weak.upgrade() else { return };
        let s = app.global::<State>();
        let name = s.get_new_name().trim().to_lowercase();
        let address = s.get_new_address().trim().to_string();
        let saved = g.borrow().update(&s, |cfg| {
            if name.is_empty() {
                return Err("Give the PC a name".into());
            }
            if cfg.peers.iter().any(|p| p.name == name) {
                return Err(format!("{name} is already in the list"));
            }
            // In the list only; it goes on the layout by dragging it there.
            cfg.peers.push(Peer { address: (!address.is_empty()).then_some(address.clone()), ..Peer::unplaced(name.clone()) });
            Ok(())
        });
        if let Some(cfg) = saved {
            s.set_new_name("".into());
            s.set_new_address("".into());
            let mut gm = g.borrow_mut();
            show_devices(&s, &cfg, &gm.paths);
            gm.reset_layout(&s, &cfg);
        }
    });

    let weak = app.as_weak();
    let g = gui.clone();
    state.on_remove_peer(move |i| {
        let Some(app) = weak.upgrade() else { return };
        let s = app.global::<State>();
        let i = i as usize;
        let saved = g.borrow().update(&s, |cfg| {
            if i >= cfg.peers.len() {
                return Err("That PC is no longer in the config".into());
            }
            cfg.peers.remove(i);
            Ok(())
        });
        if let Some(cfg) = saved {
            let mut gm = g.borrow_mut();
            show_devices(&s, &cfg, &gm.paths);
            gm.reset_layout(&s, &cfg);
        }
    });

    // Security lists (`networks` or allowed PCs); entries are checked first.
    fn edit_list(g: &Rc<RefCell<Gui>>, s: &State, networks: bool, edit: impl FnOnce(&mut Vec<String>) -> Result<(), String>) -> bool {
        let saved = g.borrow().update(s, |cfg| edit(if networks { &mut cfg.security.networks } else { &mut cfg.security.allowed }));
        if let Some(cfg) = &saved {
            show_security(s, cfg);
        }
        saved.is_some()
    }
    let weak = app.as_weak();
    let g = gui.clone();
    state.on_sec_add_network(move |text| {
        let Some(app) = weak.upgrade() else { return };
        let s = app.global::<State>();
        let entry = text.trim().to_string();
        let added = edit_list(&g, &s, true, |list| {
            entry.parse::<desklink_net::access::Subnet>()?;
            if list.iter().any(|n| *n == entry) {
                return Err(format!("{entry} is already allowed"));
            }
            list.push(entry.clone());
            Ok(())
        });
        if added {
            s.set_sec_new_network("".into());
        }
    });
    let weak = app.as_weak();
    let g = gui.clone();
    state.on_sec_add_pc(move |text| {
        let Some(app) = weak.upgrade() else { return };
        let s = app.global::<State>();
        let entry = text.trim().to_lowercase();
        let added = edit_list(&g, &s, false, |list| {
            if entry.is_empty() || entry.contains(char::is_whitespace) {
                return Err("A PC is a computer name or an IP address, without spaces".into());
            }
            if list.iter().any(|p| p.eq_ignore_ascii_case(&entry)) {
                return Err(format!("{entry} is already allowed"));
            }
            list.push(entry.clone());
            Ok(())
        });
        if added {
            s.set_sec_new_pc("".into());
        }
    });
    for networks in [true, false] {
        let weak = app.as_weak();
        let g = gui.clone();
        let remove = move |i: i32| {
            let Some(app) = weak.upgrade() else { return };
            let s = app.global::<State>();
            edit_list(&g, &s, networks, |list| {
                if i as usize >= list.len() {
                    return Err("That entry is no longer in the config".into());
                }
                list.remove(i as usize);
                Ok(())
            });
        };
        if networks {
            state.on_sec_remove_network(remove);
        } else {
            state.on_sec_remove_pc(remove);
        }
    }

    let g = gui.clone();
    state.on_select(move |i| {
        let mut gm = g.borrow_mut();
        let owner = gm.monitors.row_data(i as usize).map(|r| r.owner).unwrap_or_default();
        for r in 0..gm.monitors.row_count() {
            let mut d = gm.monitors.row_data(r).unwrap();
            d.selected = !d.local && d.owner == owner;
            gm.monitors.set_row_data(r, d);
        }
        gm.drag = gm.desk.peers.iter().position(|p| p.name.as_str() == owner.as_str()).map(|p| (p, 0.0, 0.0));
    });

    let g = gui.clone();
    state.on_drag(move |i, dx, dy| {
        let mut gm = g.borrow_mut();
        let Some(row) = gm.monitors.row_data(i as usize) else { return };
        if row.local {
            return; // this PC is the reference; move the others around it
        }
        for r in 0..gm.monitors.row_count() {
            let mut d = gm.monitors.row_data(r).unwrap();
            if d.owner == row.owner {
                d.x += dx;
                d.y += dy;
                gm.monitors.set_row_data(r, d);
            }
        }
        if let Some(d) = gm.drag.as_mut() {
            d.1 += dx;
            d.2 += dy;
        }
    });

    let weak = app.as_weak();
    let g = gui.clone();
    state.on_drop(move |_| {
        let Some(app) = weak.upgrade() else { return };
        let s = app.global::<State>();
        let mut gm = g.borrow_mut();
        let Some((peer, dx, dy)) = gm.drag.take() else { return };
        gm.refresh_monitors();
        if dx.abs() + dy.abs() < 3.0 {
            return; // a click, not a move
        }
        // Back to desk pixels; it attaches to the nearest free edge.
        let scale = gm.view.scale.max(0.0001);
        let (x, y) = gm.desk.offset(peer);
        let at = (x + (dx / scale).round() as i32, y + (dy / scale).round() as i32);
        if !gm.desk.place(peer, at) {
            status(&s, "No free spot next to another screen there.", true);
        }
        match Config::load_or_default(&gm.paths.config) {
            Ok(cfg) => gm.show_layout(&s, &cfg),
            Err(e) => status(&s, &e, true),
        }
        // Keep the dropped PC highlighted.
        let name = gm.desk.peers[peer].name.clone();
        for r in 0..gm.monitors.row_count() {
            let mut d = gm.monitors.row_data(r).unwrap();
            d.selected = d.owner.as_str() == name;
            gm.monitors.set_row_data(r, d);
        }
    });


    // A PC dragged from the list onto the canvas: it goes in the nearest free
    // spot (the one it was dropped on).
    let weak = app.as_weak();
    let g = gui.clone();
    state.on_drop_new(move |name, x, y| {
        let Some(app) = weak.upgrade() else { return };
        let s = app.global::<State>();
        let mut gm = g.borrow_mut();
        gm.refresh_monitors();
        let Some(peer) = gm.desk.peers.iter().position(|p| p.name.as_str() == name.as_str()) else { return };
        let v = gm.view;
        let centre = (((x - v.ox) / v.scale).round() as i32, ((y - v.oy) / v.scale).round() as i32);
        if !gm.desk.drop_at(peer, centre) {
            status(&s, "No free spot next to another screen there.", true);
        }
        match Config::load_or_default(&gm.paths.config) {
            Ok(cfg) => gm.show_layout(&s, &cfg),
            Err(e) => status(&s, &e, true),
        }
    });
    let weak = app.as_weak();
    let g = gui.clone();
    state.on_apply_layout(move || {
        let Some(app) = weak.upgrade() else { return };
        let s = app.global::<State>();
        let mut gm = g.borrow_mut();
        let placed = gm.desk.peers.clone();
        let local = gm.local.clone();
        if let Some(cfg) = gm.update(&s, |cfg| {
            for p in &mut cfg.peers {
                if let Some(q) = placed.iter().find(|q| q.name == p.name) {
                    p.side = q.side;
                    p.position = q.position.map(|(x, y)| [x, y]);
                    p.unplaced = q.unplaced;
                }
            }
            // Positions placed with an outdated size can cover our screens.
            cfg.fix_overlaps(&local);
            Ok(())
        }) {
            show_devices(&s, &cfg, &gm.paths);
            gm.reset_layout(&s, &cfg);
        }
    });

    let weak = app.as_weak();
    let g = gui.clone();
    state.on_revert_layout(move || {
        let Some(app) = weak.upgrade() else { return };
        let s = app.global::<State>();
        let mut gm = g.borrow_mut();
        match Config::load_or_default(&gm.paths.config) {
            Ok(cfg) => gm.reset_layout(&s, &cfg),
            Err(e) => status(&s, &e, true),
        }
    });

    state.set_log_lines(ModelRc::new(VecModel::<LogLine>::default()));
    state.set_log_status("Connecting to DeskLink...".into());
    let weak = app.as_weak();
    state.on_log_clear(move || {
        if let Some(app) = weak.upgrade()
            && let Some(m) = app.global::<State>().get_log_lines().as_any().downcast_ref::<VecModel<LogLine>>()
        {
            m.set_vec(Vec::new());
        }
    });
    let weak = app.as_weak();
    state.on_log_copy(move || {
        let Some(app) = weak.upgrade() else { return };
        let s = app.global::<State>();
        let lines = s.get_log_lines();
        let text = lines.iter().map(|l| l.text.to_string()).collect::<Vec<_>>().join("\r\n");
        if desklink_win::ui::copy_text(&text) {
            status(&s, &format!("Copied {} log lines.", lines.row_count()), false);
        } else {
            status(&s, "Couldn't copy: the clipboard is busy.", true);
        }
    });
    follow_log(app.as_weak());
    // A client shows the layout its master pushed (in memory only). Followed
    // in every role: the role can change while the window is open.
    GUI.set(Some(gui.clone()));
    {
        let weak = app.as_weak();
        let _ = std::thread::Builder::new().name("dl-layout".into()).spawn(move || {
            use std::io::{BufRead, BufReader};
            loop {
                if let Ok(pipe) = desklink_win::pipe::connect("layout") {
                    for line in BufReader::new(pipe).lines() {
                        let Ok(line) = line else { break };
                        let weak = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            let Some(app) = weak.upgrade() else { return };
                            let s = app.global::<State>();
                            GUI.with_borrow(|g| {
                                if let Some(g) = g {
                                    let mut gm = g.borrow_mut();
                                    gm.pushed = Some(crate::pushed::decode(&line));
                                    if let Ok(cfg) = Config::load_or_default(&gm.paths.config) {
                                        gm.show_layout(&s, &cfg);
                                    }
                                }
                            });
                        });
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        });
    }
    // DeskLink exited (tray Exit, `desklink stop`, shutdown): close with it.
    match desklink_win::instance::listen_for_close_settings() {
        Ok(close) => {
            let _ = std::thread::Builder::new().name("dl-close".into()).spawn(move || {
                if close.recv().is_ok() {
                    let _ = slint::invoke_from_event_loop(|| {
                        let _ = slint::quit_event_loop();
                    });
                }
            });
        }
        Err(e) => log::warn!("{e}"),
    }

    // PCs announcing themselves on the network, not set up here yet.
    let found: Rc<RefCell<Vec<(desklink_net::discovery::Found, std::time::Instant)>>> = Rc::default();
    // Lives as long as the window (this function runs it).
    let found_refresh = slint::Timer::default();
    match desklink_net::discovery::listen(cfg.port) {
        Ok(heard) => {
            let weak = app.as_weak();
            let list = found.clone();
            let g = gui.clone();

            found_refresh.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(500), move || {
                let Some(app) = weak.upgrade() else { return };
                let mut list = list.borrow_mut();
                let now = std::time::Instant::now();
                for f in heard.try_iter() {
                    list.retain(|(old, _)| old.name != f.name);
                    list.push((f, now));
                }
                // Gone quiet for a few announcements: gone.
                list.retain(|(_, seen)| now.duration_since(*seen) < desklink_net::discovery::INTERVAL * 4);
                let Ok(cfg) = Config::load_or_default(&g.borrow().paths.config) else { return };
                let rows: Vec<FoundPc> = list
                    .iter()
                    .map(|(f, _)| f)
                    .filter(|f| !f.name.eq_ignore_ascii_case(&cfg.name) && !cfg.peers.iter().any(|p| p.name.eq_ignore_ascii_case(&f.name)))
                    .map(|f| {
                        // Listed even when our role can't connect to it, with why.
                        let mine = crate::proto_role(cfg.role);
                        let theirs = desklink_proto::Role::from_name(&f.role);
                        let compatible = theirs.is_some_and(|r| mine.connects_to(r));
                        let note = match theirs {
                            Some(r) if !compatible => mine.refusal(r).to_string(),
                            None => "an unknown role".to_string(),
                            _ => String::new(),
                        };
                        FoundPc {
                            name: f.name.clone().into(),
                            address: f.addr.to_string().into(),
                            role: f.role.clone().into(),
                            fingerprint: f.fingerprint.clone().into(),
                            compatible,
                            note: note.into(),
                        }
                    })
                    .collect();
                app.global::<State>().set_found(ModelRc::new(VecModel::from(rows)));
            });
        }
        Err(e) => log::warn!("{e}"),
    }
    // "Scan network": start the list afresh and listen for a moment (every
    // DeskLink announces itself each second).
    let weak = app.as_weak();
    let list = found.clone();
    state.on_scan(move || {
        list.borrow_mut().clear();
        let Some(app) = weak.upgrade() else { return };
        app.global::<State>().set_found(ModelRc::new(VecModel::<FoundPc>::default()));
        app.global::<State>().set_scanning(true);
        let weak = weak.clone();
        slint::Timer::single_shot(desklink_net::discovery::INTERVAL * 3, move || {
            if let Some(app) = weak.upgrade() {
                app.global::<State>().set_scanning(false);
            }
        });
    });
    let weak = app.as_weak();
    let g = gui.clone();
    state.on_add_found(move |name, address| {
        let Some(app) = weak.upgrade() else { return };
        let s = app.global::<State>();
        let name = name.to_lowercase();
        let saved = g.borrow().update(&s, |cfg| {
            if cfg.peers.iter().any(|p| p.name == name) {
                return Err(format!("{name} is already in the list"));
            }
            // In the list only; it goes on the layout by dragging it there.
            cfg.peers.push(Peer { address: Some(address.to_string()), ..Peer::unplaced(name.clone()) });
            Ok(())
        });
        if let Some(cfg) = saved {
            status(&s, &format!("Added {name}. Drag it onto the screen layout in the Layout tab."), false);
            let mut gm = g.borrow_mut();
            show_devices(&s, &cfg, &gm.paths);
            gm.reset_layout(&s, &cfg);
        }
    });

    // `--page N` opens on a tab (0 layout, 1 devices, 2 settings, 3 security, 4 log).
    if let Some(p) = std::env::args().skip_while(|a| a != "--page").nth(1).and_then(|p| p.parse().ok()) {
        app.set_page(p);
    }
    app.window().set_size(slint::LogicalSize::new(1200.0, 800.0));
    app.run()
}

/// Show whether this window (started by DeskLink) runs elevated.
fn show_elevation(s: &State) {
    s.set_is_elevated(desklink_win::elevation::is_elevated());
}
