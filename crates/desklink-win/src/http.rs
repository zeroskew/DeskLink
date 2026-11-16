//! A small HTTPS GET over WinHTTP (system proxy settings, redirects
//! followed). Used by the self-update to ask GitHub for the latest release
//! and download it.

use windows::Win32::Networking::WinHttp::*;
use windows::core::{HSTRING, PCWSTR, w};

#[derive(Debug)]
pub enum Error {
    /// The server answered with this HTTP status (not 200).
    Status(u32),
    Net(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::Status(s) => write!(f, "the server answered HTTP {s}"),
            Error::Net(s) => write!(f, "{s}"),
        }
    }
}

struct Handle(*mut core::ffi::c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

fn net(what: &str) -> Error {
    let e = windows::core::Error::from_thread();
    let msg = match e.code().0 as u32 & 0xffff {
        12002 => "the connection timed out".to_string(),
        12007 => "the server name couldn't be resolved (offline?)".to_string(),
        12029 | 12030 => "couldn't connect to the server".to_string(),
        12175 => "a secure connection couldn't be made".to_string(),
        _ => e.message(),
    };
    Error::Net(format!("network error ({what}): {msg}"))
}

/// GET `url` (https only) with this Accept header; `progress(done, total)`
/// while the body arrives (total 0 = unknown).
pub fn get(url: &str, accept: &str, progress: &mut dyn FnMut(u64, u64)) -> Result<Vec<u8>, Error> {
    let rest = url.strip_prefix("https://").ok_or_else(|| Error::Net(format!("not an https URL: {url}")))?;
    let (host, path) = rest.split_once('/').map(|(h, p)| (h, format!("/{p}"))).unwrap_or((rest, "/".into()));
    unsafe {
        let session = Handle(WinHttpOpen(w!("DeskLink-Updater"), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0));
        if session.0.is_null() {
            return Err(net("open"));
        }
        let _ = WinHttpSetTimeouts(session.0, 15_000, 15_000, 30_000, 30_000);
        let conn = Handle(WinHttpConnect(session.0, &HSTRING::from(host), 443, 0));
        if conn.0.is_null() {
            return Err(net("connect"));
        }
        let req = Handle(WinHttpOpenRequest(conn.0, w!("GET"), &HSTRING::from(path.as_str()), PCWSTR::null(), PCWSTR::null(), std::ptr::null(), WINHTTP_FLAG_SECURE));
        if req.0.is_null() {
            return Err(net("request"));
        }
        let headers: Vec<u16> = format!("Accept: {accept}\r\nX-GitHub-Api-Version: 2022-11-28\r\n").encode_utf16().collect();
        WinHttpSendRequest(req.0, Some(&headers), None, 0, 0, 0).map_err(|_| net("send"))?;
        WinHttpReceiveResponse(req.0, std::ptr::null_mut()).map_err(|_| net("response"))?;
        let number = |level: u32| -> Option<u64> {
            let mut v: u32 = 0;
            let mut len = 4u32;
            let mut idx = 0u32;
            WinHttpQueryHeaders(req.0, level | WINHTTP_QUERY_FLAG_NUMBER, PCWSTR::null(), Some(&mut v as *mut u32 as _), &mut len, &mut idx)
                .ok()
                .map(|_| v as u64)
        };
        let status = number(WINHTTP_QUERY_STATUS_CODE).unwrap_or(0);
        if status != 200 {
            return Err(Error::Status(status as u32));
        }
        let total = number(WINHTTP_QUERY_CONTENT_LENGTH).unwrap_or(0);
        let mut out = Vec::with_capacity(total as usize);
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut read = 0u32;
            WinHttpReadData(req.0, buf.as_mut_ptr() as _, buf.len() as u32, &mut read).map_err(|_| net("read"))?;
            if read == 0 {
                break;
            }
            out.extend_from_slice(&buf[..read as usize]);
            progress(out.len() as u64, total);
        }
        if total > 0 && (out.len() as u64) < total {
            return Err(Error::Net("the download was cut short".into()));
        }
        Ok(out)
    }
}
