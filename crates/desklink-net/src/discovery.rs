//! Finding DeskLink PCs on the local network: every DeskLink broadcasts a
//! small announcement every second; the settings window listens
//! and offers the ones not set up yet.
//!
//! Plain UDP broadcast on `port + 1`. It carries no secrets: connecting still
//! goes through QUIC with pinned identities.

use crossbeam_channel::{Receiver, unbounded};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const MAGIC: &str = "DESKLINK1";
/// How often a PC announces itself.
pub const INTERVAL: Duration = Duration::from_secs(1);

/// Discovery runs on the port after DeskLink's own.
pub fn port_for(desklink_port: u16) -> u16 {
    desklink_port.wrapping_add(1)
}

/// A DeskLink heard on the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    /// Short fingerprint, for showing.
    pub fingerprint: String,
    /// Where to connect (its IP and DeskLink port).
    pub addr: SocketAddr,
    /// "peer", "master" or "client".
    pub role: String,
}

fn encode(name: &str, fingerprint: &str, port: u16, role: &str) -> String {
    format!("{MAGIC}\n{name}\n{fingerprint}\n{port}\n{role}")
}

fn decode(text: &str, from: SocketAddr) -> Option<Found> {
    let mut it = text.lines();
    if it.next()? != MAGIC {
        return None;
    }
    let name = it.next()?.to_string();
    let fingerprint = it.next()?.to_string();
    let port: u16 = it.next()?.parse().ok()?;
    let role = it.next().unwrap_or("peer").to_string();
    (!name.is_empty()).then(|| Found { name, fingerprint, addr: SocketAddr::new(from.ip(), port), role })
}

/// Announces this PC until dropped.
pub struct Announcer {
    stop: Arc<AtomicBool>,
}

impl Drop for Announcer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// Broadcast "DeskLink `name` is here" every `INTERVAL`.
pub fn announce(name: &str, fingerprint: &str, port: u16, role: &str) -> Result<Announcer, String> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(|e| format!("discovery: {e}"))?;
    socket.set_broadcast(true).map_err(|e| format!("discovery: {e}"))?;
    let message = encode(name, fingerprint, port, role);
    let to = SocketAddr::from((Ipv4Addr::BROADCAST, port_for(port)));
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    std::thread::Builder::new()
        .name("dl-announce".into())
        .spawn(move || {
            while !flag.load(Ordering::Acquire) {
                // No network yet (or a blip): just try again next time.
                let _ = socket.send_to(message.as_bytes(), to);
                std::thread::sleep(INTERVAL);
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(Announcer { stop })
}

/// Listen for announcements on `port_for(desklink_port)`.
pub fn listen(desklink_port: u16) -> Result<Receiver<Found>, String> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port_for(desklink_port))).map_err(|e| format!("discovery: {e}"))?;
    let (tx, rx) = unbounded();
    std::thread::Builder::new()
        .name("dl-discover".into())
        .spawn(move || {
            let mut buf = [0u8; 512];
            while let Ok((n, from)) = socket.recv_from(&mut buf) {
                if let Some(found) = std::str::from_utf8(&buf[..n]).ok().and_then(|t| decode(t, from))
                    && tx.send(found).is_err()
                {
                    break; // nobody listens any more
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    #[ignore = "uses the real network (broadcast); run with --ignored"]
    fn hears_its_own_announcement() {
        let port = 40_000 + (std::process::id() % 10_000) as u16;
        let heard = listen(port).unwrap();
        let _me = announce("probe-pc", "ab12-cd34", port, "client").unwrap();
        let found = heard.recv_timeout(INTERVAL * 2).expect("no announcement heard");
        assert_eq!(found.name, "probe-pc");
        assert_eq!(found.addr.port(), port);
    }
    #[test]
    fn announcement_round_trip() {
        let from: SocketAddr = "192.168.1.175:51000".parse().unwrap();
        let found = decode(&encode("flashunreal", "f288-68cd", 24801, "client"), from).unwrap();
        assert_eq!(found.name, "flashunreal");
        assert_eq!(found.addr, "192.168.1.175:24801".parse().unwrap());
        assert_eq!(found.role, "client");
        assert_eq!(decode("HELLO\nx\ny\n1", from), None);
    }
}
