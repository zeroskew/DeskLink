//! "Paste files from <peer>": confirm, pick a folder, copy with progress.
//! Each paste runs on its own thread: dialogs block, input must not.

use desklink_config::OnConflict;
use desklink_core::files::{self, FileSource};


use desklink_net::{FileClient, PeerId};
use desklink_proto::FileOffer;
use desklink_win::{transfer, ui};
use std::io::Read;
use std::path::PathBuf;

/// Files offered by a peer (the latest clipboard it sent us).
#[derive(Clone, Debug)]
pub struct Incoming {
    pub peer: PeerId,
    pub peer_name: String,
    pub offer: FileOffer,
}

/// Where to suggest saving.
pub enum Dest {
    /// The folder shown by this window if it's Explorer/the desktop,
    /// otherwise Downloads. Capture the window when the hotkey fires.
    InFrontOf(isize),
    Downloads,
}

/// Limits in bytes (0 = unlimited) and what to do with existing files.
#[derive(Clone, Copy)]
pub struct Settings {
    pub max_file: u64,
    pub max_total: u64,
    pub on_conflict: OnConflict,
}

struct NetSource {
    client: FileClient,
    peer: PeerId,
    offer: u64,
}

impl FileSource for NetSource {
    fn open(&self, index: u32) -> Result<Box<dyn Read>, String> {
        Ok(Box::new(self.client.open(self.peer, self.offer, index, 0)?))
    }
}

pub fn nothing_to_paste() {
    std::thread::spawn(|| {
        let _com = ui::com_init();
        ui::show_info("DeskLink", "No files to paste.\n\nCopy files in Explorer on another PC, then come back here.");
    });
}

pub fn spawn_paste(client: FileClient, inc: Incoming, dest: Dest, settings: Settings) {
    std::thread::Builder::new()
        .name("dl-paste".into())
        .spawn(move || {
            let _com = ui::com_init();
            paste(client, inc, dest, settings);
        })
        .expect("spawn paste thread");
}

fn paste(client: FileClient, inc: Incoming, dest: Dest, settings: Settings) {
    let title = format!("DeskLink: files from {}", inc.peer_name);
    if let Err(why) = files::check_limits(&inc.offer, settings.max_file, settings.max_total) {
        ui::show_error(&title, &format!("Not copied: {why}.\n\nChange the limits in desklink.cfg ([clipboard] max_file_mb / max_total_mb)."));
        return;
    }
    let mut folder: PathBuf = match dest {
        Dest::InFrontOf(hwnd) => ui::folder_for_window(hwnd).unwrap_or_else(ui::downloads_folder),
        Dest::Downloads => ui::downloads_folder(),
    };
    let names = list_names(&inc.offer);
    loop {
        let text = format!(
            "Copy {} from {} into:\n\n{}\n\n{}\n\nYes — copy here\nNo — choose another folder\nCancel — don't copy",
            files::summary(&inc.offer),
            inc.peer_name,
            folder.display(),
            names
        );
        match ui::confirm(&title, &text) {
            ui::Confirm::Copy => break,
            ui::Confirm::ChooseFolder => match ui::pick_folder(&folder, "Copy files to…") {
                Some(f) => {
                    folder = f;
                    break;
                }
                None => continue, // back to the question
            },
            ui::Confirm::Cancel => return,
        }
    }

    transfer::copy_with_dialog(transfer::CopyRequest {
        source: Box::new(NetSource { client, peer: inc.peer, offer: inc.offer.id }),
        offer: inc.offer,
        dest: folder,
        peer_name: inc.peer_name,
        conflict: settings.on_conflict.resolution(),
        max_file: settings.max_file,
        max_total: settings.max_total,
    });
}

/// Up to 12 top-level names for the confirmation.
fn list_names(offer: &FileOffer) -> String {
    let top: Vec<&str> = offer.top_level().map(|e| e.path.as_str()).collect();
    let mut s: Vec<String> = top.iter().take(12).map(|n| format!("  • {n}")).collect();
    if top.len() > 12 {
        s.push(format!("  …and {} more", top.len() - 12));
    }
    s.join("\n")
}

