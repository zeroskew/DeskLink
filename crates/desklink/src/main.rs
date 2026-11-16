//! DeskLink: a tray app sharing one keyboard and mouse between computers.
//!
//!   desklink                              run (in the tray; if it already
//!                                         runs, open the settings window)
//!   desklink add-peer <name> <side> [address]
//!   desklink info                         name, fingerprint, monitors, config path
//!   desklink stop                         ask the running DeskLink to exit cleanly
//!   desklink settings                     open the settings window
//!   desklink update                       check for updates (in a window)
//!   desklink --version                    the build version (release tag
//!                                         without the "v")
//!
//! Options: --config <path>, --verbose

// No console window; commands run from a terminal print there anyway.
#![windows_subsystem = "windows"]

use desklink_config as config;
mod gui;
mod hotkey;
mod layout;
mod files_ui;
mod logging;
mod pushed;
mod update;

use config::{Config, Paths, Role, Side};
use crossbeam_channel::{select, tick};
use desklink_core::switch::{Core, Focus, HideReason, Io, Local, PeerId, PeerSlot, Watch};
use desklink_net::identity::{self, Identity};
use desklink_net::{Net, NetConfig, NetEvent, PeerConfig};
use desklink_proto::{Command, FileOffer, GoodbyeReason, InputEvent, LockKeys, Message, Os};
use desklink_win::capture::{Capture, CaptureEvent, Hotkey};
use desklink_win::clipboard::{ClipEvent, ClipboardOptions, ClipboardWorker};
use desklink_win::session::{SessionEvent, SessionWatcher};
use desklink_win::tray::{Tray, TrayEvent, TrayState};
use desklink_win::dragdrop::{self, DragCatcher};
use desklink_win::{cursor, display, inject, ui};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Where `desklink-crash.log` goes (for the crash handler, a plain function).
static CRASH_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

const HK_RETURN: u32 = 1;
const HK_PASTE_FILES: u32 = 2;

fn main() {
    desklink_win::instance::attach_parent_console();
    desklink_win::instance::end_startup_cursor();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // Helper modes that must not touch the config or the log.
    match args.first().map(String::as_str) {
        Some("cursor-guard") => {
            if let Some(pid) = args.get(1).and_then(|p| p.parse().ok()) {
                desklink_win::instance::wait_for_process(pid);
                cursor::repair();
            }
            return;
        }
        Some("--version" | "version") => {
            println!("{}", update::VERSION);
            return;
        }
        Some("stop") => {
            if desklink_win::instance::request_stop(Duration::from_secs(5)) {
                println!("DeskLink stopped.");
            } else {
                println!("DeskLink is not running.");
            }
            return;
        }
        _ => {}
    }
    let verbose = take_flag(&mut args, "--verbose");
    // Started by a copy handing over (e.g. to run as administrator): let it
    // finish exiting first.
    if let Some(pid) = take_opt(&mut args, "--after").and_then(|p| p.parse().ok()) {
        desklink_win::instance::wait_for_process(pid);
    }
    let paths = Paths::resolve(take_opt(&mut args, "--config").map(PathBuf::from));
    // `settings --page layout|devices|settings|security|log`: open on that tab.
    let page = take_opt(&mut args, "--page");
    let migrated = paths.migrate_legacy();
    let command = args.first().map(String::as_str);
    let starting = matches!(command, None | Some("run"));
    if command == Some("update") {
        // "Check for updates" from the tray: its own window and process.
        if let Err(e) = gui::run_update() {
            eprintln!("{e}");
            ui::show_error("DeskLink", &e);
            std::process::exit(1);
        }
        return;
    }
    if command == Some("settings") || (starting && desklink_win::instance::is_running()) {
        // Its own process, without the log: the running DeskLink owns that.
        // Starting DeskLink again just opens the window.
        if let Err(e) = gui::run(&paths, page.as_deref()) {
            eprintln!("{e}");
            ui::show_error("DeskLink", &e);
            std::process::exit(1);
        }
        return;
    }

    // The log settings live in the config, so read it before the logger is up
    // (errors are reported right after).
    let early = Config::load_or_default(&paths.config);
    let log_settings = early.as_ref().map(|c| c.log.clone()).unwrap_or_default();
    match logging::init(&log_settings, verbose, &paths.dir) {
        Ok(Some(file)) => log::info!("logging to {}", file.display()),
        Ok(None) => {}
        Err(e) => log::warn!("{e}"),
    }
    for m in migrated {
        log::info!("{m}");
    }
    desklink_win::init_dpi_awareness();

    let result = early.and_then(|cfg| match args.first().map(String::as_str) {
        None | Some("run") => {
            // Started from a terminal that kills what it started when it
            // closes: start again outside it (the tray app must outlive it).
            if args.is_empty() && desklink_win::instance::escape_job() {
                log::info!("starting again outside this terminal (it would end DeskLink when closed)");
                return Ok(());
            }
            // Asked to run as administrator: hand over to an elevated copy.
            if cfg.run_as_admin && !desklink_win::elevation::is_elevated() && relaunch_elevated(&paths) {
                return Ok(());
            }
            // The tray app outlives the terminal it was started from
            // (`desklink run` stays attached, to watch its log there).
            if args.is_empty() {
                desklink_win::instance::detach_console();
            }
            start(&paths, cfg)
        }
        Some("add-peer") => add_peer(&paths, cfg, &args[1..]),
        Some("info") => info(&paths, cfg),
        Some(other) => Err(format!("unknown command '{other}'")),
    });
    if let Err(e) = result {
        log::error!("{e}");
        if starting {
            logging::crash_report(&paths.dir, &format!("error: {e}"));
            // Nobody may be looking at a console.
            ui::show_error("DeskLink", &e);
        }
        std::process::exit(1);
    }
}


/// Start DeskLink again with administrator rights (Windows asks). True if it
/// started; this copy should then exit.
fn relaunch_elevated(paths: &Paths) -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let args = vec![
        "run".to_string(),
        "--config".into(),
        paths.config.display().to_string(),
        "--after".into(),
        std::process::id().to_string(),
    ];
    match desklink_win::elevation::run_elevated(&exe.display().to_string(), &args, false) {
        Ok(Some(_)) => {
            log::info!("starting again with administrator rights");
            true
        }
        Ok(None) => {
            log::warn!("administrator rights were declined; running without them");
            false
        }
        Err(e) => {
            log::warn!("can't start with administrator rights: {e}");
            false
        }
    }
}
/// Open the settings window or the update window (`command`: "settings" or
/// "update"). Each runs as its own process, so a graphics-driver problem can
/// never take input sharing down; a second one just brings the open window to
/// the front.
fn open_window(paths: &Paths, command: &str) {
    let Ok(exe) = std::env::current_exe() else { return };
    desklink_win::instance::allow_foreground_handoff();
    let spawned = std::process::Command::new(exe)
        .arg(command)
        .arg("--config")
        .arg(&paths.config)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Err(e) = spawned {
        log::error!("cannot open the {command} window: {e}");
    }
}

fn take_flag(args: &mut Vec<String>, flag: &str) -> bool {
    let n = args.len();
    args.retain(|a| a != flag);
    args.len() != n
}

fn take_opt(args: &mut Vec<String>, opt: &str) -> Option<String> {
    let i = args.iter().position(|a| a == opt)?;
    args.remove(i);
    (i < args.len()).then(|| args.remove(i))
}

fn add_peer(paths: &Paths, mut cfg: Config, args: &[String]) -> Result<(), String> {
    let path = &paths.config;
    let [name, side, rest @ ..] = args else {
        return Err("usage: desklink add-peer <name> <left|right|top|bottom> [address]".into());
    };
    let side: Side = side.parse()?;
    let address = rest.first().cloned();
    match cfg.peers.iter_mut().find(|p| p.name.eq_ignore_ascii_case(name)) {
        Some(p) => {
            p.side = side;
            p.address = address.or(p.address.take());
        }
        None => cfg.peers.push(config::Peer { address, ..config::Peer::new(name.to_lowercase(), side) }),
    }
    cfg.save(path)?;
    println!("'{name}' is {side:?} of this machine ('{}'). Saved {}", cfg.name, path.display());
    Ok(())
}

fn info(paths: &Paths, cfg: Config) -> Result<(), String> {
    let id = Identity::load_or_create_at(&paths.cert, &paths.key)?;
    println!("name:        {}", cfg.name);
    println!("fingerprint: {}", identity::short(&id.fingerprint()));
    println!("config:      {}", paths.config.display());
    println!(
        "log file:    {}",
        if cfg.log.file { "on" } else { "off ([log] file = true to enable)" }
    );
    println!("port:        UDP {}", cfg.port);
    for m in display::monitors() {
        println!(
            "monitor:     {} {}x{} at ({},{}) {}%{}",
            m.name, m.width, m.height, m.x, m.y, m.scale_percent,
            if m.primary { " primary" } else { "" }
        );
    }
    for p in &cfg.peers {
        println!(
            "peer:        {} ({:?}) at {} pinned {}",
            p.name,
            p.side,
            p.address.as_deref().unwrap_or(&p.name),
            p.fingerprint.as_deref().map(identity::short).unwrap_or_else(|| "-".into())
        );
    }
    Ok(())
}

/// Bridges the state machine to Windows and the network.
struct WinIo<'a> {
    net: &'a Net,
    capture: &'a Capture,
    injector: inject::Injector,
    cursor: config::CursorSettings,
    clipboard: Option<&'a ClipboardWorker>,
    /// Commands to run on this machine, drained by the main loop.
    commands: Vec<Command>,
    /// Drag and drop (None when file sharing is off).
    catcher: Option<&'a DragCatcher>,
    fetch: desklink_win::clipboard::Fetch,
    peer_names: Vec<String>,
    /// Max file / total bytes for dragged files (0 = unlimited).
    drag_limits: (u64, u64),
    drag_conflict: Option<desklink_core::files::Resolution>,
    /// Cancels the drag this machine is running for a peer.
    drag_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl Io for WinIo<'_> {
    fn send(&mut self, peer: PeerId, msg: Message) {
        self.net.send(peer, msg);
    }
    fn grab(&mut self, anchor: (i32, i32), carry_buttons: bool) {
        self.capture.grab_carrying(anchor, carry_buttons);
    }
    fn ungrab(&mut self) {
        self.capture.ungrab();
    }
    fn ungrab_carrying(&mut self) {
        if self.capture.left_held() {
            log::info!("back with the left button held (a drag may follow)");
        }
        self.capture.ungrab_carrying(true);
    }
    fn set_cursor(&mut self, p: (i32, i32)) {
        inject::set_cursor_pos(p);
    }
    fn cursor_pos(&mut self) -> (i32, i32) {
        self.injector.position()
    }
    fn inject_move(&mut self, p: (i32, i32)) {
        self.injector.move_to(p.0, p.1);
    }
    fn inject(&mut self, ev: InputEvent) {
        self.injector.apply(ev);
    }
    fn release_injected(&mut self) {
        self.injector.release_all();
    }
    fn lock_keys(&mut self) -> LockKeys {
        inject::lock_keys()
    }
    fn hide_cursor(&mut self, why: HideReason) {
        let c = &self.cursor;
        let wanted = match why {
            HideReason::Directing => c.hide_while_directing,
            HideReason::Released => c.hide_after_released,
        };
        if wanted && c.method == config::HideMethod::Transparent {
            cursor::hide();
        }
    }
    fn show_cursor(&mut self) {
        if cursor::show() {
            // Windows draws a re-shown cursor where it was last visible (the
            // edge we left through) until mouse input arrives: send a
            // zero-distance move at its real position to redraw it there.
            let (x, y) = inject::cursor_pos();
            self.injector.move_to(x, y);
        }
    }
    fn share_clipboard(&mut self, peer: PeerId) {
        if let Some(c) = self.clipboard {
            c.request_read(peer);
        }
    }
    fn watch_local_activity(&mut self, purpose: Watch) {
        match purpose {
            Watch::Reveal => self.capture.watch_activity(self.cursor.reveal_on_mouse, self.cursor.reveal_on_keyboard),
            Watch::Takeover => self.capture.watch_activity(true, true),
        }
    }
    fn game_holds_mouse(&mut self) -> bool {
        desklink_win::fullscreen::screen().is_some()
    }
    fn left_button_held(&mut self) -> bool {
        // Windows' view (also injected presses, when controlled), or the
        // physical button (Windows doesn't see presses swallowed while grabbed).
        self.catcher.is_some() && (inject::left_button_down() || self.capture.left_held())
    }
    fn start_drag_catch(&mut self) {
        // Only called when `left_button_held` said yes, i.e. with a catcher.
        if let Some(c) = self.catcher {
            c.begin(Duration::from_millis(400));
        }
    }
    fn park(&mut self, anchor: (i32, i32)) {
        self.capture.repark(anchor);
    }
    fn start_drag(&mut self, peer: PeerId, files: FileOffer) {
        if self.catcher.is_none() {
            return; // file sharing is off here
        }
        self.cancel_drag();
        // The drag presses the button itself; the peer's release must pass.
        self.injector.mark_held(desklink_proto::MouseButton::Left);
        self.drag_cancel = Some(dragdrop::start_drag(
            files,
            dragdrop::DragOptions {
                peer,
                peer_name: self.peer_names[peer].clone(),
                fetch: self.fetch.clone(),
                conflict: self.drag_conflict,
                max_file: self.drag_limits.0,
                max_total: self.drag_limits.1,
            },
        ));
    }
    fn cancel_drag(&mut self) {
        if let Some(c) = self.drag_cancel.take() {
            c.store(true, std::sync::atomic::Ordering::Release);
        }
    }
    fn run_command(&mut self, cmd: Command) {
        self.commands.push(cmd);
    }
}

/// How `run` ended.
enum Stop {
    /// A copy with administrator rights took over.
    Handover,
    Exit,
    /// Settings changed that need everything restarted (peers, ports, ...).
    Restart,
}

/// Run DeskLink in the tray until it is told to exit; settings that can't
/// change live restart the inner part (tray, log and stop request stay).
fn start(paths: &Paths, mut cfg: Config) -> Result<(), String> {
    // Every mouse move waits for DeskLink: run ahead of ordinary programs
    // (the main loop handles input; the hook thread raises itself further).
    desklink_win::raise_process_priority();
    desklink_win::raise_thread_priority(false);
    let tray = Tray::start()?;
    // The settings window shows the log through this pipe.
    // On a client, the settings window shows the master's layout through this.
    desklink_win::pipe::serve("layout", |mut out| {
        use std::io::Write;
        let (current, new) = pushed::follow();
        let mut send = |line: &str| writeln!(out, "{line}").and_then(|_| out.flush()).is_ok();
        if send(&current) {
            for line in new {
                if !send(&line) {
                    break;
                }
            }
        }
    })?;
    desklink_win::pipe::serve("log", |mut out| {
        use std::io::Write;
        let (backlog, new) = logging::follow();
        // Stops when the window closes (the write fails).
        let mut send = |line: &str| writeln!(out, "{line}").and_then(|_| out.flush()).is_ok();
        if backlog.iter().all(|l| send(l)) {
            for line in new {
                if !send(&line) {
                    break;
                }
            }
        }
    })?;
    if cfg.peers.is_empty() {
        log::warn!("no other PCs yet: add one in the settings window");
        open_window(paths, "settings");
    }

    // A previous run may have died with the cursor hidden; and never leave
    // it hidden if this one panics.
    cursor::repair();
    // If this process dies without restoring the cursor (killed, crashed), a
    // small watchdog process restores it.
    spawn_cursor_guard();
    let stop_requests = desklink_win::instance::listen_for_stop().unwrap_or_else(|e| {
        log::warn!("`desklink stop` unavailable: {e}");
        crossbeam_channel::never()
    });
    // Self-update: automatic checks, and "Restart to update" from the
    // settings window.
    let update_requests = update::start(tray.handle(), &cfg.updates);
    let default_hook = std::panic::take_hook();
    let crash_dir = paths.dir.clone();
    let _ = CRASH_DIR.set(paths.dir.clone());
    desklink_win::on_hard_crash(|what| {
        cursor::show();
        if let Some(dir) = CRASH_DIR.get() {
            logging::crash_report(dir, &format!("crashed: {what}"));
        }
    });
    std::panic::set_hook(Box::new(move |info| {
        cursor::show();
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        logging::crash_report(&crash_dir, &format!("crashed in thread '{thread}': {info}"));
        default_hook(info);
    }));
    loop {
        match run(paths, cfg, &tray, &stop_requests, &update_requests)? {
            // An elevated copy took over: the settings window stays (it
            // reconnects to the new copy).
            Stop::Handover => return Ok(()),
            Stop::Exit => {
                // The settings window goes with us.
                desklink_win::instance::close_settings();
                // A custom config goes to the restarted copy too.
                let custom = (Paths::resolve(None).config != paths.config).then_some(paths.config.as_path());
                update::apply_on_exit(custom);
                return Ok(());
            }
            Stop::Restart => {
                cfg = Config::load_or_default(&paths.config)?;
                log::info!("restarting with the new settings");
            }
        }
    }
}

fn run(
    paths: &Paths,
    mut cfg: Config,
    tray: &Tray,
    stop_requests: &crossbeam_channel::Receiver<()>,
    update_requests: &crossbeam_channel::Receiver<()>,
) -> Result<Stop, String> {
    let path = &paths.config;
    // Show every option (new ones with defaults) in the file.
    if cfg.save_if_changed(path)? {
        log::info!("updated {} with all current options", path.display());
    }
    let id = Identity::load_or_create_at(&paths.cert, &paths.key)?;
    log::info!(
        "DeskLink '{}' fingerprint {} (config {})",
        cfg.name,
        identity::short(&id.fingerprint()),
        path.display()
    );

    let monitors = display::monitors();
    // Placed before their real size was known, peers may cover our screens.
    if cfg.fix_overlaps(&monitors) {
        log::info!("moved other PCs that covered this PC's screens");
        cfg.save(path)?;
    }
    let (mods, key) = hotkey::parse(&cfg.hotkeys.return_to_local)?;
    let capture = Capture::start()?;
    let (pmods, pkey) = hotkey::parse(&cfg.hotkeys.paste_files)?;
    capture.set_hotkeys(vec![
        Hotkey { id: HK_RETURN, modifiers: mods, key, release_grab: true },
        Hotkey { id: HK_PASTE_FILES, modifiers: pmods, key: pkey, release_grab: false },
    ]);
    capture.set_drift_filter(cfg.local_activity.min_mouse_move, cfg.local_activity.window_ms);

    let mut net_cfg = NetConfig::new(
        cfg.name.clone(),
        cfg.peers
            .iter()
            .map(|p| PeerConfig {
                name: p.name.clone(),
                address: Some(p.address.clone().unwrap_or_else(|| p.name.clone())),
                fingerprint: p.fingerprint.clone(),
                last_addr: p.last_ip.as_deref().and_then(|s| s.parse().ok()),
            })
            .collect(),
    );
    net_cfg.port = cfg.port;
    net_cfg.role = proto_role(cfg.role);
    // Input goes out on these threads: busy PCs mustn't delay it.
    net_cfg.on_thread_start = Some(|| desklink_win::raise_thread_priority(false));
    if cfg.security.enabled {
        net_cfg.access = access(&cfg.security);
    }
    // Headroom over the clipboard limit for message framing.
    net_cfg.max_bulk_bytes = ((cfg.clipboard.max_size_mb as usize) << 20) + (1 << 20);
    let net = Net::start(net_cfg, &id, monitors.clone())?;
    // Tell the network we are here (the master's settings window lists us).
    let _announcer = desklink_net::discovery::announce(&cfg.name, &identity::short(&id.fingerprint()), cfg.port, proto_role(cfg.role).name())
        .map_err(|e| log::warn!("{e}"))
        .ok();
    let session = SessionWatcher::start()?;
    // Downloads offered files from peers (paste and drag-and-drop).
    let fetch: desklink_win::clipboard::Fetch = {
        let fc = net.file_client();
        std::sync::Arc::new(move |peer, offer, index, offset| {
            fc.open(peer, offer, index, offset).map(|r| Box::new(r) as Box<dyn std::io::Read>)
        })
    };
    // Dragging files across the edge (part of file sharing).
    let catcher = if cfg.clipboard.enabled && cfg.clipboard.files {
        match DragCatcher::start() {
            Ok(c) => Some(c),
            Err(e) => {
                log::warn!("drag and drop unavailable: {e}");
                None
            }
        }
    } else {
        None
    };
    let clip = if cfg.clipboard.enabled {
        Some(ClipboardWorker::start(
            ClipboardOptions {
                text: cfg.clipboard.text,
                images: cfg.clipboard.images,
                max_bytes: (cfg.clipboard.max_size_mb as usize) << 20,
                respect_private: cfg.clipboard.respect_private,
                files: cfg.clipboard.files,
                max_file_bytes: cfg.clipboard.max_file_mb.saturating_mul(1 << 20),
                max_total_bytes: cfg.clipboard.max_total_mb.saturating_mul(1 << 20),
                virtual_files: cfg.clipboard.paste_mode == config::PasteMode::Virtual,
                conflict: cfg.clipboard.on_conflict.resolution(),
            },
            Some(fetch.clone()),
        )?)
    } else {
        None
    };
    log::info!(
        "listening on UDP {}; return hotkey {}",
        net.local_port(),
        cfg.hotkeys.return_to_local
    );

    let mut core = Core::new(
        &monitors,
        cfg.peers
            .iter()
            .map(|p| PeerSlot {
                name: p.name.clone(),
                edge: p.side.into(),
                offset: p.position.map(|[x, y]| (x, y)),
                screens: None,
                // Not on the layout yet: nothing leads to it.
                skip: p.unplaced,
            })
            .collect(),
    );
    let mb = |m: u64| m.saturating_mul(1 << 20);
    let mut io = WinIo {
        net: &net,
        capture: &capture,
        injector: inject::Injector::new(),
        cursor: cfg.cursor.clone(),
        clipboard: clip.as_ref(),
        commands: Vec::new(),
        catcher: catcher.as_ref(),
        fetch: fetch.clone(),
        peer_names: cfg.peers.iter().map(|p| p.name.clone()).collect(),
        drag_limits: (mb(cfg.clipboard.max_file_mb), mb(cfg.clipboard.max_total_mb)),
        drag_conflict: cfg.clipboard.on_conflict.resolution(),
        drag_cancel: None,
    };
    let clip_events = clip.as_ref().map(|c| c.events().clone()).unwrap_or_else(crossbeam_channel::never);
    let drag_catches = catcher.as_ref().map(|c| c.results().clone()).unwrap_or_else(crossbeam_channel::never);
    core.set_park((cfg.cursor.method != config::HideMethod::None).then(|| cfg.cursor.park.into()));
    // A client is driven by its master only.
    core.set_can_direct(cfg.role != Role::Client);
    // Peer mode: the layout of the PC the cursor is on decides where it goes.
    core.set_own_layout_rules(cfg.role == Role::Peer);
    core.set_corner_guard(corner_guard(&cfg));
    core.set_fullscreen_guard(!cfg.cursor.switch_in_fullscreen);
    // A full-screen program keeps the cursor on its screen (checked a few
    // times a second; released when it goes, and when DeskLink stops).
    let mut confine = desklink_win::fullscreen::Confine::default();
    let mut confine_checked = Instant::now();
    let mut game_held = false;
    // While controlled: since when input goes to a screen that refuses it
    // (screensaver, lock screen, UAC), and when we last closed a screensaver.
    let mut refused_since: Option<Instant> = None;
    let mut saver_closed: Option<Instant> = None;
    let ticker = tick(Duration::from_millis(50));
    let file_client = net.file_client();
    let mut peer_os: Vec<Option<Os>> = vec![None; cfg.peers.len()];
    let mut last_files: Option<files_ui::Incoming> = None;
    let limits = files_ui::Settings {
        max_file: mb(cfg.clipboard.max_file_mb),
        max_total: mb(cfg.clipboard.max_total_mb),
        on_conflict: cfg.clipboard.on_conflict,
    };
    // "Disable" in the tray menu: no sharing until "Enable".
    let mut enabled = true;
    // Saved settings are picked up once a second.
    let mut settings_stamp = modified(path);
    let mut settings_checked = Instant::now();
    // Which peers are connected, for the tray icon.
    let mut connected = vec![false; cfg.peers.len()];
    show_status(&tray, enabled, &connected, &cfg);

    loop {
        select! {
            recv(capture.events()) -> ev => {
                let mut ev = ev;
                loop {
                    let now = Instant::now();
                    // An event read while merging moves, handled next.
                    let mut next = None;
                    match ev {
                        Ok(CaptureEvent::Input(InputEvent::MouseMove { dx, dy })) => {
                            // Moves that queued up meanwhile (a busy PC): send
                            // them as one, instead of replaying a backlog.
                            let (mut dx, mut dy) = (dx, dy);
                            while let Ok(e) = capture.events().try_recv() {
                                match e {
                                    CaptureEvent::Input(InputEvent::MouseMove { dx: x, dy: y }) => {
                                        dx += x;
                                        dy += y;
                                    }
                                    other => {
                                        next = Some(Ok(other));
                                        break;
                                    }
                                }
                            }
                            core.on_local(&mut io, Local::Input(InputEvent::MouseMove { dx, dy }), now);
                        }
                        Ok(CaptureEvent::CursorMoved { x, y }) => core.on_local(&mut io, Local::CursorMoved((x, y)), now),
                        Ok(CaptureEvent::Input(ev)) => core.on_local(&mut io, Local::Input(ev), now),
                        Ok(CaptureEvent::GrabReleased(r)) => {
                            log::info!("grab released: {r:?}");
                            core.on_local(&mut io, Local::GrabReleased, now);
                        }
                        Ok(CaptureEvent::HookRestored { mouse, keyboard }) => {
                            log::warn!("input hook restored by watchdog (mouse={mouse} keyboard={keyboard})");
                        }
                        Ok(CaptureEvent::LocalActivity) => core.on_local(&mut io, Local::Activity, now),
                        Ok(CaptureEvent::Hotkey(HK_PASTE_FILES)) => core.on_local(&mut io, Local::Command(Command::PasteFiles), now),
                        Ok(CaptureEvent::Hotkey(_)) => {}
                        Err(_) => return Err("capture stopped".into()),
                    }
                    match next {
                        Some(e) => ev = e,
                        None => break,
                    }
                }
            }
            recv(net.events()) -> ev => {
                let now = Instant::now();
                match ev {
                    Ok(NetEvent::Connected { peer, monitors, os }) => {
                        connected[peer] = true;
                        show_status(&tray, enabled, &connected, &cfg);
                        peer_os[peer] = Some(os);
                        core.on_connected(peer, &monitors);
                        // Remember its screens for the Layout tab; the edge facing
                        // us stays where the user put it.
                        if cfg.update_peer_monitors(peer, monitors, &display::monitors()) {
                            cfg.fix_overlaps(&display::monitors());
                            core.set_offset(peer, cfg.peers[peer].position.map(|[x, y]| (x, y)));
                            if let Err(e) = cfg.save(path) {
                                log::error!("saving peer screens: {e}");
                            }
                        }
                        push_layout(&net, &cfg, &connected);
                    }
                    Ok(NetEvent::Message { peer, msg: Message::Clipboard { items, files } }) => {
                        let Some(c) = &clip else {
                            log::debug!("ignoring clipboard from peer {peer} (sharing disabled)");
                            continue;
                        };
                        let name = &cfg.peers[peer].name;
                        let files = files.filter(|_| cfg.clipboard.files);
                        if !items.is_empty() {
                            log::info!("clipboard <- {name}: {}", describe(&items));
                        }
                        if let Some(offer) = &files {
                            log::info!("files <- {name}: {} (transferred when pasted)", desklink_core::files::summary(offer));
                        }
                        // Auto: Ctrl+V in any app pulls the files on demand.
                        // Dialog: ask where to save them right away.
                        let on_demand = cfg.clipboard.files_mode == config::FilesMode::Auto;
                        if !items.is_empty() || files.is_some() {
                            c.write(peer, name, items, files.clone().filter(|_| on_demand));
                        }
                        // A newer clipboard replaces any earlier file offer.
                        last_files = files.map(|offer| files_ui::Incoming { peer, peer_name: name.clone(), offer });
                        if !on_demand && let Some(inc) = &last_files {
                            files_ui::spawn_paste(file_client.clone(), inc.clone(), files_ui::Dest::Downloads, limits);
                        }
                    }
                    Ok(NetEvent::Message { msg: Message::Layout { pcs }, .. }) => {
                        if cfg.role == Role::Client {
                            pushed::set(&pcs);
                        }
                    }
                    Ok(NetEvent::Message { peer, msg }) => core.on_message(&mut io, peer, msg, now),
                    Ok(NetEvent::Disconnected { peer, .. }) => {
                        connected[peer] = false;
                        if cfg.role == Role::Client {
                            pushed::set(&[]); // no master: no layout to show
                        }
                        show_status(&tray, enabled, &connected, &cfg);
                        core.on_disconnected(&mut io, peer);
                    }
                    Ok(NetEvent::PeerAddress { peer, addr }) => {
                        cfg.peers[peer].last_ip = Some(addr.to_string());
                        if let Err(e) = cfg.save(path) {
                            log::error!("saving peer address: {e}");
                        }
                    }
                    Ok(NetEvent::Pinned { peer, fingerprint }) => {
                        cfg.peers[peer].fingerprint = Some(fingerprint);
                        if let Err(e) = cfg.save(path) {
                            log::error!("saving pinned fingerprint: {e}");
                        }
                    }
                    Ok(NetEvent::Rejected { reason }) => log::warn!("rejected: {reason}"),
                    Ok(NetEvent::Stranger { name, addr, fingerprint }) => {
                        // A client without a master: the first one to connect
                        // becomes it (pinned; from then on only that one).
                        if cfg.role == Role::Client && cfg.peers.is_empty() {
                            log::info!("{name} ({addr}) connected: it is this client's master from now on");
                            cfg.peers.push(config::Peer {
                                fingerprint: Some(fingerprint),
                                last_ip: Some(addr.to_string()),
                                ..config::Peer::new(name, Side::Left)
                            });
                            cfg.save(path)?;
                            core.shutdown(&mut io);
                            drop(io);
                            net.shutdown(GoodbyeReason::Disabled, Duration::from_millis(300));
                            return Ok(Stop::Restart);
                        }
                    }
                    Err(_) => return Err("network stopped".into()),
                }
            }
            recv(clip_events) -> ev => {
                if let Ok(ClipEvent::Read { peer, items, files }) = ev {
                    let name = &cfg.peers[peer].name;
                    if !items.is_empty() {
                        log::info!("clipboard -> {name}: {}", describe(&items));
                    }
                    let files = files.map(|(offer, paths)| {
                        log::info!("files -> {name}: {}", desklink_core::files::summary(&offer));
                        net.publish_files(offer.id, paths);
                        offer
                    });
                    net.send_bulk(peer, Message::Clipboard { items, files });
                }
            }
            recv(session.events()) -> ev => match ev {
                Ok(SessionEvent::EndSession { logoff, ack }) => {
                    core.shutdown(&mut io);
                    net.pause(if logoff { GoodbyeReason::Logoff } else { GoodbyeReason::Shutdown });
                    let _ = ack.send(());
                }
                Ok(SessionEvent::Suspend { ack }) => {
                    core.shutdown(&mut io);
                    net.pause(GoodbyeReason::Sleep);
                    let _ = ack.send(());
                }
                Ok(SessionEvent::EndCancelled) | Ok(SessionEvent::Resume) => {
                    // Disabled in the tray stays disabled.
                    if enabled {
                        log::info!("resuming");
                        net.resume();
                    }
                }
                Ok(SessionEvent::DisplayChanged) => {
                    let monitors = display::monitors();
                    log::info!("display layout changed: {} monitors", monitors.len());
                    core.set_monitors(&monitors);
                    net.set_monitors(monitors);
                    // Peers learn the new layout on reconnect.
                    net.disconnect_all(GoodbyeReason::Disabled);
                }
                Ok(SessionEvent::Quit { ack }) => {
                    log::info!("exiting");
                    core.shutdown(&mut io);
                    drop(io);
                    net.shutdown(GoodbyeReason::Exit, Duration::from_millis(300));
                    let _ = ack.send(());
                    return Ok(Stop::Exit);
                }
                Err(_) => return Err("session watcher stopped".into()),
            },
            recv(drag_catches) -> paths => {
                // Background drag check done: share the files, if any.
                let offer = paths.ok().flatten().and_then(|paths| {
                    let (offer, local) = desklink_core::files::build_offer(desklink_core::files::new_offer_id(), &paths);
                    if offer.entries.is_empty() {
                        return None;
                    }
                    if let Err(why) = desklink_core::files::check_limits(&offer, limits.max_file, limits.max_total) {
                        log::warn!("dragged files not shared: {why}");
                        return None;
                    }
                    net.publish_files(offer.id, local);
                    Some(offer)
                });
                core.on_local(&mut io, Local::DragCaught(offer), Instant::now());
            }
            recv(tray.events()) -> ev => match ev {
                Ok(TrayEvent::Open) => open_window(paths, "settings"),
                Ok(TrayEvent::Toggle) => {
                    enabled = !enabled;
                    if enabled {
                        log::info!("enabled: sharing with other PCs again");
                        net.resume();
                    } else {
                        log::info!("disabled: not sharing with other PCs");
                        // Keyboard and mouse back here, everything released.
                        core.shutdown(&mut io);
                        net.pause(GoodbyeReason::Disabled);
                    }
                    show_status(&tray, enabled, &connected, &cfg);
                }
                Ok(TrayEvent::CheckUpdate) => open_window(paths, "update"),
                Ok(TrayEvent::ApplyUpdate) => {
                    log::info!("restarting to update");
                    update::install_on_exit(true);
                    core.shutdown(&mut io);
                    drop(io);
                    net.shutdown(GoodbyeReason::Exit, Duration::from_millis(300));
                    return Ok(Stop::Exit);
                }
                Ok(TrayEvent::Exit) | Err(_) => {
                    log::info!("exit from the tray");
                    // A downloaded update installs now (not on `desklink
                    // stop`: build.cmd stops DeskLink to copy a new build in).
                    update::install_on_exit(false);
                    core.shutdown(&mut io);
                    drop(io);
                    net.shutdown(GoodbyeReason::Exit, Duration::from_millis(300));
                    return Ok(Stop::Exit);
                }
            },
            recv(update_requests) -> _ => {
                // "Restart to update" in the settings window.
                if update::is_ready() {
                    log::info!("restarting to update");
                    update::install_on_exit(true);
                    core.shutdown(&mut io);
                    drop(io);
                    net.shutdown(GoodbyeReason::Exit, Duration::from_millis(300));
                    return Ok(Stop::Exit);
                }
            },
            recv(stop_requests) -> _ => {
                // `desklink stop`: exit the same clean way as Ctrl+C.
                log::info!("stop requested; exiting");
                core.shutdown(&mut io);
                drop(io);
                net.shutdown(GoodbyeReason::Exit, Duration::from_millis(300));
                return Ok(Stop::Exit);
            },
            recv(ticker) -> _ => {
                core.on_tick(&mut io, Instant::now());
                if confine_checked.elapsed() >= Duration::from_millis(250) {
                    confine_checked = Instant::now();
                    // Not while this PC drives another one (its cursor is
                    // parked, maybe on another screen).
                    let here = enabled && !cfg.cursor.switch_in_fullscreen && matches!(core.focus(), Focus::Local);
                    let screen = here.then(desklink_win::fullscreen::screen).flatten();
                    if screen.is_some() != game_held {
                        game_held = screen.is_some();
                        log::info!("{}", if game_held { "a game holds the mouse: the cursor stays in it" } else { "the game let go of the mouse: cursor free again" });
                    }
                    confine.set(screen);
                }
                // While controlled, every tick: a screensaver should go the
                // moment the cursor arrives.
                if core.is_controlled() {
                    use desklink_win::desktop;
                    // Our input doesn't end it, so close it.
                    if desktop::screensaver_running() && saver_closed.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
                        let what = desktop::close_screensaver();
                        log::info!("screensaver running: closing it ({what})");
                        saver_closed = Some(Instant::now());
                    }
                    // Still on a screen that refuses our input: don't keep
                    // the other PC's keyboard and mouse stuck here.
                    if desktop::input_elsewhere() {
                        let since = *refused_since.get_or_insert_with(Instant::now);
                        if since.elapsed() >= Duration::from_millis(1500) {
                            core.hand_back(&mut io, "this PC shows a screen that refuses input (screensaver, lock screen or an admin prompt)");
                            refused_since = None;
                        }
                    } else {
                        refused_since = None;
                    }
                } else {
                    refused_since = None;
                }
                // The settings window saved: apply what can change live,
                // restart for the rest.
                if settings_checked.elapsed() >= Duration::from_secs(1) {
                    settings_checked = Instant::now();
                    let stamp = modified(path);
                    if stamp != settings_stamp {
                        settings_stamp = stamp;
                        let loaded = Config::load_or_default(path);
                        if let Ok(new) = &loaded
                            && new.run_as_admin != cfg.run_as_admin
                        {
                            if new.run_as_admin && !desklink_win::elevation::is_elevated() {
                                // Hand over to a copy with administrator rights.
                                if relaunch_elevated(paths) {
                                    core.shutdown(&mut io);
                                    drop(io);
                                    net.shutdown(GoodbyeReason::Exit, Duration::from_millis(300));
                                    return Ok(Stop::Handover);
                                }
                            } else if !new.run_as_admin && desklink_win::elevation::is_elevated() {
                                log::info!("DeskLink keeps administrator rights until it is started again");
                            }
                            cfg.run_as_admin = new.run_as_admin;
                        }
                        match loaded {
                            Ok(new) if needs_restart(&cfg, &new) => {
                                core.shutdown(&mut io);
                                drop(io);
                                net.shutdown(GoodbyeReason::Disabled, Duration::from_millis(300));
                                return Ok(Stop::Restart);
                            }
                            Ok(mut new) => {
                                // A layout placed with an old idea of a PC's size may
                                // cover our screens: fix it before using it.
                                if new.fix_overlaps(&display::monitors()) {
                                    log::info!("moved other PCs that covered this PC's screens");
                                    if let Err(e) = new.save(path) {
                                        log::error!("saving the fixed layout: {e}");
                                    }
                                    settings_stamp = modified(path);
                                }
                                apply_live(&new, &mut core, &mut io, &capture);
                                cfg = new;
                                // Clients show the master's layout, live.
                                push_layout(&net, &cfg, &connected);
                            }
                            Err(e) => log::warn!("new settings not applied: {e}"),
                        }
                    }
                }
            }
        }

        // Commands the state machine decided to run on this machine.
        for cmd in std::mem::take(&mut io.commands) {
            match cmd {
                Command::PasteFiles => match &last_files {
                    Some(inc) => files_ui::spawn_paste(file_client.clone(), inc.clone(), files_ui::Dest::InFrontOf(ui::foreground_window()), limits),
                    None => files_ui::nothing_to_paste(),
                },
            }
        }
    }
}




pub fn proto_role(role: Role) -> desklink_proto::Role {
    match role {
        Role::Peer => desklink_proto::Role::Peer,
        Role::Master => desklink_proto::Role::Master,
        Role::Client => desklink_proto::Role::Client,
    }
}

/// Master: send the whole layout to every connected client (they show it,
/// read-only; nothing to do in other roles).
fn push_layout(net: &Net, cfg: &Config, connected: &[bool]) {
    if cfg.role != Role::Master {
        return;
    }
    let local = display::monitors();
    let desk = layout::Desk::new(&local, cfg.peers.iter().map(layout::PlacedPeer::from_config).collect());
    let mut pcs = vec![desklink_proto::LayoutPc { name: cfg.name.clone(), master: true, x: 0, y: 0, monitors: local }];
    for (i, p) in cfg.peers.iter().enumerate() {
        if p.unplaced {
            continue; // not on the layout yet
        }
        let (x, y) = desk.offset(i);
        pcs.push(desklink_proto::LayoutPc { name: p.name.clone(), master: false, x, y, monitors: p.monitors.clone() });
    }
    for (peer, _) in connected.iter().enumerate().filter(|(_, c)| **c) {
        net.send(peer, Message::Layout { pcs: pcs.clone() });
    }
}
/// The Security tab's rules for the network. A network that doesn't parse
/// (typed into the file by hand) is reported and allows nothing.
fn access(s: &desklink_config::SecuritySettings) -> desklink_net::access::Access {
    use desklink_net::access::{Access, Subnet};
    let mut networks = Vec::new();
    for n in &s.networks {
        match n.parse::<Subnet>() {
            Ok(n) => networks.push(n),
            Err(e) => log::warn!("security: {e}; ignored"),
        }
    }
    if networks.is_empty() && !s.networks.is_empty() {
        // Listed networks were meant to restrict: don't fall open.
        networks.push("255.255.255.255".parse().unwrap());
    }
    log::info!(
        "security on: networks {}, PCs {}",
        if s.networks.is_empty() { "any".into() } else { s.networks.join(", ") },
        if s.allowed.is_empty() { "any".into() } else { s.allowed.join(", ") }
    );
    Access { networks, allowed: s.allowed.clone() }
}

fn modified(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Do the settings differ in more than what `apply_live` handles?
fn needs_restart(old: &Config, new: &Config) -> bool {
    let live_only = |c: &Config| {
        let mut c = c.clone();
        c.cursor = Default::default();
        c.local_activity = Default::default();
        c.hotkeys = Default::default();
        // The log is set up once per start of the app.
        c.log = Default::default();
        // The self-update picks these up by itself.
        c.updates = Default::default();
        // Handled on its own: a hand-over to an elevated copy.
        c.run_as_admin = false;
        for p in &mut c.peers {
            p.side = Side::Left;
            p.position = None;
            p.monitors.clear();
            p.unplaced = false;
            p.last_ip = None;
        }
        c
    };
    !live_only(old).same_as(&live_only(new))
}

/// Settings that take effect without restarting: layout, cursor, hotkeys,
/// micro-drift filter.

/// Corners that don't switch (`None`: they do).
fn corner_guard(cfg: &Config) -> Option<i32> {
    (!cfg.cursor.switch_at_corners).then_some(cfg.cursor.corner_size as i32)
}
fn apply_live(cfg: &Config, core: &mut Core, io: &mut WinIo<'_>, capture: &Capture) {
    for (i, p) in cfg.peers.iter().enumerate() {
        core.set_edge(i, p.side.into());
        core.set_offset(i, p.position.map(|[x, y]| (x, y)));
        // Not on the layout yet: nothing leads to it.
        core.set_skip(i, p.unplaced);
    }
    io.cursor = cfg.cursor.clone();
    core.set_corner_guard(corner_guard(cfg));
    core.set_fullscreen_guard(!cfg.cursor.switch_in_fullscreen);
    core.set_park((cfg.cursor.method != config::HideMethod::None).then(|| cfg.cursor.park.into()));
    capture.set_drift_filter(cfg.local_activity.min_mouse_move, cfg.local_activity.window_ms);
    update::configure(&cfg.updates);
    match (hotkey::parse(&cfg.hotkeys.return_to_local), hotkey::parse(&cfg.hotkeys.paste_files)) {
        (Ok((mods, key)), Ok((pmods, pkey))) => capture.set_hotkeys(vec![
            Hotkey { id: HK_RETURN, modifiers: mods, key, release_grab: true },
            Hotkey { id: HK_PASTE_FILES, modifiers: pmods, key: pkey, release_grab: false },
        ]),
        (Err(e), _) | (_, Err(e)) => log::warn!("hotkeys not changed: {e}"),
    }
    log::info!("settings applied");
}
/// Tray icon: light blue while connected to a PC (named in the tooltip),
/// dark blue while not, grey while disabled.
fn show_status(tray: &Tray, enabled: bool, connected: &[bool], cfg: &Config) {
    let names: Vec<&str> = cfg.peers.iter().zip(connected).filter(|(_, c)| **c).map(|(p, _)| p.name.as_str()).collect();
    if !enabled {
        tray.set_status(TrayState::Disabled, "DeskLink: disabled");
    } else if names.is_empty() {
        tray.set_status(TrayState::Waiting, "DeskLink: no other PC connected");
    } else {
        tray.set_status(TrayState::Connected, &format!("DeskLink: connected to {}", names.join(", ")));
    }
}
/// "text 12 B, dib 31.6 MB" for logs.
fn describe(items: &[desklink_proto::ClipItem]) -> String {
    items
        .iter()
        .map(|i| {
            let n = i.data.len();
            let size = if n >= 1 << 20 {
                format!("{:.1} MB", n as f64 / (1 << 20) as f64)
            } else if n >= 1 << 10 {
                format!("{:.1} KB", n as f64 / 1024.0)
            } else {
                format!("{n} B")
            };
            format!("{:?} {size}", i.format).to_lowercase()
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Start `desklink cursor-guard <our pid>`: it waits for this process to end
/// and then restores the cursor scheme (harmless if it was never hidden).
fn spawn_cursor_guard() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    let Ok(exe) = std::env::current_exe() else { return };
    let spawned = std::process::Command::new(exe)
        .args(["cursor-guard", &std::process::id().to_string()])
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Err(e) = spawned {
        log::warn!("cursor watchdog not started: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_and_cursor_apply_live_peers_and_clipboard_restart() {
        let mut old = Config::defaults();
        old.peers.push(config::Peer::new("flash", Side::Left));
        let mut new = old.clone();
        new.peers[0].side = Side::Right;
        new.peers[0].position = Some([-1920, 0]);
        new.cursor.park = config::ParkAt::Center;
        new.local_activity.min_mouse_move = 40;
        assert!(!needs_restart(&old, &new));
        let mut added = old.clone();
        added.peers.push(config::Peer::new("laptop", Side::Top));
        assert!(needs_restart(&old, &added));
        let mut clip = old.clone();
        clip.clipboard.images = false;
        assert!(needs_restart(&old, &clip));
    }

    #[test]
    fn config_defaults_match_their_owners() {
        assert_eq!(desklink_config::DEFAULT_PORT, desklink_net::DEFAULT_PORT);
        assert_eq!(desklink_config::DEFAULT_DRIFT_MIN_PX, desklink_win::capture::DEFAULT_DRIFT_MIN_PX);
        assert_eq!(desklink_config::DEFAULT_DRIFT_WINDOW_MS, desklink_win::capture::DEFAULT_DRIFT_WINDOW_MS);
    }
}
