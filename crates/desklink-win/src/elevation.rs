//! Running as administrator: needed to send input into windows of programs
//! that run elevated (Task Manager, installers, other KVM tools). Windows
//! blocks input from normal programs into those.

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, INFINITE, OpenProcessToken, WaitForSingleObject};
use windows_sys::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Does this process run with administrator rights?
pub fn is_elevated() -> bool {
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut size = 0u32;
    let ok = unsafe {
        GetTokenInformation(token, TokenElevation, (&mut elevation as *mut TOKEN_ELEVATION).cast(), size_of::<TOKEN_ELEVATION>() as u32, &mut size)
    };
    unsafe { CloseHandle(token) };
    ok != 0 && elevation.TokenIsElevated != 0
}

/// Quote one command-line argument the way Windows programs parse them.
fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut slashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(slashes));
                out.push(c);
                slashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    out
}

/// Start `program` with administrator rights (Windows asks the user).
/// `Ok(None)` if the user said no; with `wait`, the exit code.
pub fn run_elevated(program: &str, args: &[String], wait: bool) -> Result<Option<u32>, String> {
    let verb = wide("runas");
    let file = wide(program);
    let params = wide(&args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" "));
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = 0; // SW_HIDE: DeskLink and schtasks show no console
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        let e = std::io::Error::last_os_error();
        // ERROR_CANCELLED: the user declined the prompt.
        return if e.raw_os_error() == Some(1223) { Ok(None) } else { Err(e.to_string()) };
    }
    let mut code = 0u32;
    if !info.hProcess.is_null() {
        if wait {
            unsafe {
                WaitForSingleObject(info.hProcess, INFINITE);
                windows_sys::Win32::System::Threading::GetExitCodeProcess(info.hProcess, &mut code);
            }
        }
        unsafe { CloseHandle(info.hProcess) };
    }
    Ok(Some(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_quoted_for_windows() {
        assert_eq!(quote("run"), "run");
        assert_eq!(quote(r"C:\Work\Desk Link\desklink.cfg"), r#""C:\Work\Desk Link\desklink.cfg""#);
        assert_eq!(quote(r"a\"), "a\\");
        assert_eq!(quote(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote(""), r#""""#);
    }
}
