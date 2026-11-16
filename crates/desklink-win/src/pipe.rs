//! Local named pipes between DeskLink processes of this Windows session: the
//! settings window reads the running DeskLink's log through one.

use std::fs::File;
use std::os::windows::io::FromRawHandle;
use std::sync::Arc;
use std::time::Duration;
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_OUTBOUND;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::GetCurrentProcessId;

/// `\\.\pipe\DeskLink-<session>-<name>`: one DeskLink per logged-in user.
pub fn path(name: &str) -> String {
    let mut session = 0u32;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
    format!(r"\\.\pipe\DeskLink-{session}-{name}")
}

/// Serve pipe `name` in the background: each client that connects gets its
/// own thread running `client` with the writing end.
pub fn serve(name: &str, client: impl Fn(File) + Send + Sync + 'static) -> Result<(), String> {
    let path: Vec<u16> = path(name).encode_utf16().chain(Some(0)).collect();
    let client = Arc::new(client);
    std::thread::Builder::new()
        .name("dl-pipe".into())
        .spawn(move || loop {
            let h = unsafe {
                CreateNamedPipeW(
                    path.as_ptr(),
                    PIPE_ACCESS_OUTBOUND,
                    PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    PIPE_UNLIMITED_INSTANCES,
                    64 * 1024,
                    0,
                    0,
                    std::ptr::null(),
                )
            };
            if h == INVALID_HANDLE_VALUE {
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            let connected = unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) } != 0
                || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
            if !connected {
                unsafe { CloseHandle(h) };
                continue;
            }
            let file = unsafe { File::from_raw_handle(h as _) };
            let client = client.clone();
            let _ = std::thread::Builder::new().name("dl-pipe-client".into()).spawn(move || client(file));
        })
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Connect to pipe `name` served by the running DeskLink, for reading.
pub fn connect(name: &str) -> std::io::Result<File> {
    std::fs::OpenOptions::new().read(true).open(path(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};

    #[test]
    fn client_reads_what_the_server_writes() {
        let name = format!("test-{}", std::process::id());
        serve(&name, |mut f| {
            let _ = writeln!(f, "hello");
            let _ = writeln!(f, "world");
        })
        .unwrap();
        // The server thread may not be listening yet.
        let file = (0..50)
            .find_map(|_| connect(&name).ok().or_else(|| {
                std::thread::sleep(Duration::from_millis(20));
                None
            }))
            .expect("connect");
        let lines: Vec<String> = BufReader::new(file).lines().map_while(Result::ok).collect();
        assert_eq!(lines, ["hello", "world"]);
    }
}
