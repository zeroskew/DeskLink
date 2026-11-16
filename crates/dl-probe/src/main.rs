//! DeskLink probe: single-machine diagnostics for the Windows platform layer.
//!
//!   dl-probe displays             list monitors (physical pixels, scale)
//!   dl-probe watch                print captured events (nothing is grabbed)
//!   dl-probe loopback [secs]      grab all input and re-inject it locally:
//!                                 the full capture -> inject pipeline
//!   dl-probe hookloss             silently remove our hooks, watch them recover
//!   dl-probe stall                stall the hook so Windows (maybe) drops it
//!   dl-probe clipboard            show what would be shared from the clipboard (read-only)
//!   dl-probe clipboard-selftest   write sample clipboard data, read it back (overwrites clipboard!)
//!   dl-probe files-selftest       copy the files on the clipboard over a localhost link into a temp folder
//!   dl-probe folders              which folder the paste-files hotkey would use (TC, Explorer)
//!   dl-probe fullscreen [secs]    every second: the window in front, and whether it holds the mouse (a game)
//!   dl-probe desktop [secs]       every second: whether input goes to another desktop (lock screen, secure screensaver, UAC)
//!   dl-probe desktop-close [secs] the same, and closes a running screensaver (as DeskLink does)
//!   dl-probe paste-selftest [virtual]  offer the clipboard's files over localhost (download or virtual mode)
//!
//! In grabbing modes, LCtrl+LAlt+Pause releases immediately, and the grab
//! always ends by itself after the given number of seconds (default 15).

use crossbeam_channel::{RecvTimeoutError, select, tick};
use desklink_proto::Key;
use desklink_win::capture::{Capture, CaptureEvent, Hotkey};
use desklink_win::{display, inject};
use std::time::{Duration, Instant};

const HK_RELEASE: u32 = 1;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    if !desklink_win::init_dpi_awareness() {
        log::warn!("could not enable per-monitor DPI awareness");
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let secs = |i: usize| args.get(i).and_then(|s| s.parse().ok()).unwrap_or(15u64);
    match args.first().map(String::as_str) {
        Some("displays") => displays(),
        Some("watch") => watch(),
        Some("loopback") => loopback(Duration::from_secs(secs(1))),
        Some("hookloss") => hook_recovery(false),
        Some("stall") => hook_recovery(true),
        Some("clipboard") => clipboard(),
        Some("clipboard-selftest") => clipboard_selftest(),
        Some("files-selftest") => files_selftest(),
        Some("folders") => folders(),
        Some("fullscreen") => fullscreen(Duration::from_secs(secs(1))),
        Some("desktop") => desktop(Duration::from_secs(secs(1)), false),
        Some("desktop-close") => desktop(Duration::from_secs(secs(1)), true),
        Some("paste-selftest") => paste_selftest(),
        _ => {
            eprintln!("usage: dl-probe <displays | watch | loopback [secs] | hookloss | stall | clipboard | clipboard-selftest | files-selftest | folders | fullscreen [secs] | desktop [secs] | desktop-close [secs]>");
            std::process::exit(2);
        }
    }
}

fn displays() {
    for m in display::monitors() {
        println!(
            "{:<16} {:>5}x{:<5} at ({:>6},{:>6})  scale {:>3}%{}",
            m.name,
            m.width,
            m.height,
            m.x,
            m.y,
            m.scale_percent,
            if m.primary { "  [primary]" } else { "" }
        );
    }
    let vd = inject::VirtualDesktop::get();
    println!("virtual desktop: {}x{} at ({},{})", vd.w, vd.h, vd.x, vd.y);
}

fn start_capture() -> Capture {
    let cap = Capture::start().unwrap_or_else(|e| {
        eprintln!("capture failed: {e}");
        std::process::exit(1);
    });
    cap.set_hotkeys(vec![Hotkey {
        id: HK_RELEASE,
        modifiers: vec![Key(0xE0), Key(0xE2)], // LCtrl + LAlt
        key: Key(0x48),                        // Pause/Break
        release_grab: true,
    }]);
    cap
}

fn watch() {
    let cap = start_capture();
    println!("watching input for 60s (Ctrl+C to quit). Cursor moves are summarised.");
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut moves = 0u32;
    let mut last_pos = (0, 0);
    let ticker = tick(Duration::from_secs(1));
    while Instant::now() < deadline {
        select! {
            recv(cap.events()) -> ev => match ev {
                Ok(CaptureEvent::CursorMoved { x, y }) => { moves += 1; last_pos = (x, y); }
                Ok(ev) => println!("{ev:?}"),
                Err(_) => break,
            },
            recv(ticker) -> _ => {
                if moves > 0 {
                    println!("cursor: {moves} moves/s, at {last_pos:?}");
                    moves = 0;
                }
            }
        }
    }
}

fn loopback(limit: Duration) {
    let cap = start_capture();
    let mut injector = inject::Injector::new();
    println!(
        "Grabbing in 2s for up to {}s. All input is captured, swallowed and\n\
         re-injected, so the PC should behave normally. LCtrl+LAlt+Pause to stop.",
        limit.as_secs()
    );
    std::thread::sleep(Duration::from_secs(2));

    cap.grab(inject::cursor_pos());
    let start = Instant::now();
    let (mut events, mut max_lag) = (0u64, Duration::ZERO);

    loop {
        let remaining = limit.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            println!("time limit reached");
            break;
        }
        match cap.events().recv_timeout(remaining) {
            Ok(CaptureEvent::Input(ev)) => {
                let t = Instant::now();
                events += 1;
                // Our injections are tagged, so the hook lets them through.
                injector.apply(ev);
                max_lag = max_lag.max(t.elapsed());
            }
            Ok(CaptureEvent::GrabReleased(r)) => {
                println!("grab released by hook: {r:?}");
                break;
            }
            Ok(CaptureEvent::HookRestored { .. }) => println!("hook restored by watchdog"),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    cap.ungrab();
    injector.release_all();
    let s = cap.stats();
    println!(
        "{events} events in {:.1}s, worst inject time {:?}, hook restores {}",
        start.elapsed().as_secs_f32(),
        max_lag,
        s.restores
    );
}

fn hook_recovery(stall: bool) {
    let cap = start_capture();
    if stall {
        println!(
            "Making the next 3 hook callbacks sleep 1.5s each, longer than Windows'\n\
             LowLevelHooksTimeout. Keep moving the mouse and typing. Windows may or\n\
             may not remove the hook; if it does, the watchdog reinstalls it."
        );
        cap.simulate_stall(1500, 3);
    } else {
        println!(
            "Hooks will be silently removed in 3s, the way Windows removes an\n\
             unresponsive hook. Keep moving the mouse and typing: 'hook' counters\n\
             freeze, 'raw' keep going, and within ~1-2s the watchdog reinstalls."
        );
        std::thread::sleep(Duration::from_secs(3));
        cap.simulate_hook_loss();
        println!("--- hooks removed ---");
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    let ticker = tick(Duration::from_secs(1));
    let mut restored = false;
    while Instant::now() < deadline {
        select! {
            recv(cap.events()) -> ev => {
                if let Ok(CaptureEvent::HookRestored { mouse, keyboard }) = ev {
                    println!(">>> watchdog reinstalled hooks: mouse={mouse} keyboard={keyboard}");
                    restored = true;
                }
            },
            recv(ticker) -> _ => {
                let s = cap.stats();
                println!(
                    "hook m/k {:>6}/{:<5}  raw m/k {:>6}/{:<5}  restores {}",
                    s.hook_mouse, s.hook_keyboard, s.raw_mouse, s.raw_keyboard, s.restores
                );
            }
        }
    }
    if !restored {
        println!("no restore happened (did you move the mouse / type?)");
    }
}

fn clipboard() {
    use desklink_win::clipboard::{ClipEvent, ClipboardOptions, ClipboardWorker};
    let w = ClipboardWorker::start(ClipboardOptions {
        text: true,
        images: true,
        max_bytes: 256 << 20,
        respect_private: true,
        files: true,
        max_file_bytes: 0,
        max_total_bytes: 0,
        virtual_files: false,
        conflict: None,
    }, None)
    .expect("clipboard worker");
    let t = Instant::now();
    w.request_read(0);
    match w.events().recv_timeout(Duration::from_secs(5)) {
        Ok(ClipEvent::Read { items, files, .. }) => {
            if let Some((offer, _)) = &files {
                println!("files: {}", desklink_core::files::summary(offer));
                for e in offer.entries.iter().take(20) {
                    println!("  {}{}", e.path, if e.is_dir { "/" } else { "" });
                }
            }
            println!("read in {:?}:", t.elapsed());
            for i in &items {
                let preview = match i.format {
                    desklink_proto::ClipFormat::Text => {
                        let s = String::from_utf8_lossy(&i.data);
                        format!("  \"{}\"", s.chars().take(60).collect::<String>().replace('\n', "\n"))
                    }
                    _ => String::new(),
                };
                println!("  {:?}: {} bytes{preview}", i.format, i.data.len());
            }
            let raw: usize = items.iter().map(|i| i.data.len()).sum();
            let packed = desklink_proto::encode(&desklink_proto::Message::Clipboard { items, files: None }).unwrap();
            println!("  total {raw} bytes (encoded {} bytes before LZ4)", packed.len());
        }
        Err(_) => println!("nothing to share (empty, unsupported formats, private, or over the limit)"),
    }
}

fn clipboard_selftest() {
    use desklink_proto::{ClipFormat, ClipItem};
    use desklink_win::clipboard::{ClipEvent, ClipboardOptions, ClipboardWorker};
    let w = ClipboardWorker::start(ClipboardOptions {
        text: true,
        images: true,
        max_bytes: 256 << 20,
        respect_private: true,
        files: true,
        max_file_bytes: 0,
        max_total_bytes: 0,
        virtual_files: false,
        conflict: None,
    }, None)
    .expect("clipboard worker");

    // 2x2 32-bit DIB: BITMAPINFOHEADER (40 bytes) + 16 bytes of pixels.
    let mut dib = Vec::new();
    for v in [40u32, 2, 2] {
        dib.extend_from_slice(&v.to_le_bytes());
    }
    dib.extend_from_slice(&1u16.to_le_bytes()); // planes
    dib.extend_from_slice(&32u16.to_le_bytes()); // bpp
    dib.extend_from_slice(&[0u8; 24]); // BI_RGB, sizes, palette
    dib.extend_from_slice(&[0, 0, 255, 0, 0, 255, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0]);

    let text = "DeskLink ✓ Привет 👋\r\nline 2";
    let html = "Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\n<b>DeskLink</b>";
    let rtf = r"{\rtf1\ansi {\b DeskLink} test}";
    let sent = vec![
        ClipItem { format: ClipFormat::Text, data: text.as_bytes().to_vec() },
        ClipItem { format: ClipFormat::Html, data: html.as_bytes().to_vec() },
        ClipItem { format: ClipFormat::Rtf, data: rtf.as_bytes().to_vec() },
        ClipItem { format: ClipFormat::Dib, data: dib.clone() },
    ];

    // As if received from peer 0.
    w.write(0, "selftest", sent.clone(), None);

    // Echo protection: reading for peer 0 must not send its own data back.
    w.request_read(0);
    let echo = w.events().recv_timeout(Duration::from_millis(1500));
    println!("echo back to sender suppressed: {}", if echo.is_err() { "PASS" } else { "FAIL" });

    // Another peer gets the content: compare every format.
    w.request_read(1);
    let Ok(ClipEvent::Read { items, .. }) = w.events().recv_timeout(Duration::from_secs(5)) else {
        println!("read back: FAIL (nothing read)");
        return;
    };
    let mut ok = true;
    for s in &sent {
        let got = items.iter().find(|i| i.format == s.format);
        let pass = match (s.format, got) {
            // Windows may pad DIBs; the header and pixels must match.
            (ClipFormat::Dib, Some(g)) => g.data.len() >= s.data.len() && g.data[..s.data.len()] == s.data[..],
            (_, Some(g)) => g.data == s.data,
            (_, None) => false,
        };
        ok &= pass;
        println!(
            "  {:?}: {} ({} bytes back)",
            s.format,
            if pass { "PASS" } else { "FAIL" },
            got.map_or(0, |g| g.data.len())
        );
    }
    println!("round trip: {}", if ok { "PASS" } else { "FAIL" });
}

fn files_selftest() {
    use desklink_core::files::{self, FileSource, Progress};
    use desklink_net::identity::Identity;
    use desklink_net::{Net, NetConfig, NetEvent, PeerConfig};
    use desklink_win::clipboard::{ClipEvent, ClipboardOptions, ClipboardWorker};

    // What's on the clipboard?
    let w = ClipboardWorker::start(ClipboardOptions {
        text: false,
        images: false,
        max_bytes: 1 << 20,
        respect_private: true,
        files: true,
        max_file_bytes: 0,
        max_total_bytes: 0,
        virtual_files: false,
        conflict: None,
    }, None)
    .expect("clipboard worker");
    w.request_read(0);
    let Ok(ClipEvent::Read { files: Some((offer, paths)), .. }) = w.events().recv_timeout(Duration::from_secs(10)) else {
        println!("no files on the clipboard: copy some in Explorer first");
        return;
    };
    println!("offer: {}", files::summary(&offer));

    // Two endpoints on localhost.
    let port = |_| std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let (pa, pb) = (port(0), port(1));
    let mk = |name: &str, port: u16, peer: &str, addr: Option<String>| {
        let mut c = NetConfig::new(name, vec![PeerConfig { name: peer.into(), address: addr, fingerprint: None, last_addr: None }]);
        c.port = port;
        c
    };
    let a = Net::start(mk("a", pa, "b", None), &Identity::generate().unwrap(), vec![]).unwrap();
    let b = Net::start(mk("b", pb, "a", Some(format!("127.0.0.1:{pa}"))), &Identity::generate().unwrap(), vec![]).unwrap();
    while !matches!(b.events().recv_timeout(Duration::from_secs(5)), Ok(NetEvent::Connected { .. })) {}
    a.publish_files(offer.id, paths.clone());

    struct Src(desklink_net::FileClient, u64);
    impl FileSource for Src {
        fn open(&self, index: u32) -> Result<Box<dyn std::io::Read>, String> {
            Ok(Box::new(self.0.open(0, self.1, index, 0)?))
        }
    }
    struct Quiet;
    impl Progress for Quiet {
        fn update(&mut self, _: u64, _: u64, _: &str) {}
        fn cancelled(&self) -> bool {
            false
        }
    }
    let dest = std::env::temp_dir().join(format!("desklink-files-selftest-{}", std::process::id()));
    std::fs::create_dir_all(&dest).unwrap();
    let t = Instant::now();
    let mut keep_both = files::Policy(files::Resolution::Rename);
    let created = files::download(&offer, &Src(b.file_client(), offer.id), &dest, &mut Quiet, &mut keep_both).expect("download");
    println!("downloaded in {:?} into {}", t.elapsed(), dest.display());

    // Compare every file with its source.
    let mut ok = true;
    for (e, src) in offer.entries.iter().zip(&paths) {
        let Some(src) = src else { continue };
        let same = std::fs::read(src).ok() == std::fs::read(dest.join(&e.path)).ok();
        ok &= same;
        println!("  {} {}", if same { "PASS" } else { "FAIL" }, e.path);
    }
    println!("created: {created:?}");
    println!("files round trip: {}", if ok { "PASS" } else { "FAIL" });
    let _ = std::fs::remove_dir_all(&dest);
}

fn folders() {
    use desklink_win::ui;
    let _com = ui::com_init();
    // Every top-level window of a file manager we know.
    for class in ["TTOTAL_CMD", "CabinetWClass", "Progman"] {
        let wide: Vec<u16> = class.encode_utf16().chain(Some(0)).collect();
        let mut hwnd = std::ptr::null_mut();
        loop {
            hwnd = unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::FindWindowExW(
                    std::ptr::null_mut(),
                    hwnd,
                    wide.as_ptr(),
                    std::ptr::null(),
                )
            };
            if hwnd.is_null() {
                break;
            }
            println!("{class:<14} -> {:?}", ui::folder_for_window(hwnd as isize));
        }
    }
    println!("fallback       -> {:?}", ui::downloads_folder());
}

/// Puts the clipboard's files back on the clipboard as a remote
/// paste-on-demand placeholder served over localhost, then waits 30 s so
/// another app can paste them (that triggers the transfer).
fn paste_selftest() {
    let virtual_files = std::env::args().any(|a| a == "virtual");
    use desklink_net::identity::Identity;
    use desklink_net::{Net, NetConfig, NetEvent, PeerConfig};
    use desklink_win::clipboard::{ClipEvent, ClipboardOptions, ClipboardWorker, Fetch};
    let opts = ClipboardOptions {
        text: false,
        images: false,
        max_bytes: 1 << 20,
        respect_private: true,
        files: true,
        max_file_bytes: 0,
        max_total_bytes: 0,
        virtual_files,
        conflict: None,
    };
    let port = |_| std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let (pa, pb) = (port(0), port(1));
    let mk = |name: &str, port: u16, peer: &str, addr: Option<String>| {
        let mut c = NetConfig::new(name, vec![PeerConfig { name: peer.into(), address: addr, fingerprint: None, last_addr: None }]);
        c.port = port;
        c
    };
    let a = Net::start(mk("a", pa, "b", None), &Identity::generate().unwrap(), vec![]).unwrap();
    let b = Net::start(mk("b", pb, "a", Some(format!("127.0.0.1:{pa}"))), &Identity::generate().unwrap(), vec![]).unwrap();
    while !matches!(b.events().recv_timeout(Duration::from_secs(5)), Ok(NetEvent::Connected { .. })) {}

    let fc = b.file_client();
    let fetch: Fetch = std::sync::Arc::new(move |peer, offer, index, offset| {
        fc.open(peer, offer, index, offset).map(|r| Box::new(r) as Box<dyn std::io::Read>)
    });
    let w = ClipboardWorker::start(opts, Some(fetch)).expect("worker");
    w.request_read(1);
    let Ok(ClipEvent::Read { files: Some((offer, paths)), .. }) = w.events().recv_timeout(Duration::from_secs(10)) else {
        println!("no files on the clipboard");
        return;
    };
    a.publish_files(offer.id, paths);
    println!("placeholder for {} on the clipboard; paste within 30 s", desklink_core::files::summary(&offer));
    w.write(0, "selftest-peer", Vec::new(), Some(offer));
    // DeskLink only serves a paste right after a key press or click. The
    // probe has no hook: simulate a person unless "noinput" is given.
    if !std::env::args().any(|a| a == "noinput") {
        std::thread::spawn(|| loop {
            desklink_win::intent::note();
            std::thread::sleep(Duration::from_millis(300));
        });
    }
    std::thread::sleep(Duration::from_secs(if virtual_files { 60 } else { 30 }));
}

/// Switch to a game within the time given: what DeskLink sees in front.
fn fullscreen(for_: Duration) {
    use desklink_win::fullscreen;
    let end = Instant::now() + for_;
    while Instant::now() < end {
        match fullscreen::front() {
            Some(f) => println!(
                "{} {:?}: window {:?} monitor {:?} pointer hidden {} captured {} locked {}",
                if f.holds_mouse() { "HOLDS MOUSE" } else { "free       " },
                f.class,
                f.window,
                f.monitor,
                f.pointer_hidden,
                f.captured,
                f.locked
            ),
            None => println!("nothing in front"),
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Every second: does input reach our desktop (not the lock screen, a secure
/// screensaver or UAC), and is a screensaver running?
fn desktop(for_: Duration, close: bool) {
    use desklink_win::desktop;
    let end = Instant::now() + for_;
    while Instant::now() < end {
        let saver = desktop::screensaver_running();
        println!("input elsewhere {}  screensaver running {saver}", desktop::input_elsewhere());
        if saver && close {
            println!("  closing it: {}", desktop::close_screensaver());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}
