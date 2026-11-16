# DeskLink handover

State of the project as of 2026-10-31, for continuing work on another PC
(by a person or an AI assistant). Build, install and release steps are in
`README.md`. Source: https://github.com/zeroskew/DeskLink (public).

## What it is

A software KVM in Rust, like Input Director or Synergy: one keyboard and
mouse drive several Windows PCs. The cursor crosses screen edges onto the
other PC. The clipboard (text, images, files) is shared, and files and
folders can be dragged between PCs. Windows only for now; Linux and macOS
are possible later (see "Other platforms").

The PCs it's used on:
- **dpolyakov-dt** ("4K"): a 3840x2160 screen at 175%, with a 1920x1200
  screen at 100% on its right.
- **flashunreal** ("Flash"): one 3840x2160 screen at 175%.
- Both use Total Commander, so file paste and drag and drop are tested
  against it as well as Explorer.

## Workspace layout

| Crate | What's in it |
|---|---|
| `desklink-proto` | Wire messages (postcard). Protocol v3: `Hello{role,..}`, `Enter`, `LeaveRequest{to}`, `Layout{pcs}`, input, clipboard, files |
| `desklink-core` | Platform-free logic: `geometry.rs` (desk geometry, edges, corners), `switch.rs` (the switching state machine, 50 tests), `files.rs` |
| `desklink-net` | QUIC (quinn) links with pinned self-signed identities (`identity.rs`, `tls.rs`), role checks, `access.rs` (Security tab rules), `discovery.rs` (UDP broadcast every 1 s on port+1) |
| `desklink-config` | `desklink.cfg` (TOML next to the exe, written with comments from `KEY_DOCS`) |
| `desklink-win` | Windows layer: capture hooks, inject, cursor hiding, clipboard, virtual files, drag and drop, tray, named pipes, single instance, autostart (Run key / Task Scheduler), elevation |
| `desklink` | The exe: daemon (`main.rs`) + settings GUI (`gui.rs`, `layout.rs`, `ui/app.slint`) + self-update (`update.rs`, `relauncher.rs`) |
| `dl-probe` | Diagnostics tool: displays, captured events, loopback, clipboard and file self-tests |

## How the exe works

- **One windowless exe** (`#![windows_subsystem = "windows"]`). With no
  arguments it's the tray daemon. `desklink settings` is the settings
  window, run as its own process. Commands from a terminal attach to the
  parent console.
- **GUI:** Slint with the FemtoVG renderer, compiled in. GUI-only DLLs are
  delay-loaded (`build.rs`) so the daemon starts fast. The icon is
  embedded from `crates/desklink-win/assets/app.ico`.
- **Talking to the settings window:** the daemon serves its log and a
  pushed layout to it over named pipes ("log", "layout").
- **Live settings:** the daemon checks `desklink.cfg`'s modification time
  every second. `needs_restart()` decides whether it restarts internally
  (network-level changes such as role, peers, port, security) or applies
  the change live (`apply_live`: cursor, layout, hotkeys).
- **Roles:**
  - Peer: connects only to peers. Each PC has its own layout; the layout
    of the PC the cursor is on decides crossings.
  - Master: connects only to clients. It pushes its layout to them.
  - Client: shows the master's layout read-only and adopts the first
    master that connects.
- **Layout:** PCs snap centred on the edge they attach to; nothing floats.
  New PCs start "unplaced" and are dragged from "Your PCs" into a slot.
  `fix_overlaps()` repairs layouts that were saved before a PC's real
  screen size was known.
- **Run as administrator** (default on): relaunches itself elevated with
  `--after <pid>`, because Windows blocks input injection into elevated
  windows from a normal process (UIPI). Side effect: drag and drop from
  normal programs into an elevated DeskLink is blocked by Windows. Start
  with Windows then uses a Task Scheduler task instead of the Run key.
- **Security tab** (default off): allowed networks (CIDR) and allowed PCs
  (name or IP). Checked after the Hello, in both directions; a refusal is
  logged once per PC.
- **Corners:** "Switch at screen corners" is off by default with a 30 px
  corner size. Only outer corners count.
- **Games:** "Leave games holding the mouse" is off by default. While the
  program in front hides the pointer and captures or clips the mouse
  (`desklink-win/src/fullscreen.rs`; Unreal's Game mode, not its UI mode),
  the core doesn't switch out (`Io::game_holds_mouse`) and the daemon clips
  the cursor to the game window's part of its screen (`ClipCursor`, checked every 250 ms, never
  overriding a program's own smaller clip).

## Builds, releases and self-update

- **Build scripts** (same set as AI Terminal): `build.cmd <cargo args>`
  runs cargo inside a VS 2022 developer environment found with vswhere
  (this matters: a newer VS 18 is installed too, and Rust would pick it).
  After a release build it copies `desklink.exe`, `dl-probe.exe` and
  `README.md` into `dist\`, stopping a running DeskLink first and starting
  it again after. `dist.cmd` = release build. `backup.cmd` zips the sources
  into `backups\`. `build_publish_exe.cmd [-y]` builds, zips
  `desklink.exe` as `desklink.zip` and creates a GitHub release tagged
  `v<version>` with `gh`.
- **Version:** `crates/desklink/build.rs` stamps `DESKLINK_VERSION` = UTC
  build time `YYYY.MM.DD-HHMM` (rerun on any change under `crates/`).
  `desklink --version` prints it.
- **Self-update** (`update.rs`, in the tray daemon only): checks
  `api.github.com/repos/zeroskew/DeskLink/releases/latest` a minute after
  start and every `[updates] check_every_hours` (12); a newer tag's
  `desklink.zip` is downloaded to `%LOCALAPPDATA%\DeskLink\update`. Tray
  balloon + "Restart to update" in the tray menu and Settings > Updates.
  Tray "Check for updates" opens the update window (`desklink update`, Slint
  `UpdateWindow`, its own process like the settings window): it asks the
  daemon to check, follows the "update" pipe (moving bar, download
  progress), offers Restart now / Later, and closes when DeskLink exits.
  Balloons alone aren't enough: Windows drops them when notifications are
  off (they are on the dev PC).
  Installs on "Restart to update" (restarts) or tray Exit, not on
  `desklink stop` (build.cmd uses that to copy in a fresh build). The
  embedded relauncher (`relauncher.rs`, compiled by build.rs with the same
  rustc/linker) waits until the exe is unlocked, swaps it, restarts it.
  Builds run from a `target` folder make no automatic checks.
- **Settings window <-> daemon:** the status comes over the "update" pipe
  (`Status::line`); requests go through the named events
  `Local\DeskLinkUpdateCheck` / `...UpdateApply`. All DeskLink events carry a
  medium integrity label (`instance::create_event`), so a normal process
  (a settings window the user opened, `desklink stop` from a terminal) can
  signal an elevated DeskLink.
- The elevated copy is started with SW_HIDE (no console flash), and
  Windows applies that to the first window a process shows. The tray window
  uses it up at start (`ShowWindow(SW_HIDE)`), or the first message box or
  dialog of the daemon stays invisible.
- `attach_parent_console` leaves a stdout that is redirected to a file or
  pipe alone, so `desklink --version` can be captured by scripts.

## Working rules (agreed with the user)

- **Git:** the project is on GitHub (zeroskew/DeskLink, public). Commit and
  push only when asked. `dist\`, `target\` and `backups\` are ignored; the
  identity files (`desklink.key`, `desklink.cert`) and `desklink.cfg` must
  never be committed.
- **Smoke-test after release builds:** launch `desklink.exe` and check it
  starts. Passing tests don't prove the exe starts.
- **Deploying:** `build.cmd build --release` (or `dist.cmd`) stops DeskLink,
  copies straight over `dist\desklink.exe` and starts it again. By hand: stop
  DeskLink, then copy over it (no renaming to .old.exe). Use `desklink.exe stop`, or kill it if it
  won't stop (the user is fine with that). After a kill, restore the system
  cursor, because hiding replaces it.
- **Don't drive the user's desktop:** no automated typing or clicking in
  their windows. Never use `desklink.exe run` for checks. No always-on-top
  windows.
- **README screenshots:** `tools\screenshots.ps1 [-Exe target\debug\desklink.exe]`
  (uses `desklink settings --page <tab>` and a demo config with a throwaway
  identity; never this PC's real config, it's a public repo).
- **Screenshots:** first check the user doesn't have the settings window
  open (single instance brings theirs to the front):
  `Get-Process desklink | Where MainWindowTitle -eq 'DeskLink'`.
- **Config file:** stays next to the exe, with comments. New settings get
  a `KEY_DOCS` entry; a test checks every documented key exists.
- **Style:** match the surrounding code; short doc comments in plain
  language; UI text with tooltips.

## Known issues and gotchas

- There is no `.cargo/config.toml` any more (it pinned one PC's MSVC
  paths). Build through `build.cmd`; plain `cargo` outside a VS 2022
  developer prompt may pick the incomplete VS 18 toolset.
- Total Commander caches exe icons. After an icon change, run
  `ie4uinit.exe -show` and restart TC.
- Configs keep values written by older versions. A changed default (for
  example `corner_size` 15 to 30, `run_as_admin` false to true) only
  reaches new configs. Existing ones must be changed in Settings.
- The local-network suggestion in the Security tab assumes a /24 mask.
- Terminals (Tilted Terminals, Windows Terminal) run what they start in a
  kill-on-close job. The tray app escapes it at start (`instance::escape_job`:
  relaunch through Explorer); otherwise closing the terminal kills DeskLink.

## Possible next steps

- **A Windows service** (SYSTEM, plus an agent per session): input on the
  sign-in and UAC screens, and no drag-and-drop limits from elevation.
  This was discussed; "Run as administrator" was chosen as the first
  step.
- **Live connection status** (dots) in the Devices tab.
- **Other platforms:**
  - Linux X11 has no UIPI-like block.
  - Wayland needs uinput (the input group or a udev rule) or the
    remote-desktop portal.
  - macOS needs the Accessibility permission.
  - The GUI (Slint) already runs on both.

## Testing

`cargo test --workspace`. One net test is ignored, because it needs a
real second machine. Tests never touch the running DeskLink; the stop
test uses its own event name.
