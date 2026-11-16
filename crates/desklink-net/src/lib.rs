//! DeskLink transport.
//!
//! * QUIC (quinn) with mutual TLS 1.3 over self-signed certificates; peers
//!   are authorised by pinned certificate fingerprint (trust on first use).
//! * One ordered stream per connection carries control + input, so a click is
//!   never applied before the motion that preceded it.
//! * Heartbeats every `heartbeat`; a peer silent for `timeout` is dropped.
//!   Explicit goodbyes close the connection with a code, so the other side
//!   reacts in one round-trip instead of a timeout.
//! * Every configured peer is dialled continuously with a short capped
//!   backoff, and incoming connections are accepted at the same time; if both
//!   sides connect at once, a deterministic tie-break keeps exactly one.
//!
//! The API is synchronous (crossbeam events, non-blocking `send`) so the
//! caller's core loop stays simple; tokio runs on its own threads.

pub mod access;
pub mod discovery;
pub mod identity;
mod tls;

use crossbeam_channel::{Receiver, Sender, unbounded};
use desklink_proto::{FileRequest, FileResponse, GoodbyeReason, Message, Monitor, Os, PROTOCOL_VERSION, Role};
use identity::Identity;
use quinn::{Connection, Endpoint, RecvStream, SendStream, VarInt};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc};

pub type PeerId = usize;

pub const DEFAULT_PORT: u16 = 24801;

const CLOSE_NORMAL: u32 = 0;
const CLOSE_GOODBYE: u32 = 1;
const CLOSE_REPLACED: u32 = 2;
const CLOSE_TIMEOUT: u32 = 3;
const CLOSE_REJECTED: u32 = 4;

const DIAL_TIMEOUT: Duration = Duration::from_secs(2);
const HELLO_TIMEOUT: Duration = Duration::from_secs(2);
const BACKOFF_START: Duration = Duration::from_millis(100);
const BACKOFF_MAX: Duration = Duration::from_millis(1000);
const DEFER_DIAL: Duration = Duration::from_millis(1500);
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(20);
/// File offers kept fetchable at once (the clipboard's and recent drags).
const MAX_OFFERS: usize = 4;
/// Largest control message accepted (long file lists).
const MAX_FRAME: usize = 16 << 20;
const PRIORITY_CONTROL: i32 = 10;
const PRIORITY_BULK: i32 = 0;

#[derive(Clone, Debug)]
pub struct PeerConfig {
    pub name: String,
    /// `host` or `host:port`; `None` = only accept incoming connections.
    pub address: Option<String>,
    /// Pinned certificate fingerprint; `None` = pin on first connection.
    pub fingerprint: Option<String>,
    /// Where the peer was last reached; dialled first.
    pub last_addr: Option<SocketAddr>,
}

#[derive(Clone, Debug)]
pub struct NetConfig {
    pub name: String,
    pub port: u16,
    pub peers: Vec<PeerConfig>,
    pub heartbeat: Duration,
    pub timeout: Duration,
    /// Largest bulk payload (e.g. clipboard) accepted, uncompressed.
    pub max_bulk_bytes: usize,
    /// Peers connect only to peers; a master only to clients and back.
    pub role: Role,
    /// The Security tab: which networks and PCs may talk to us.
    pub access: access::Access,
    /// Run on each network thread as it starts (e.g. raise its priority).
    pub on_thread_start: Option<fn()>,
}

impl NetConfig {
    pub fn new(name: impl Into<String>, peers: Vec<PeerConfig>) -> Self {
        NetConfig {
            name: name.into(),
            port: DEFAULT_PORT,
            peers,
            heartbeat: Duration::from_millis(100),
            timeout: Duration::from_millis(600),
            max_bulk_bytes: 256 << 20,
            role: Role::Peer,
            access: access::Access::default(),
            on_thread_start: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum NetEvent {
    Connected { peer: PeerId, monitors: Vec<Monitor>, os: Os },
    Message { peer: PeerId, msg: Message },
    Disconnected { peer: PeerId, reason: String },
    /// A fingerprint was pinned on first contact; persist it.
    Pinned { peer: PeerId, fingerprint: String },
    /// The peer was reached at a new address; persist it for fast reconnects.
    PeerAddress { peer: PeerId, addr: SocketAddr },
    /// A connection was refused (unknown peer, fingerprint mismatch, ...).
    Rejected { reason: String },
    /// A PC not in our list connected (and was refused): who, from where,
    /// with which identity. A client without a master adopts it.
    Stranger { name: String, addr: SocketAddr, fingerprint: String },
}

struct Live {
    id: u64,
    conn: Connection,
    tx: mpsc::UnboundedSender<Message>,
    initiated_by_me: bool,
}

struct Inner {
    cfg: NetConfig,
    my_fp: String,
    monitors: Mutex<Vec<Monitor>>,
    pins: Mutex<Vec<Option<String>>>,
    /// Where each peer was last reached (dialled before resolving its name).
    known: Vec<Mutex<Option<SocketAddr>>>,
    slots: Vec<Mutex<Option<Live>>>,
    slot_changed: Vec<Notify>,
    events: Sender<NetEvent>,
    next_id: AtomicU64,
    closing: AtomicBool,
    /// The files we currently offer (latest few: clipboard and drags): id
    /// and, per entry, the local path
    /// (`None` for directories).
    offers: Mutex<std::collections::VecDeque<(u64, Vec<Option<PathBuf>>)>>,
    /// Set while sleeping or shutting down the session: no new connections.
    paused: AtomicBool,
    /// PCs already reported as refused for their role (reported once).
    refused_roles: Mutex<std::collections::HashSet<String>>,
}

pub struct Net {
    inner: Arc<Inner>,
    endpoint: Endpoint,
    rt: tokio::runtime::Runtime,
    events: Receiver<NetEvent>,
}

impl Net {
    pub fn start(cfg: NetConfig, id: &Identity, monitors: Vec<Monitor>) -> Result<Net, String> {
        let on_start = cfg.on_thread_start;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("dl-net")
            .on_thread_start(move || {
                if let Some(f) = on_start {
                    f();
                }
            })
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let (server, client) = tls::configs(id, cfg.timeout.max(Duration::from_millis(1500)))?;
        let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, cfg.port));
        let mut endpoint = rt
            .block_on(async { Endpoint::server(server, addr) })
            .map_err(|e| format!("bind UDP {addr}: {e}"))?;
        endpoint.set_default_client_config(client);

        let (tx, rx) = unbounded();
        let n = cfg.peers.len();
        let inner = Arc::new(Inner {
            pins: Mutex::new(cfg.peers.iter().map(|p| p.fingerprint.clone()).collect()),
            known: cfg.peers.iter().map(|p| Mutex::new(p.last_addr)).collect(),
            cfg,
            my_fp: id.fingerprint(),
            monitors: Mutex::new(monitors),
            slots: (0..n).map(|_| Mutex::new(None)).collect(),
            slot_changed: (0..n).map(|_| Notify::new()).collect(),
            events: tx,
            next_id: AtomicU64::new(1),
            closing: AtomicBool::new(false),
            offers: Mutex::new(std::collections::VecDeque::new()),
            paused: AtomicBool::new(false),
            refused_roles: Mutex::default(),
        });

        rt.spawn(accept_loop(inner.clone(), endpoint.clone()));
        for peer in 0..n {
            if inner.cfg.peers[peer].address.is_some() {
                rt.spawn(dial_loop(inner.clone(), endpoint.clone(), peer));
            }
        }
        Ok(Net { inner, endpoint, rt, events: rx })
    }

    pub fn events(&self) -> &Receiver<NetEvent> {
        &self.events
    }

    pub fn local_port(&self) -> u16 {
        self.endpoint.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    /// Queue a message for `peer`. Never blocks; dropped if not connected.
    pub fn send(&self, peer: PeerId, msg: Message) {
        if let Some(Some(live)) = self.inner.slots.get(peer).map(|s| s.lock().unwrap().as_ref().map(|l| l.tx.clone())) {
            let _ = live.send(msg);
        }
    }


    /// Make these local files fetchable by peers as offer `id` (replacing
    /// any previous offer). `paths[i]` is entry `i`'s file; `None` for
    /// directories, which have no content.
    pub fn publish_files(&self, id: u64, paths: Vec<Option<PathBuf>>) {
        let mut offers = self.inner.offers.lock().unwrap();
        offers.retain(|(i, _)| *i != id);
        offers.push_back((id, paths));
        while offers.len() > MAX_OFFERS {
            offers.pop_front();
        }
    }

    /// A handle for fetching files from peers, usable from any thread.
    pub fn file_client(&self) -> FileClient {
        FileClient { inner: self.inner.clone(), rt: self.rt.handle().clone() }
    }
    /// Send a large message (e.g. clipboard) on its own compressed,
    /// low-priority stream so it never delays input. Not ordered relative to
    /// `send`. Dropped if not connected.
    pub fn send_bulk(&self, peer: PeerId, msg: Message) {
        let conn = self.inner.slots.get(peer).and_then(|s| s.lock().unwrap().as_ref().map(|l| l.conn.clone()));
        let Some(conn) = conn else {
            return;
        };
        let name = self.inner.cfg.peers[peer].name.clone();
        self.rt.spawn(async move {
            let started = Instant::now();
            match send_bulk_on(&conn, &msg).await {
                Ok(bytes) => log::debug!("bulk to {name}: {bytes} bytes in {:?}", started.elapsed()),
                Err(e) => log::warn!("bulk send to {name} failed: {e}"),
            }
        });
    }

    pub fn is_connected(&self, peer: PeerId) -> bool {
        self.inner.slots.get(peer).is_some_and(|s| s.lock().unwrap().is_some())
    }

    /// Update the monitor layout announced to peers on (re)connect.
    pub fn set_monitors(&self, monitors: Vec<Monitor>) {
        *self.inner.monitors.lock().unwrap() = monitors;
    }

    /// Tell every peer we are going away and wait (bounded) until the close
    /// frames are on the wire. Peers react immediately.
    pub fn shutdown(self, reason: GoodbyeReason, wait: Duration) {
        self.inner.closing.store(true, Release);
        let why = format!("{reason:?}");
        for slot in &self.inner.slots {
            if let Some(live) = slot.lock().unwrap().take() {
                let _ = live.tx.send(Message::Goodbye { reason });
                live.conn.close(VarInt::from_u32(CLOSE_GOODBYE), why.as_bytes());
            }
        }
        self.endpoint.close(VarInt::from_u32(CLOSE_GOODBYE), why.as_bytes());
        let ep = self.endpoint.clone();
        self.rt.block_on(async move {
            let _ = tokio::time::timeout(wait, ep.wait_idle()).await;
        });
        self.rt.shutdown_timeout(Duration::from_millis(100));
    }

    /// Say goodbye to every peer and refuse new connections until `resume`.
    /// Used on sleep and session end, where the process may live on.
    pub fn pause(&self, reason: GoodbyeReason) {
        self.inner.paused.store(true, Release);
        self.disconnect_all(reason);
    }

    pub fn resume(&self) {
        self.inner.paused.store(false, Release);
        for n in &self.inner.slot_changed {
            n.notify_waiters();
        }
    }

    /// Drop every connection with a goodbye; reconnection starts right away.
    pub fn disconnect_all(&self, reason: GoodbyeReason) {
        let why = format!("{reason:?}");
        for (peer, slot) in self.inner.slots.iter().enumerate() {
            let mut slot = slot.lock().unwrap();
            if let Some(live) = slot.take() {
                live.conn.close(VarInt::from_u32(CLOSE_GOODBYE), why.as_bytes());
                emit(&self.inner, NetEvent::Disconnected { peer, reason: format!("local {why}") });
                drop(slot);
                self.inner.slot_changed[peer].notify_waiters();
            }
        }
    }
}

// ---------------------------------------------------------------------------

fn emit(inner: &Inner, ev: NetEvent) {
    let _ = inner.events.send(ev);
}

async fn write_frame(s: &mut SendStream, msg: &Message) -> Result<(), String> {
    let bytes = desklink_proto::encode(msg).map_err(|e| e.to_string())?;
    let mut buf = Vec::with_capacity(bytes.len() + 4);
    buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(&bytes);
    s.write_all(&buf).await.map_err(|e| e.to_string())
}

async fn read_frame(r: &mut RecvStream) -> Result<Message, String> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await.map_err(|e| e.to_string())?;
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(format!("control message too large ({n} bytes)"));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).await.map_err(|e| e.to_string())?;
    desklink_proto::decode(&buf).map_err(|e| e.to_string())
}

fn hello(inner: &Inner) -> Message {
    Message::Hello {
        version: PROTOCOL_VERSION,
        caps: 0,
        name: inner.cfg.name.clone(),
        monitors: inner.monitors.lock().unwrap().clone(),
        os: Os::current(),
        role: inner.cfg.role,
    }
}

fn peer_fingerprint(conn: &Connection) -> Option<String> {
    let any = conn.peer_identity()?;
    let certs = any.downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>().ok()?;
    certs.first().map(identity::fingerprint)
}


/// Refuse a PC whose role doesn't fit ours (peers only with peers, a master
/// only with clients). Reported once per PC; it keeps retrying.
fn refuse_role(inner: &Inner, conn: &Connection, name: &str, theirs: Role) -> String {
    let reason = format!("{name} is a {}; {}", theirs.name(), inner.cfg.role.refusal(theirs));
    refuse_once(inner, conn, name, reason)
}

/// Close with `reason`; log and report it only the first time for this PC.
fn refuse_once(inner: &Inner, conn: &Connection, name: &str, reason: String) -> String {
    conn.close(VarInt::from_u32(CLOSE_REJECTED), reason.as_bytes());
    if inner.refused_roles.lock().unwrap().insert(name.to_lowercase()) {
        log::warn!("not connecting to {name}: {reason}");
        emit(inner, NetEvent::Rejected { reason: reason.clone() });
    }
    reason
}
fn reject(inner: &Inner, conn: &Connection, reason: String) {
    log::warn!("rejecting connection from {}: {reason}", conn.remote_address());
    conn.close(VarInt::from_u32(CLOSE_REJECTED), reason.as_bytes());
    emit(inner, NetEvent::Rejected { reason });
}

/// Authorise `fp` for `peer`: must match the pin, or becomes the pin.
fn authorize(inner: &Inner, peer: PeerId, fp: &str) -> Result<(), String> {
    let mut pins = inner.pins.lock().unwrap();
    match &pins[peer] {
        Some(pinned) if pinned == fp => Ok(()),
        Some(pinned) => Err(format!(
            "certificate of '{}' changed (pinned {}, got {}); remove the pin to re-pair",
            inner.cfg.peers[peer].name,
            identity::short(pinned),
            identity::short(fp)
        )),
        None => {
            pins[peer] = Some(fp.to_string());
            log::info!("pinned '{}' as {}", inner.cfg.peers[peer].name, identity::short(fp));
            emit(inner, NetEvent::Pinned { peer, fingerprint: fp.to_string() });
            Ok(())
        }
    }
}

async fn accept_loop(inner: Arc<Inner>, endpoint: Endpoint) {
    while let Some(incoming) = endpoint.accept().await {
        let inner = inner.clone();
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(c) => c,
                Err(e) => return log::debug!("incoming handshake failed: {e}"),
            };
            let Ok(Ok((mut send, mut recv))) = tokio::time::timeout(HELLO_TIMEOUT, conn.accept_bi()).await
            else {
                return conn.close(VarInt::from_u32(CLOSE_NORMAL), b"no stream");
            };
            let Ok(Ok(msg)) = tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut recv)).await else {
                return conn.close(VarInt::from_u32(CLOSE_NORMAL), b"no hello");
            };
            let Message::Hello { version, name, monitors, os, role, .. } = msg else {
                return reject(&inner, &conn, "expected hello".into());
            };
            if version != PROTOCOL_VERSION {
                return reject(&inner, &conn, format!("'{name}' speaks protocol v{version}, we speak v{PROTOCOL_VERSION}"));
            }
            if let Err(reason) = inner.cfg.access.check(&name, conn.remote_address().ip()) {
                refuse_once(&inner, &conn, &name, format!("security: {reason}"));
                return;
            }
            if !inner.cfg.role.connects_to(role) {
                refuse_role(&inner, &conn, &name, role);
                return;
            }
            let Some(peer) = inner.cfg.peers.iter().position(|p| p.name.eq_ignore_ascii_case(&name)) else {
                let fingerprint = peer_fingerprint(&conn).unwrap_or_default();
                emit(&inner, NetEvent::Stranger { name: name.clone(), addr: conn.remote_address(), fingerprint });
                return reject(&inner, &conn, format!("unknown peer '{name}'"));
            };
            let fp = peer_fingerprint(&conn).unwrap_or_default();
            if let Err(e) = authorize(&inner, peer, &fp) {
                return reject(&inner, &conn, e);
            }
            if write_frame(&mut send, &hello(&inner)).await.is_err() {
                return;
            }
            adopt(inner, peer, conn, send, recv, false, monitors, os, fp);
        });
    }
}

async fn dial_loop(inner: Arc<Inner>, endpoint: Endpoint, peer: PeerId) {
    let mut backoff = Duration::ZERO;
    loop {
        if inner.closing.load(Acquire) {
            return;
        }
        if inner.slots[peer].lock().unwrap().is_some() {
            // Connected: sleep until the slot empties (or check periodically).
            let _ = tokio::time::timeout(Duration::from_secs(1), inner.slot_changed[peer].notified()).await;
            backoff = Duration::ZERO;
            continue;
        }
        // When both sides know each other, let the preferred dialler (smaller
        // fingerprint) go first; we dial too, a bit later, in case only one
        // direction gets through a firewall.
        let defer = inner.pins.lock().unwrap()[peer].as_ref().is_some_and(|fp| inner.my_fp > *fp);
        tokio::time::sleep(if defer { backoff.max(DEFER_DIAL) } else { backoff }).await;
        if inner.paused.load(Acquire) {
            backoff = BACKOFF_START;
            continue;
        }
        // The peer may have connected to us while we slept.
        if inner.slots[peer].lock().unwrap().is_some() {
            continue;
        }
        match dial(&inner, &endpoint, peer).await {
            Ok(()) => backoff = Duration::ZERO,
            Err(e) => {
                log::debug!("dial {}: {e}", inner.cfg.peers[peer].name);
                backoff = (backoff * 2).clamp(BACKOFF_START, BACKOFF_MAX);
            }
        }
    }
}

async fn dial(inner: &Arc<Inner>, endpoint: &Endpoint, peer: PeerId) -> Result<(), String> {
    // Where the peer was last reached first: resolving a LAN name that DNS
    // doesn't know can take Windows 10+ seconds per attempt.
    let known = inner.known[peer].lock().unwrap().filter(|a| a.is_ipv4());
    if let Some(addr) = known {
        match dial_addr(inner, endpoint, peer, addr).await {
            Ok(()) => return Ok(()),
            Err(e) => log::debug!("dial {} at last address {addr}: {e}", inner.cfg.peers[peer].name),
        }
    }
    let pc = &inner.cfg.peers[peer];
    let addr_s = pc.address.as_deref().unwrap_or_default();
    let target = if addr_s.contains(':') { addr_s.to_string() } else { format!("{addr_s}:{}", inner.cfg.port) };
    let addr = tokio::time::timeout(RESOLVE_TIMEOUT, tokio::net::lookup_host(&target))
        .await
        .map_err(|_| format!("resolve {target}: timed out"))?
        .map_err(|e| format!("resolve {target}: {e}"))?
        .find(SocketAddr::is_ipv4)
        .ok_or_else(|| format!("no IPv4 address for {target}"))?;
    if Some(addr) == known {
        return Err(format!("{target} still resolves to {addr}, which didn't answer"));
    }
    dial_addr(inner, endpoint, peer, addr).await
}

async fn dial_addr(inner: &Arc<Inner>, endpoint: &Endpoint, peer: PeerId, addr: SocketAddr) -> Result<(), String> {
    let connecting = endpoint.connect(addr, tls::SERVER_NAME).map_err(|e| e.to_string())?;
    let conn = tokio::time::timeout(DIAL_TIMEOUT, connecting)
        .await
        .map_err(|_| "connect timeout".to_string())?
        .map_err(|e| e.to_string())?;

    let fp = peer_fingerprint(&conn).unwrap_or_default();
    if let Err(e) = authorize(inner, peer, &fp) {
        reject(inner, &conn, e.clone());
        return Err(e);
    }
    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
    write_frame(&mut send, &hello(inner)).await?;
    let msg = match tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut recv)).await {
        Ok(Ok(msg)) => msg,
        Ok(Err(e)) => {
            // Refused by the other side (its role, a changed identity, ...):
            // say why, once, instead of retrying silently.
            if let Some(quinn::ConnectionError::ApplicationClosed(c)) = conn.close_reason()
                && c.error_code == VarInt::from_u32(CLOSE_REJECTED)
            {
                let name = &inner.cfg.peers[peer].name;
                let reason = format!("{name} refused the connection: {}", String::from_utf8_lossy(&c.reason));
                if inner.refused_roles.lock().unwrap().insert(name.to_lowercase()) {
                    log::warn!("{reason}");
                    emit(inner, NetEvent::Rejected { reason: reason.clone() });
                }
                return Err(reason);
            }
            return Err(e);
        }
        Err(_) => return Err("hello timeout".into()),
    };
    let Message::Hello { version, monitors, os, role, .. } = msg else {
        return Err("expected hello".into());
    };
    if version != PROTOCOL_VERSION {
        let e = format!("peer speaks protocol v{version}, we speak v{PROTOCOL_VERSION}");
        reject(inner, &conn, e.clone());
        return Err(e);
    }
    let name = &inner.cfg.peers[peer].name;
    if let Err(reason) = inner.cfg.access.check(name, conn.remote_address().ip()) {
        return Err(refuse_once(inner, &conn, name, format!("security: {reason}")));
    }
    if !inner.cfg.role.connects_to(role) {
        return Err(refuse_role(inner, &conn, name, role));
    }
    adopt(inner.clone(), peer, conn, send, recv, true, monitors, os, fp);
    Ok(())
}

/// Install a freshly authenticated connection in the peer's slot, resolving
/// duplicates, and start its reader/writer tasks.
#[allow(clippy::too_many_arguments)]
fn adopt(
    inner: Arc<Inner>,
    peer: PeerId,
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
    initiated_by_me: bool,
    monitors: Vec<Monitor>,
    os: Os,
    peer_fp: String,
) {
    if inner.closing.load(Acquire) || inner.paused.load(Acquire) {
        return conn.close(VarInt::from_u32(CLOSE_GOODBYE), b"closing");
    }
    let id = inner.next_id.fetch_add(1, Relaxed);
    let (tx, rx) = mpsc::unbounded_channel();
    {
        let mut slot = inner.slots[peer].lock().unwrap();
        if let Some(existing) = slot.as_ref() {
            // Same initiator: the old one is stale (e.g. peer restarted).
            // Different initiators (both dialled at once): the side with the
            // smaller fingerprint is the preferred dialler. Both ends agree.
            let i_prefer_mine = inner.my_fp < peer_fp;
            let keep_new = existing.initiated_by_me == initiated_by_me || initiated_by_me == i_prefer_mine;
            if !keep_new {
                drop(slot);
                return conn.close(VarInt::from_u32(CLOSE_REPLACED), b"duplicate");
            }
        }
        // Emit under the lock: see `reader`.
        match slot.replace(Live { id, conn: conn.clone(), tx, initiated_by_me }) {
            Some(old) => {
                log::debug!("replaced duplicate connection to '{}'", inner.cfg.peers[peer].name);
                old.conn.close(VarInt::from_u32(CLOSE_REPLACED), b"replaced");
            }
            None => {
                log::info!(
                    "connected to '{}' at {} ({})",
                    inner.cfg.peers[peer].name,
                    conn.remote_address(),
                    if initiated_by_me { "outgoing" } else { "incoming" }
                );
                emit(&inner, NetEvent::Connected { peer, monitors, os });
            }
        }
    }
    inner.slot_changed[peer].notify_waiters();

    // Remember where this peer is: the next dial goes there directly. Our
    // endpoint dials from its listening port, so this address is dialable
    // for incoming connections too.
    let remote = conn.remote_address();
    if remote.is_ipv4() {
        let mut known = inner.known[peer].lock().unwrap();
        if *known != Some(remote) {
            *known = Some(remote);
            emit(&inner, NetEvent::PeerAddress { peer, addr: remote });
        }
    }

    let last_rx = Arc::new(AtomicU64::new(0));
    let epoch = Instant::now();
    // Input and control outrank bulk transfers (clipboard) on the wire.
    let _ = send.set_priority(PRIORITY_CONTROL);
    tokio::spawn(bulk_reader(inner.clone(), peer, conn.clone()));
    tokio::spawn(file_server(inner.clone(), conn.clone()));
    tokio::spawn(writer(inner.clone(), conn.clone(), send, rx, last_rx.clone(), epoch));
    tokio::spawn(reader(inner, peer, id, conn, recv, last_rx, epoch));
}

async fn writer(
    inner: Arc<Inner>,
    conn: Connection,
    mut send: SendStream,
    mut rx: mpsc::UnboundedReceiver<Message>,
    last_rx: Arc<AtomicU64>,
    epoch: Instant,
) {
    let mut beat = tokio::time::interval(inner.cfg.heartbeat);
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let timeout_ms = inner.cfg.timeout.as_millis() as u64;
    let mut seq = 0u32;
    let mut buf = Vec::new();
    loop {
        tokio::select! {
            msg = rx.recv() => {
                let Some(msg) = msg else { return };
                // Batch whatever else is queued into one write.
                buf.clear();
                buf.push(msg);
                while let Ok(m) = rx.try_recv() {
                    buf.push(m);
                    if buf.len() >= 256 { break; }
                }
                for m in &buf {
                    if write_frame(&mut send, m).await.is_err() {
                        return;
                    }
                }
            }
            _ = beat.tick() => {
                let silent = epoch.elapsed().as_millis() as u64 - last_rx.load(Relaxed);
                if silent > timeout_ms {
                    conn.close(VarInt::from_u32(CLOSE_TIMEOUT), b"heartbeat timeout");
                    return;
                }
                seq = seq.wrapping_add(1);
                if write_frame(&mut send, &Message::Heartbeat { seq }).await.is_err() {
                    return;
                }
            }
            _ = conn.closed() => return,
        }
    }
}

async fn reader(
    inner: Arc<Inner>,
    peer: PeerId,
    id: u64,
    conn: Connection,
    mut recv: RecvStream,
    last_rx: Arc<AtomicU64>,
    epoch: Instant,
) {
    let reason = loop {
        match read_frame(&mut recv).await {
            Ok(msg) => {
                last_rx.store(epoch.elapsed().as_millis() as u64, Relaxed);
                match msg {
                    Message::Heartbeat { .. } | Message::HeartbeatAck { .. } => {}
                    Message::Goodbye { reason } => break format!("goodbye: {reason:?}"),
                    msg => emit(&inner, NetEvent::Message { peer, msg }),
                }
            }
            Err(_) => {
                break match conn.close_reason() {
                    Some(quinn::ConnectionError::ApplicationClosed(c)) => {
                        let code = c.error_code.into_inner();
                        let why = String::from_utf8_lossy(&c.reason).into_owned();
                        match code as u32 {
                            CLOSE_GOODBYE => format!("goodbye: {why}"),
                            CLOSE_REPLACED => format!("replaced: {why}"),
                            _ => format!("closed ({code}): {why}"),
                        }
                    }
                    Some(quinn::ConnectionError::LocallyClosed) => "closed locally".into(),
                    Some(e) => e.to_string(),
                    None => "stream ended".into(),
                };
            }
        }
    };
    conn.close(VarInt::from_u32(CLOSE_NORMAL), reason.as_bytes());

    // Only report a disconnect if this connection still owns the slot (a
    // replacement may already have taken over).
    // Events are emitted under the slot lock so their order always matches
    // the order of slot changes (Connected/Disconnected never swap).
    let mut slot = inner.slots[peer].lock().unwrap();
    if slot.as_ref().is_some_and(|l| l.id == id) {
        *slot = None;
        log::info!("disconnected from '{}': {reason}", inner.cfg.peers[peer].name);
        emit(&inner, NetEvent::Disconnected { peer, reason });
        drop(slot);
        inner.slot_changed[peer].notify_waiters();
    }
}

/// Bulk stream: `lz4(size-prepended postcard(Message))`, one message per
/// unidirectional stream.
async fn send_bulk_on(conn: &Connection, msg: &Message) -> Result<usize, String> {
    let raw = desklink_proto::encode(msg).map_err(|e| e.to_string())?;
    let packed = lz4_flex::compress_prepend_size(&raw);
    let mut s = conn.open_uni().await.map_err(|e| e.to_string())?;
    let _ = s.set_priority(PRIORITY_BULK);
    s.write_all(&packed).await.map_err(|e| e.to_string())?;
    s.finish().map_err(|e| e.to_string())?;
    Ok(packed.len())
}

async fn bulk_reader(inner: Arc<Inner>, peer: PeerId, conn: Connection) {
    let max = inner.cfg.max_bulk_bytes;
    while let Ok(mut r) = conn.accept_uni().await {
        let inner = inner.clone();
        tokio::spawn(async move {
            // Compressed data is never larger than raw + small overhead.
            let packed = match r.read_to_end(max + max / 255 + 16).await {
                Ok(p) => p,
                Err(e) => return log::warn!("bulk receive: {e}"),
            };
            let declared = packed.get(..4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize);
            if declared.is_none_or(|n| n > max) {
                return log::warn!("bulk message too large ({declared:?} bytes, limit {max}); dropped");
            }
            let msg = lz4_flex::decompress_size_prepended(&packed)
                .map_err(|e| e.to_string())
                .and_then(|raw| desklink_proto::decode(&raw).map_err(|e| e.to_string()));
            match msg {
                Ok(msg) => emit(&inner, NetEvent::Message { peer, msg }),
                Err(e) => log::warn!("bulk decode: {e}"),
            }
        });
    }
}

// ---------------------------------------------------------------------------
// File transfer: one bidirectional stream per file (or resumed range).
// ---------------------------------------------------------------------------

/// No data from the peer for this long fails the read (and so the copy)
/// instead of hanging Explorer or our progress dialog.
const FILE_READ_TIMEOUT: Duration = Duration::from_secs(15);

async fn write_obj<T: serde::Serialize>(s: &mut SendStream, v: &T) -> Result<(), String> {
    let bytes = postcard::to_stdvec(v).map_err(|e| e.to_string())?;
    let mut buf = (bytes.len() as u16).to_le_bytes().to_vec();
    buf.extend_from_slice(&bytes);
    s.write_all(&buf).await.map_err(|e| e.to_string())
}

async fn read_obj<T: serde::de::DeserializeOwned>(r: &mut RecvStream) -> Result<T, String> {
    let mut len = [0u8; 2];
    r.read_exact(&mut len).await.map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; u16::from_le_bytes(len) as usize];
    r.read_exact(&mut buf).await.map_err(|e| e.to_string())?;
    postcard::from_bytes(&buf).map_err(|e| e.to_string())
}

async fn file_server(inner: Arc<Inner>, conn: Connection) {
    while let Ok((send, recv)) = conn.accept_bi().await {
        tokio::spawn(serve_file(inner.clone(), send, recv));
    }
}

async fn serve_file(inner: Arc<Inner>, mut send: SendStream, mut recv: RecvStream) {
    let _ = send.set_priority(PRIORITY_BULK);
    let Ok(Ok(req)) = tokio::time::timeout(HELLO_TIMEOUT, read_obj::<FileRequest>(&mut recv)).await else {
        return;
    };
    let path = {
        let offers = inner.offers.lock().unwrap();
        match offers.iter().find(|(id, _)| *id == req.offer) {
            Some((_, paths)) => match paths.get(req.index as usize) {
                Some(Some(p)) => Ok(p.clone()),
                Some(None) => Err("not a file".to_string()),
                None => Err("no such entry".to_string()),
            },
            None => Err("files are no longer offered (copied something else since)".to_string()),
        }
    };
    let opened = match path {
        Ok(p) => open_at(&p, req.offset).await,
        Err(e) => Err(e),
    };
    match opened {
        Ok((mut file, remaining)) => {
            if write_obj(&mut send, &FileResponse::Ok { remaining }).await.is_err() {
                return;
            }
            match send_verified(&mut file, remaining, &mut send).await {
                Ok(()) => {
                    let _ = send.finish();
                }
                Err(SendFail::SourceChanged) => {
                    log::warn!("file changed while being sent; transfer aborted: {:?}", req);
                    let _ = send.reset(VarInt::from_u32(RESET_SOURCE_CHANGED));
                }
                // Receiver cancelled (stopped the stream) or the link died.
                Err(SendFail::Io(e)) => log::debug!("file send ended: {e}"),
            }
        }
        Err(e) => {
            log::warn!("file request {:?} refused: {e}", req);
            let _ = write_obj(&mut send, &FileResponse::Err(e)).await;
            let _ = send.finish();
        }
    }
}

async fn open_at(path: &std::path::Path, offset: u64) -> Result<(tokio::fs::File, u64), String> {
    use tokio::io::AsyncSeekExt;
    let mut f = tokio::fs::File::open(path).await.map_err(|e| format!("{}: {e}", path.display()))?;
    let len = f.metadata().await.map_err(|e| e.to_string())?.len();
    let offset = offset.min(len);
    f.seek(std::io::SeekFrom::Start(offset)).await.map_err(|e| e.to_string())?;
    Ok((f, len - offset))
}

/// Fetches offered files from peers. Cheap to clone; blocking API.
#[derive(Clone)]
pub struct FileClient {
    inner: Arc<Inner>,
    rt: tokio::runtime::Handle,
}

impl FileClient {
    /// Open entry `index` of `offer` on `peer`, starting at `offset`.
    /// Must not be called from a tokio runtime thread.
    pub fn open(&self, peer: PeerId, offer: u64, index: u32, offset: u64) -> Result<FileReader, String> {
        let conn = self
            .inner
            .slots
            .get(peer)
            .and_then(|s| s.lock().unwrap().as_ref().map(|l| l.conn.clone()))
            .ok_or("not connected")?;
        let (recv, remaining) = self.rt.block_on(async move {
            let (mut send, mut recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
            let _ = send.set_priority(PRIORITY_BULK);
            write_obj(&mut send, &FileRequest { offer, index, offset }).await?;
            let _ = send.finish();
            match tokio::time::timeout(FILE_READ_TIMEOUT, read_obj::<FileResponse>(&mut recv)).await {
                Ok(Ok(FileResponse::Ok { remaining })) => Ok((recv, remaining)),
                Ok(Ok(FileResponse::Err(e))) => Err(e),
                Ok(Err(e)) => Err(e),
                Err(_) => Err("peer did not answer".to_string()),
            }
        })?;
        let mut reader = FileReader { recv, remaining, rt: self.rt.clone(), hasher: blake3::Hasher::new() };
        if remaining == 0 {
            reader.verify_trailer().map_err(|e| e.to_string())?;
        }
        Ok(reader)
    }
}

/// Blocking reader over a file stream. Dropping it cancels the transfer.
pub struct FileReader {
    recv: RecvStream,
    remaining: u64,
    rt: tokio::runtime::Handle,
    hasher: blake3::Hasher,
}

impl FileReader {
    pub fn remaining(&self) -> u64 {
        self.remaining
    }
}

impl FileReader {
    /// After the last data byte: the sender's hash of what it sent must match
    /// what we received.
    fn verify_trailer(&mut self) -> std::io::Result<()> {
        let recv = &mut self.recv;
        let mut expected = [0u8; 32];
        let r = self.rt.block_on(async {
            tokio::time::timeout(FILE_READ_TIMEOUT, recv.read_exact(&mut expected)).await
        });
        match r {
            Ok(Ok(())) if *self.hasher.finalize().as_bytes() == expected => Ok(()),
            Ok(Ok(())) => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "checksum mismatch: file corrupted in transfer")),
            Ok(Err(e)) => Err(read_error(e.to_string())),
            Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "peer stopped sending")),
        }
    }
}

fn read_error(e: String) -> std::io::Error {
    if e.contains(&format!("reset by peer: error {RESET_SOURCE_CHANGED}")) {
        std::io::Error::other("the file was modified on the other PC while it was being copied")
    } else {
        std::io::Error::other(e)
    }
}

impl std::io::Read for FileReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 || buf.is_empty() {
            return Ok(0);
        }
        let want = buf.len().min(self.remaining as usize);
        let recv = &mut self.recv;
        let r = self.rt.block_on(async { tokio::time::timeout(FILE_READ_TIMEOUT, recv.read(&mut buf[..want])).await });
        match r {
            Ok(Ok(Some(n))) => {
                self.hasher.update(&buf[..n]);
                self.remaining -= n as u64;
                if self.remaining == 0 {
                    self.verify_trailer()?;
                }
                Ok(n)
            }
            Ok(Ok(None)) => Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "file stream ended early")),
            Ok(Err(e)) => Err(read_error(e.to_string())),
            Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "peer stopped sending")),
        }
    }
}

/// Stream reset code: the source file changed while it was being sent.
const RESET_SOURCE_CHANGED: u32 = 1;

enum SendFail {
    SourceChanged,
    Io(String),
}

/// Send `remaining` bytes of `file`, then a BLAKE3 hash of exactly those
/// bytes. Refuses to vouch for the data if the file changed meanwhile.
async fn send_verified(file: &mut tokio::fs::File, remaining: u64, send: &mut SendStream) -> Result<(), SendFail> {
    use tokio::io::AsyncReadExt;
    let fingerprint = |m: std::fs::Metadata| (m.len(), m.modified().ok());
    let before = file.metadata().await.map(fingerprint).map_err(|e| SendFail::Io(e.to_string()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 256 << 10];
    let mut left = remaining;
    while left > 0 {
        let want = buf.len().min(left as usize);
        let n = file.read(&mut buf[..want]).await.map_err(|e| SendFail::Io(e.to_string()))?;
        if n == 0 {
            return Err(SendFail::SourceChanged); // truncated under us
        }
        hasher.update(&buf[..n]);
        send.write_all(&buf[..n]).await.map_err(|e| SendFail::Io(e.to_string()))?;
        left -= n as u64;
    }
    let after = file.metadata().await.map(fingerprint).map_err(|e| SendFail::Io(e.to_string()))?;
    if before != after {
        return Err(SendFail::SourceChanged);
    }
    send.write_all(hasher.finalize().as_bytes()).await.map_err(|e| SendFail::Io(e.to_string()))
}
