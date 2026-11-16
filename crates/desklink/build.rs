//! Compiles the settings window's UI, embeds the exe icon, stamps the build
//! version (DESKLINK_VERSION, UTC "YYYY.MM.DD-HHMM"; the release tag is "v"
//! + this) and compiles the relauncher (relauncher.rs) that src/update.rs
//! embeds.

use std::path::PathBuf;
use std::process::Command;

/// UTC now as "YYYY.MM.DD-HHMM" (or $DESKLINK_VERSION when set).
fn build_version() -> String {
    if let Ok(v) = std::env::var("DESKLINK_VERSION") {
        return v;
    }
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}.{m:02}.{d:02}-{:02}{:02}", rem / 3600, rem % 3600 / 60)
}

/// Compiles relauncher.rs (std only) into OUT_DIR/relauncher.exe with the
/// same rustc and linker. On failure an empty file is left, and DeskLink
/// reports the relauncher as missing.
fn build_relauncher() {
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("relauncher.exe");
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let mut cmd = Command::new(rustc);
    cmd.args(["--edition=2024", "--crate-type=bin", "--crate-name=relauncher", "-O", "-C", "panic=abort", "-C", "target-feature=+crt-static", "-C", "debuginfo=0", "-C", "strip=symbols"]);
    if let Ok(target) = std::env::var("TARGET") {
        cmd.args(["--target", &target]);
    }
    // The linker pinned in .cargo/config.toml (cargo passes it on).
    if let Ok(linker) = std::env::var("RUSTC_LINKER") {
        cmd.arg("-C").arg(format!("linker={linker}"));
    }
    cmd.arg("-o").arg(&out).arg("relauncher.rs");
    match cmd.status() {
        Ok(s) if s.success() => {}
        other => {
            println!("cargo:warning=couldn't build the relauncher ({other:?}); self-update is disabled");
            let _ = std::fs::write(&out, b"");
        }
    }
}

fn main() {
    slint_build::compile("ui/app.slint").expect("slint compile");
    // The exe icon (Explorer, taskbar, Alt+Tab).
    embed_resource::compile("desklink.rc", embed_resource::NONE).manifest_optional().expect("desklink.rc");
    println!("cargo:rerun-if-changed=../desklink-win/assets/app.ico");
    // A new version stamp whenever any crate's sources change.
    println!("cargo:rerun-if-changed=..");
    println!("cargo:rerun-if-changed=../../Cargo.toml");
    println!("cargo:rerun-if-env-changed=DESKLINK_VERSION");
    println!("cargo:rustc-env=DESKLINK_VERSION={}", build_version());
    println!("cargo:rerun-if-changed=relauncher.rs");
    build_relauncher();
    // Only the settings window uses these: load them when it opens, not in
    // every DeskLink process.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        for dll in ["opengl32.dll", "dwrite.dll", "dwmapi.dll", "imm32.dll", "uxtheme.dll"] {
            println!("cargo:rustc-link-arg-bins=/DELAYLOAD:{dll}");
        }
        println!("cargo:rustc-link-arg-bins=delayimp.lib");
    }
}
