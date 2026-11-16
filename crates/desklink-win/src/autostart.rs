//! Start with Windows. Normally the per-user Run key (no admin rights
//! needed). When DeskLink runs as administrator, a Task Scheduler task with
//! "highest privileges" instead: the Run key can't start a program elevated
//! without asking at every sign-in.

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Where Windows' Startup apps page (and Task Manager) switch entries off.
const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
const NAME: &str = "DeskLink";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Starts at sign-in: registered (and not switched off in Windows' Startup
/// apps), or as an elevated task.
pub fn is_enabled() -> bool {
    run_key_enabled() || task_exists()
}

fn run_key_enabled() -> bool {
    let mut size = 0u32;
    let registered = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, wide(RUN).as_ptr(), wide(NAME).as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), std::ptr::null_mut(), &mut size)
    } == ERROR_SUCCESS;
    registered && !switched_off()
}

fn switched_off() -> bool {
    // 12 bytes; an odd first byte (3) means disabled.
    let mut data = [0u8; 12];
    let mut size = data.len() as u32;
    let found = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, wide(APPROVED).as_ptr(), wide(NAME).as_ptr(), RRF_RT_REG_BINARY, std::ptr::null_mut(), data.as_mut_ptr().cast(), &mut size)
    } == ERROR_SUCCESS;
    found && data[0] & 1 == 1
}

/// `schtasks` with no console window; true if it succeeded.
fn schtasks(args: &[&str]) -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("schtasks.exe")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn task_exists() -> bool {
    schtasks(&["/Query", "/TN", NAME])
}

/// Run `schtasks` with administrator rights (asking, unless we have them).
fn schtasks_as_admin(args: &[String]) -> Result<(), String> {
    if crate::elevation::is_elevated() {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        return if schtasks(&args) { Ok(()) } else { Err("Task Scheduler refused the change".into()) };
    }
    match crate::elevation::run_elevated("schtasks.exe", args, true)? {
        Some(0) => Ok(()),
        Some(code) => Err(format!("Task Scheduler refused the change (code {code})")),
        None => Err("Administrator permission was declined".into()),
    }
}

/// The sign-in task: this exe, for this user only, with highest privileges,
/// no time limit and also on battery (schtasks' defaults would stop it after
/// three days and never start it on battery).
fn task_xml(exe: &str, user: &str) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>DeskLink: one keyboard and mouse for several PCs (as administrator).</Description></RegistrationInfo>
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>{user}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
  </Settings>
  <Actions Context="Author"><Exec><Command>{}</Command></Exec></Actions>
</Task>"#,
        esc(exe),
        user = esc(user),
    )
}

/// Start this exe (in the tray) at sign-in, or not; `elevated`: as
/// administrator (a task; asks for permission once if needed).
pub fn set(enabled: bool, elevated: bool) -> Result<(), String> {
    let name = wide(NAME);
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    if enabled && elevated {
        let user = format!("{}\\{}", std::env::var("USERDOMAIN").unwrap_or_default(), std::env::var("USERNAME").unwrap_or_default());
        // schtasks reads the task definition from a file (UTF-16 with BOM).
        let file = std::env::temp_dir().join("desklink-task.xml");
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(task_xml(&exe.display().to_string(), &user).encode_utf16().flat_map(u16::to_le_bytes));
        std::fs::write(&file, bytes).map_err(|e| e.to_string())?;
        let result = schtasks_as_admin(&["/Create".into(), "/TN".into(), NAME.into(), "/XML".into(), file.display().to_string(), "/F".into()]);
        let _ = std::fs::remove_file(&file);
        result?;
        // One way to start, not two.
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, wide(RUN).as_ptr(), name.as_ptr()) };
        return Ok(());
    }
    if task_exists() {
        schtasks_as_admin(&["/Delete".into(), "/TN".into(), NAME.into(), "/F".into()])?;
    }
    if enabled {
        let command = wide(&format!("\"{}\"", exe.display()));
        let rc = unsafe {
            RegSetKeyValueW(HKEY_CURRENT_USER, wide(RUN).as_ptr(), name.as_ptr(), REG_SZ, command.as_ptr().cast(), (command.len() * 2) as u32)
        };
        if rc != ERROR_SUCCESS {
            return Err(format!("couldn't add DeskLink to Windows startup (error {rc})"));
        }
        // Undo an earlier "Disabled" in Windows' Startup apps.
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, wide(APPROVED).as_ptr(), name.as_ptr()) };
    } else {
        let rc = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, wide(RUN).as_ptr(), name.as_ptr()) };
        if rc != ERROR_SUCCESS && rc != ERROR_FILE_NOT_FOUND {
            return Err(format!("couldn't remove DeskLink from Windows startup (error {rc})"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_runs_elevated_without_limits() {
        let xml = task_xml(r"C:\Work\Desk & Link\desklink.exe", r"DOMAIN\me");
        assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(xml.contains(r"<Command>C:\Work\Desk &amp; Link\desklink.exe</Command>"));
        assert!(xml.contains(r"<UserId>DOMAIN\me</UserId>"));
    }
}
