//! End-to-end transport tests over localhost.

use desklink_net::identity::Identity;
use desklink_net::{Net, NetConfig, NetEvent, PeerConfig};
use desklink_proto::{GoodbyeReason, InputEvent, Key, Message};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn cfg(name: &str, port: u16, peer: &str, peer_addr: Option<String>, pin: Option<String>) -> NetConfig {
    let mut c = NetConfig::new(
        name,
        vec![PeerConfig { name: peer.into(), address: peer_addr, fingerprint: pin, last_addr: None }],
    );
    c.port = port;
    c
}

/// Wait for the first event matching `f`, skipping others.
fn wait_for(net: &Net, within: Duration, f: impl Fn(&NetEvent) -> bool) -> Option<(NetEvent, Duration)> {
    let start = Instant::now();
    while let Some(left) = within.checked_sub(start.elapsed()) {
        match net.events().recv_timeout(left) {
            Ok(ev) if f(&ev) => return Some((ev, start.elapsed())),
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
    None
}

fn connected(ev: &NetEvent) -> bool {
    matches!(ev, NetEvent::Connected { .. })
}
fn disconnected(ev: &NetEvent) -> bool {
    matches!(ev, NetEvent::Disconnected { .. })
}

fn pair() -> (Net, Net, Identity, u16) {
    let (ida, idb) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let pb = free_port();
    let b = Net::start(cfg("b", pb, "a", None, None), &idb, vec![]).unwrap();
    let a = Net::start(cfg("a", free_port(), "b", Some(format!("127.0.0.1:{pb}")), None), &ida, vec![]).unwrap();
    assert!(wait_for(&a, Duration::from_secs(5), connected).is_some(), "a connects");
    assert!(wait_for(&b, Duration::from_secs(5), connected).is_some(), "b connects");
    (a, b, ida, pb)
}

#[test]
fn connect_pin_and_exchange() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (a, b, _, _) = pair();
    let msg = Message::Input { session: 1, event: InputEvent::Key { key: Key(4), down: true } };
    a.send(0, msg.clone());
    let (ev, _) = wait_for(&b, Duration::from_secs(2), |e| matches!(e, NetEvent::Message { .. })).unwrap();
    assert_eq!(ev, NetEvent::Message { peer: 0, msg: msg.clone() });
    b.send(0, msg.clone());
    assert!(wait_for(&a, Duration::from_secs(2), |e| matches!(e, NetEvent::Message { .. })).is_some());
}

#[test]
fn goodbye_is_reported_immediately() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (a, b, _, _) = pair();
    let t = Instant::now();
    a.shutdown(GoodbyeReason::Shutdown, Duration::from_millis(500));
    let (ev, _) = wait_for(&b, Duration::from_secs(2), disconnected).expect("b notices");
    let took = t.elapsed();
    println!("goodbye noticed after {took:?}: {ev:?}");
    assert!(took < Duration::from_millis(600), "took {took:?}");
    assert!(matches!(ev, NetEvent::Disconnected { reason, .. } if reason.contains("goodbye")));
}

#[test]
fn reconnects_after_restart() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (a, b, ida, pb) = pair();
    a.shutdown(GoodbyeReason::Exit, Duration::from_millis(300));
    assert!(wait_for(&b, Duration::from_secs(2), disconnected).is_some());

    let t = Instant::now();
    let a2 = Net::start(cfg("a", free_port(), "b", Some(format!("127.0.0.1:{pb}")), None), &ida, vec![]).unwrap();
    assert!(wait_for(&a2, Duration::from_secs(5), connected).is_some());
    assert!(wait_for(&b, Duration::from_secs(5), connected).is_some());
    println!("reconnected after {:?}", t.elapsed());
}

#[test]
fn changed_certificate_is_rejected() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (a, b, _, pb) = pair();
    a.shutdown(GoodbyeReason::Exit, Duration::from_millis(300));
    assert!(wait_for(&b, Duration::from_secs(2), disconnected).is_some());

    // Same name, different key: an impostor.
    let evil = Identity::generate().unwrap();
    let _a2 = Net::start(cfg("a", free_port(), "b", Some(format!("127.0.0.1:{pb}")), None), &evil, vec![]).unwrap();
    let (ev, _) = wait_for(&b, Duration::from_secs(5), |e| matches!(e, NetEvent::Rejected { .. } | NetEvent::Connected { .. })).unwrap();
    assert!(matches!(ev, NetEvent::Rejected { ref reason } if reason.contains("changed")), "{ev:?}");
}

#[test]
fn simultaneous_dial_settles_on_one_connection() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (pa, pb) = (free_port(), free_port());
    let (ida, idb) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let a = Net::start(cfg("a", pa, "b", Some(format!("127.0.0.1:{pb}")), None), &ida, vec![]).unwrap();
    let b = Net::start(cfg("b", pb, "a", Some(format!("127.0.0.1:{pa}")), None), &idb, vec![]).unwrap();
    assert!(wait_for(&a, Duration::from_secs(5), connected).is_some());
    assert!(wait_for(&b, Duration::from_secs(5), connected).is_some());
    // Give duplicate resolution time, then verify the link is stable and works.
    std::thread::sleep(Duration::from_millis(1500));
    assert!(a.is_connected(0) && b.is_connected(0));
    for _ in 0..50 {
        a.send(0, Message::Heartbeat { seq: 0 }); // filtered by transport
    }
    let msg = Message::Leave { session: 9 };
    a.send(0, msg.clone());
    let (ev, _) = wait_for(&b, Duration::from_secs(2), |e| !matches!(e, NetEvent::Pinned { .. } | NetEvent::PeerAddress { .. })).unwrap();
    assert_eq!(ev, NetEvent::Message { peer: 0, msg }, "no disconnect churn");
}

/// Fold a side's event stream into "does the app think the peer is up?".
/// This is what the core sees; it must end up `true` while the link is up.
fn app_view(net: &Net, settle: Duration) -> (bool, Vec<String>) {
    let mut up = false;
    let mut log = Vec::new();
    while let Ok(ev) = net.events().recv_timeout(settle) {
        match ev {
            NetEvent::Connected { .. } => up = true,
            NetEvent::Disconnected { .. } => up = false,
            _ => continue,
        }
        log.push(format!("{ev:?}"));
    }
    (up, log)
}

#[test]
fn event_order_matches_link_state_under_races() {
    let _ = env_logger::builder().is_test(true).try_init();
    for round in 0..5 {
        let (pa, pb) = (free_port(), free_port());
        let (ida, idb) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let a = Net::start(cfg("a", pa, "b", Some(format!("127.0.0.1:{pb}")), None), &ida, vec![]).unwrap();
        let b = Net::start(cfg("b", pb, "a", Some(format!("127.0.0.1:{pa}")), None), &idb, vec![]).unwrap();
        let (va, la) = app_view(&a, Duration::from_millis(1500));
        let (vb, lb) = app_view(&b, Duration::from_millis(1500));
        assert!(a.is_connected(0) && b.is_connected(0), "round {round}: link up");
        assert!(va, "round {round}: a's app thinks peer is down: {la:?}");
        assert!(vb, "round {round}: b's app thinks peer is down: {lb:?}");

        // Restart b with pins known (the deferred-dial path) and re-check.
        b.shutdown(GoodbyeReason::Exit, Duration::from_millis(200));
        let pin_a = ida.fingerprint();
        let b2 = Net::start(cfg("b", pb, "a", Some(format!("127.0.0.1:{pa}")), Some(pin_a)), &idb, vec![]).unwrap();
        let (va, la) = app_view(&a, Duration::from_millis(2500));
        let (vb, lb) = app_view(&b2, Duration::from_millis(500));
        assert!(va, "round {round} after restart: a thinks down: {la:?}");
        assert!(vb, "round {round} after restart: b thinks down: {lb:?}");
    }
}

#[test]
fn bulk_clipboard_transfer() {
    use desklink_proto::{ClipFormat, ClipItem};
    let _ = env_logger::builder().is_test(true).try_init();
    let (a, b, _, _) = pair();
    // A 4K-screenshot-sized DIB with screenshot-like redundancy.
    let mut dib = vec![0u8; 3840 * 2160 * 4];
    for (i, px) in dib.chunks_mut(4).enumerate() {
        px[0] = (i / 3840 % 256) as u8; // horizontal bands
    }
    let msg = Message::Clipboard {
        items: vec![
            ClipItem { format: ClipFormat::Text, data: "héllo 👋".as_bytes().to_vec() },
            ClipItem { format: ClipFormat::Dib, data: dib },
        ],
        files: None,
    };
    let t = Instant::now();
    a.send_bulk(0, msg.clone());
    // Input sent right after must not wait behind the bulk transfer.
    let key = Message::Input { session: 1, event: InputEvent::Key { key: Key(4), down: true } };
    a.send(0, key.clone());
    let mut got_key = None;
    let mut got_clip = None;
    while got_clip.is_none() {
        match b.events().recv_timeout(Duration::from_secs(10)).expect("event") {
            NetEvent::Message { msg: m, .. } if m == key => got_key = Some(t.elapsed()),
            NetEvent::Message { msg: m, .. } if matches!(m, Message::Clipboard { .. }) => {
                assert_eq!(m, msg);
                got_clip = Some(t.elapsed());
            }
            _ => {}
        }
    }
    println!("33 MB clipboard in {:?}, key in {:?}", got_clip.unwrap(), got_key);
    assert!(got_key.is_some_and(|k| k < got_clip.unwrap()), "input overtook the bulk transfer");
}

#[test]
fn file_transfer_resume_and_refusals() {
    use std::io::Read;
    let _ = env_logger::builder().is_test(true).try_init();
    let (a, b, _, _) = pair();
    let dir = std::env::temp_dir().join(format!("desklink-files-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("big.bin");
    let data: Vec<u8> = (0..50u32 << 20).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
    std::fs::write(&path, &data).unwrap();

    a.publish_files(7, vec![None, Some(path.clone())]);
    let client = b.file_client();

    // Whole file.
    let t = Instant::now();
    let mut r = client.open(0, 7, 1, 0).expect("open");
    assert_eq!(r.remaining(), data.len() as u64);
    let mut got = Vec::new();
    r.read_to_end(&mut got).unwrap();
    let took = t.elapsed();
    assert!(got == data, "content matches");
    println!("50 MB in {took:?} ({:.0} MB/s)", 50.0 / took.as_secs_f64());

    // Resume from an offset.
    let mut r = client.open(0, 7, 1, 40 << 20).expect("open at offset");
    let mut tail = Vec::new();
    r.read_to_end(&mut tail).unwrap();
    assert!(tail[..] == data[40 << 20..]);

    // Directory entries have no content.
    assert!(client.open(0, 7, 0, 0).is_err());
    // A newer offer (e.g. a drag) keeps older ones (the clipboard's) usable...
    a.publish_files(8, vec![Some(path.clone())]);
    assert!(client.open(0, 7, 1, 0).is_ok(), "older offer still served");
    // ...but only the latest few: after 4 newer ones, 7 is gone.
    for id in 9..12 {
        a.publish_files(id, vec![Some(path.clone())]);
    }
    let e = client.open(0, 7, 1, 0).err().expect("oldest offer dropped");
    assert!(e.contains("no longer offered"), "{e}");

    // Cancelling mid-way (drop) doesn't disturb the link.
    let mut r = client.open(0, 8, 0, 0).unwrap();
    let mut buf = vec![0u8; 1 << 20];
    r.read_exact(&mut buf).unwrap();
    drop(r);
    let mut r = client.open(0, 8, 0, 0).unwrap();
    r.read_exact(&mut buf).unwrap();
    assert!(buf[..] == data[..1 << 20]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn checksum_guards_source_changes_and_empty_files() {
    use std::io::Read;
    let _ = env_logger::builder().is_test(true).try_init();
    let (a, b, _, _) = pair();
    let dir = std::env::temp_dir().join(format!("desklink-sum-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let big = dir.join("growing.bin");
    let empty = dir.join("empty.txt");
    std::fs::write(&big, vec![7u8; 64 << 20]).unwrap();
    std::fs::write(&empty, b"").unwrap();
    a.publish_files(1, vec![Some(big.clone()), Some(empty.clone())]);
    let client = b.file_client();

    // Empty file: opens, verifies, reads nothing.
    let mut r = client.open(0, 1, 1, 0).expect("empty file verifies");
    assert_eq!(r.read(&mut [0u8; 16]).unwrap(), 0);

    // Big file modified on the sender mid-transfer: must fail, not succeed
    // with mixed content.
    let mut r = client.open(0, 1, 0, 0).unwrap();
    let mut buf = vec![0u8; 1 << 20];
    r.read_exact(&mut buf).unwrap();
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&big).unwrap();
        f.write_all(b"more").unwrap();
    }
    let mut rest = Vec::new();
    let err = r.read_to_end(&mut rest).err().expect("transfer must fail");
    println!("got: {err}");
    assert!(err.to_string().contains("modified"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The peer's address is learned from the connection and dialled first, so a
/// name that doesn't resolve (or resolves slowly) doesn't delay reconnects.
#[test]
fn reconnects_via_learned_address_when_name_does_not_resolve() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (pa, pb) = (free_port(), free_port());
    let (ida, idb) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    // b only accepts; a knows b by an address that resolves.
    let _b = Net::start(cfg("b", pb, "a", None, None), &idb, vec![]).unwrap();
    let a = Net::start(cfg("a", pa, "b", Some(format!("127.0.0.1:{pb}")), None), &ida, vec![]).unwrap();
    let (ev, _) = wait_for(&a, Duration::from_secs(5), |e| matches!(e, NetEvent::PeerAddress { .. })).expect("address learned");
    let NetEvent::PeerAddress { addr, .. } = ev else { unreachable!() };
    assert_eq!(addr.port(), pb);
    a.shutdown(GoodbyeReason::Exit, Duration::from_millis(200));

    // Restart a with an unresolvable name but the learned address.
    let mut c = cfg("a", pa, "b", Some("no-such-host.invalid".into()), None);
    c.peers[0].last_addr = Some(addr);
    let t = Instant::now();
    let a2 = Net::start(c, &ida, vec![]).unwrap();
    assert!(wait_for(&a2, Duration::from_secs(5), connected).is_some(), "connected via last address");
    println!("reconnected via learned address in {:?}", t.elapsed());
    assert!(t.elapsed() < Duration::from_secs(2));
}

#[test]
fn roles_decide_who_may_connect() {
    let _ = env_logger::builder().is_test(true).try_init();
    let start = |role_a: desklink_proto::Role, role_b: desklink_proto::Role| {
        let (ida, idb) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let pb = free_port();
        let mut cb = cfg("b", pb, "a", None, None);
        cb.role = role_b;
        let mut ca = cfg("a", free_port(), "b", Some(format!("127.0.0.1:{pb}")), None);
        ca.role = role_a;
        let b = Net::start(cb, &idb, vec![]).unwrap();
        let a = Net::start(ca, &ida, vec![]).unwrap();
        (a, b)
    };
    use desklink_proto::Role::*;
    // A master and a client connect.
    let (a, _b) = start(Master, Client);
    assert!(wait_for(&a, Duration::from_secs(5), connected).is_some());
    // A peer and a master don't, and say why.
    let (a, _b) = start(Peer, Master);
    let (ev, _) = wait_for(&a, Duration::from_secs(5), |e| matches!(e, NetEvent::Rejected { .. } | NetEvent::Connected { .. })).unwrap();
    assert!(matches!(&ev, NetEvent::Rejected { reason } if reason.contains("refused") && reason.contains("a is a peer")), "{ev:?}");
    // Two masters don't either.
    let (a, _b) = start(Master, Master);
    let (ev, _) = wait_for(&a, Duration::from_secs(5), |e| matches!(e, NetEvent::Rejected { .. } | NetEvent::Connected { .. })).unwrap();
    assert!(matches!(ev, NetEvent::Rejected { .. }), "{ev:?}");
}
