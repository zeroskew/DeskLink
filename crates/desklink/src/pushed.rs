//! The layout a master pushed to this client: kept in memory only (never
//! saved) and handed to the settings window through a pipe, one line per
//! layout. An empty line means none (no master connected).

use crossbeam_channel::{Receiver, Sender, unbounded};
use desklink_proto::{LayoutPc, Monitor};
use std::sync::Mutex;

struct Shared {
    line: String,
    followers: Vec<Sender<String>>,
}

static LAYOUT: Mutex<Shared> = Mutex::new(Shared { line: String::new(), followers: Vec::new() });

/// The master sent a new layout (or `[]`: it went away).
pub fn set(pcs: &[LayoutPc]) {
    let line = encode(pcs);
    let mut s = LAYOUT.lock().unwrap_or_else(|e| e.into_inner());
    s.followers.retain(|f| f.send(line.clone()).is_ok());
    s.line = line;
}

/// The current layout line, then every new one.
pub fn follow() -> (String, Receiver<String>) {
    let (tx, rx) = unbounded();
    let mut s = LAYOUT.lock().unwrap_or_else(|e| e.into_inner());
    s.followers.push(tx);
    (s.line.clone(), rx)
}

// name,master,x,y,monitors ; ...   monitors: x:y:w:h:scale/...
pub fn encode(pcs: &[LayoutPc]) -> String {
    pcs.iter()
        .map(|p| {
            let mons: Vec<String> = p.monitors.iter().map(|m| format!("{}:{}:{}:{}:{}", m.x, m.y, m.width, m.height, m.scale_percent)).collect();
            format!("{},{},{},{},{}", p.name.replace([',', ';'], "_"), p.master as u8, p.x, p.y, mons.join("/"))
        })
        .collect::<Vec<_>>()
        .join(";")
}

pub fn decode(line: &str) -> Vec<LayoutPc> {
    line.split(';')
        .filter_map(|pc| {
            let f: Vec<&str> = pc.split(',').collect();
            let [name, master, x, y, mons] = f.as_slice() else { return None };
            let monitors = mons
                .split('/')
                .filter(|m| !m.is_empty())
                .filter_map(|m| {
                    let v: Vec<i32> = m.split(':').filter_map(|n| n.parse().ok()).collect();
                    let [x, y, w, h, s] = v.as_slice() else { return None };
                    Some(Monitor { x: *x, y: *y, width: *w, height: *h, scale_percent: *s as u16, primary: *x == 0 && *y == 0, name: String::new() })
                })
                .collect();
            Some(LayoutPc { name: name.to_string(), master: *master == "1", x: x.parse().ok()?, y: y.parse().ok()?, monitors })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_line_round_trip() {
        let pcs = vec![
            LayoutPc { name: "dpolyakov-dt".into(), master: true, x: 0, y: 0, monitors: vec![Monitor { x: 0, y: 0, width: 3840, height: 2160, scale_percent: 175, primary: true, name: String::new() }] },
            LayoutPc { name: "flashunreal".into(), master: false, x: -3840, y: 0, monitors: vec![] },
        ];
        let back = decode(&encode(&pcs));
        assert_eq!(back, pcs);
        assert!(decode("").is_empty());
    }
}
