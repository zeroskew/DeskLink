# DeskLink

One keyboard and mouse for several Windows PCs, like Input Director or Synergy. The cursor crosses
a screen edge onto the next PC; the clipboard (text, images, files) is shared, and files and
folders can be dragged from one PC to another. Written in Rust.

## Download

**[Download desklink.zip](https://github.com/zeroskew/DeskLink/releases/latest/download/desklink.zip)**
(latest release). Unzip `desklink.exe` into a folder of its own on every PC and run it; there is
no installer. All releases are on the [Releases page](https://github.com/zeroskew/DeskLink/releases).

DeskLink updates itself: it checks GitHub a minute after it starts and then twice a day, downloads
a new release in the background and installs it when you pick **Restart to update** (tray menu, or
Settings > Updates) or exit DeskLink from the tray. Each PC updates its own copy.

## Using it

1. Start `desklink.exe`. It lives in the tray (no window):
   - double-click the icon: settings window
   - right-click: Open / Disable-Enable / Check for updates / Exit
   - icon colour: light blue = connected, dark blue = no PC connected, grey = disabled

   With **Run as administrator** on (the default) Windows asks for admin rights when it starts,
   so the other PC's mouse also works in admin windows (Task Manager, installers).
2. Allow it through Windows Firewall when asked (private networks). It uses UDP port 24801
   (connections) and 24802 (discovery broadcasts).
3. Set up the PCs:
   - **Settings > General > Role**: Peer on all of them (or one Master + Clients)
   - **Devices > Scan network > Add** the other PC (or type its name or IP address)
   - **Layout**: drag the PC from "Your PCs" into a "Drop a PC here" slot

   Each PC is trusted (pinned) on its first connection.

Its settings (`desklink.cfg`, with comments) and its identity (`desklink.key`, `desklink.cert`)
are created next to the exe. Never copy the identity to another PC: each PC needs its own.

Default hotkeys: **LCtrl+LAlt+Pause** takes the keyboard and mouse back to this PC;
**LCtrl+LAlt+V** pastes files copied on another PC into the folder in front of you.

Command line:

    desklink.exe              start in the tray (if it already runs: open the settings window)
    desklink.exe settings     open the settings window
    desklink.exe stop         stop the running DeskLink
    desklink.exe info         this PC's name, identity and peers
    desklink.exe --version    the build version
    desklink.exe add-peer <name> <left|right|top|bottom> [address]

## Building

Needs [Rust](https://rustup.rs) (x86_64-pc-windows-msvc, 1.85+ for edition 2024) and Visual
Studio 2022 or its Build Tools with **Desktop development with C++** (with a Windows 11 SDK).

| Script | What it does |
|---|---|
| `build.cmd <cargo args>` | cargo inside a VS 2022 developer environment (found with vswhere). After a release build it copies `desklink.exe`, `dl-probe.exe` and this README into `dist\`, stopping and restarting a DeskLink that runs from there |
| `dist.cmd` | `build.cmd build --release` |
| `build_publish_exe.cmd [-y]` | release build, `desklink.zip`, GitHub release tagged `v<build version>` (needs `gh`, logged in) |
| `backup.cmd [folder]` | zips the sources and scripts into `backups\` (or the folder) |

    build.cmd build --release          all binaries; dist\ updated
    build.cmd test --workspace         all tests

Use `build.cmd` rather than plain `cargo` where a newer Visual Studio (18) is installed too:
Rust would otherwise pick the newest toolset.

The build version is the UTC build time (`YYYY.MM.DD-HHMM`, stamped by `crates\desklink\build.rs`),
so every build is newer than the last and needs no manual bump. Builds run from a cargo `target`
folder don't check for updates by themselves.

## Workspace

| Crate | What's in it |
|---|---|
| `desklink-proto` | Wire messages (postcard) |
| `desklink-core` | Platform-free logic: desk geometry, the switching state machine, file offers |
| `desklink-net` | QUIC (quinn) links with pinned self-signed identities, access rules, discovery |
| `desklink-config` | `desklink.cfg` (TOML next to the exe, written with comments) |
| `desklink-win` | Windows layer: input hooks, injection, cursor, clipboard, drag and drop, tray, pipes, HTTP |
| `desklink` | The exe: tray daemon, settings window (Slint), self-update |
| `dl-probe` | Diagnostics: displays, captured events, loopback, clipboard and file self-tests |

`HANDOVER.md` has the design notes and working rules.
