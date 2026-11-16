//! Virtual files: a peer's files on our clipboard as an OLE data object
//! (`FileGroupDescriptorW` + `FileContents`), the way Outlook offers
//! attachments.
//!
//! Pasting is then driven by the target (Explorer's copy engine, Total
//! Commander via the shell): it asks about conflicts *first*, then pulls only
//! the files it keeps, through one progress dialog. Each file is an `IStream`
//! that reads from the network on demand and can resume after a seek.

use crate::clipboard::{Fetch, PeerId};
use desklink_proto::FileOffer;
use std::cell::{Cell, RefCell};
use std::io::Read;
use std::mem::ManuallyDrop;
use windows::Win32::Foundation::{
    DV_E_FORMATETC, E_NOTIMPL, FILETIME, GlobalFree, HGLOBAL, OLE_E_ADVISENOTSUPPORTED, S_FALSE, S_OK,
    STG_E_ACCESSDENIED, STG_E_READFAULT,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows::Win32::System::Com::{
    CoTaskMemAlloc, DATADIR_GET, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC,
    IEnumSTATDATA, ISequentialStream_Impl, IStream, IStream_Impl, LOCKTYPE, STATFLAG, STATFLAG_NONAME, STATSTG,
    STGC, STGMEDIUM, STGMEDIUM_0, STGTY_STREAM, STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET,
    TYMED_HGLOBAL, TYMED_ISTREAM, DVASPECT_CONTENT,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::ReleaseStgMedium;
use windows::Win32::UI::Shell::{
    FD_ATTRIBUTES, FD_FILESIZE, FD_PROGRESSUI, FD_UNICODE, FD_WRITESTIME, FILEDESCRIPTORW, SHCreateStdEnumFmtEtc,
};
use windows_core::{BOOL, HRESULT, PWSTR, Ref, implement};

/// Registered clipboard format ids.
#[derive(Clone, Copy)]
pub struct Formats {
    pub descriptor: u16,
    pub contents: u16,
    pub drop_effect: u16,
}

impl Formats {
    pub fn register() -> Formats {
        use windows_sys::Win32::System::DataExchange::RegisterClipboardFormatW;
        let reg = |s: &str| {
            let w: Vec<u16> = s.encode_utf16().chain(Some(0)).collect();
            unsafe { RegisterClipboardFormatW(w.as_ptr()) as u16 }
        };
        Formats {
            descriptor: reg("FileGroupDescriptorW"),
            contents: reg("FileContents"),
            drop_effect: reg("Preferred DropEffect"),
        }
    }
}

const DROPEFFECT_COPY: u32 = 1;

/// One descriptor slot: which offer entry it is and its relative Windows path.
struct Slot {
    entry: usize,
    path: String,
}

#[implement(IDataObject)]
pub struct RemoteFiles {
    offer: FileOffer,
    peer: PeerId,
    fetch: Fetch,
    fmt: Formats,
    slots: Vec<Slot>,
    /// Other clipboard formats copied alongside the files (text etc.).
    extra: Vec<(u16, Vec<u8>)>,
}

impl RemoteFiles {
    pub fn create(offer: FileOffer, peer: PeerId, fetch: Fetch, fmt: Formats, extra: Vec<(u16, Vec<u8>)>) -> IDataObject {
        // FILEDESCRIPTORW holds at most MAX_PATH-1 characters of relative path.
        let mut slots = Vec::new();
        for (i, e) in offer.entries.iter().enumerate() {
            let path = e.path.replace('/', "\\");
            if path.encode_utf16().count() >= 260 {
                log::warn!("path too long for virtual files, skipped: {}", e.path);
                continue;
            }
            slots.push(Slot { entry: i, path });
        }
        RemoteFiles { offer, peer, fetch, fmt, slots, extra }.into()
    }
}

fn filetime(ms: u64) -> FILETIME {
    let ticks = ms.saturating_mul(10_000) + 116_444_736_000_000_000;
    FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 }
}

pub(crate) fn hglobal(bytes: &[u8]) -> windows_core::Result<HGLOBAL> {
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))?;
        let p = GlobalLock(h) as *mut u8;
        if p.is_null() {
            let _ = GlobalFree(Some(h));
            return Err(windows_core::Error::from_thread());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(h);
        Ok(h)
    }
}

pub(crate) fn medium_hglobal(bytes: &[u8]) -> windows_core::Result<STGMEDIUM> {
    Ok(STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 { hGlobal: hglobal(bytes)? },
        pUnkForRelease: ManuallyDrop::new(None),
    })
}

impl RemoteFiles_Impl {
    /// FILEGROUPDESCRIPTORW: item count followed by the descriptors.
    fn descriptor_bytes(&self) -> Vec<u8> {
        let mut out = (self.slots.len() as u32).to_le_bytes().to_vec();
        for s in &self.slots {
            let e = &self.offer.entries[s.entry];
            let mut d = FILEDESCRIPTORW {
                dwFlags: [FD_ATTRIBUTES, FD_FILESIZE, FD_WRITESTIME, FD_PROGRESSUI, FD_UNICODE].iter().fold(0u32, |a, f| a | f.0 as u32),
                dwFileAttributes: if e.is_dir { FILE_ATTRIBUTE_DIRECTORY.0 } else { FILE_ATTRIBUTE_NORMAL.0 },
                ftLastWriteTime: filetime(e.modified_ms),
                nFileSizeHigh: (e.size >> 32) as u32,
                nFileSizeLow: e.size as u32,
                ..Default::default()
            };
            // Packed struct: build the name locally, then assign it whole.
            let mut name = [0u16; 260];
            for (dst, src) in name.iter_mut().zip(s.path.encode_utf16()) {
                *dst = src;
            }
            d.cFileName = name;
            let raw = unsafe {
                std::slice::from_raw_parts(&d as *const _ as *const u8, size_of::<FILEDESCRIPTORW>())
            };
            out.extend_from_slice(raw);
        }
        out
    }

    fn supports(&self, f: &FORMATETC) -> bool {
        let hg = f.tymed & TYMED_HGLOBAL.0 as u32 != 0;
        (f.cfFormat == self.fmt.descriptor && hg)
            || (f.cfFormat == self.fmt.contents && f.tymed & TYMED_ISTREAM.0 as u32 != 0)
            || (f.cfFormat == self.fmt.drop_effect && hg)
            || (hg && self.extra.iter().any(|(cf, _)| *cf == f.cfFormat))
    }
}

impl IDataObject_Impl for RemoteFiles_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows_core::Result<STGMEDIUM> {
        if let Some(f) = unsafe { pformatetcin.as_ref() } {
            log::debug!("vfiles GetData cf={} lindex={} tymed={}", f.cfFormat, f.lindex, f.tymed);
        }
        let f = unsafe { pformatetcin.as_ref() }.ok_or(DV_E_FORMATETC)?;
        let hg = f.tymed & TYMED_HGLOBAL.0 as u32 != 0;
        if f.cfFormat == self.fmt.descriptor && hg {
            return medium_hglobal(&self.descriptor_bytes());
        }
        if f.cfFormat == self.fmt.drop_effect && hg {
            return medium_hglobal(&DROPEFFECT_COPY.to_le_bytes());
        }
        if f.cfFormat == self.fmt.contents && f.tymed & TYMED_ISTREAM.0 as u32 != 0 {
            let slot = usize::try_from(f.lindex).ok().and_then(|i| self.slots.get(i)).ok_or(DV_E_FORMATETC)?;
            let e = &self.offer.entries[slot.entry];
            if e.is_dir {
                return Err(DV_E_FORMATETC.into());
            }
            let stream: IStream = RemoteStream {
                fetch: self.fetch.clone(),
                peer: self.peer,
                offer: self.offer.id,
                index: slot.entry as u32,
                size: e.size,
                name: e.path.rsplit('/').next().unwrap_or(&e.path).to_string(),
                modified_ms: e.modified_ms,
                pos: Cell::new(0),
                reader: RefCell::new(None),
            }
            .into();
            return Ok(STGMEDIUM {
                tymed: TYMED_ISTREAM.0 as u32,
                u: STGMEDIUM_0 { pstm: ManuallyDrop::new(Some(stream)) },
                pUnkForRelease: ManuallyDrop::new(None),
            });
        }
        if hg && let Some((_, bytes)) = self.extra.iter().find(|(cf, _)| *cf == f.cfFormat) {
            return medium_hglobal(bytes);
        }
        Err(DV_E_FORMATETC.into())
    }

    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        if let Some(f) = unsafe { pformatetc.as_ref() } {
            log::debug!("vfiles QueryGetData cf={} tymed={}", f.cfFormat, f.tymed);
        }
        match unsafe { pformatetc.as_ref() } {
            Some(f) if self.supports(f) => S_OK,
            _ => DV_E_FORMATETC,
        }
    }

    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, pformatetcout: *mut FORMATETC) -> HRESULT {
        if let Some(out) = unsafe { pformatetcout.as_mut() } {
            out.ptd = std::ptr::null_mut();
        }
        E_NOTIMPL
    }

    /// Explorer reports results here ("Performed DropEffect", "Paste
    /// Succeeded", ...). Accept and discard.
    fn SetData(&self, _: *const FORMATETC, pmedium: *const STGMEDIUM, frelease: BOOL) -> windows_core::Result<()> {
        log::debug!("vfiles SetData");
        if frelease.as_bool() && !pmedium.is_null() {
            unsafe { ReleaseStgMedium(pmedium as *mut STGMEDIUM) };
        }
        Ok(())
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> windows_core::Result<IEnumFORMATETC> {
        log::debug!("vfiles EnumFormatEtc {dwdirection}");
        if dwdirection != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        let fe = |cf: u16, lindex: i32, tymed: u32| FORMATETC {
            cfFormat: cf,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex,
            tymed,
        };
        let mut list = vec![
            fe(self.fmt.descriptor, -1, TYMED_HGLOBAL.0 as u32),
            fe(self.fmt.contents, -1, TYMED_ISTREAM.0 as u32),
            fe(self.fmt.drop_effect, -1, TYMED_HGLOBAL.0 as u32),
        ];
        list.extend(self.extra.iter().map(|(cf, _)| fe(*cf, -1, TYMED_HGLOBAL.0 as u32)));
        unsafe { SHCreateStdEnumFmtEtc(&list) }
    }

    fn DAdvise(&self, _: *const FORMATETC, _: u32, _: Ref<IAdviseSink>) -> windows_core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _: u32) -> windows_core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows_core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

/// One file's contents, fetched from the peer as it is read.
#[implement(IStream)]
struct RemoteStream {
    fetch: Fetch,
    peer: PeerId,
    offer: u64,
    index: u32,
    size: u64,
    name: String,
    modified_ms: u64,
    pos: Cell<u64>,
    /// Open network reader and the position it is at.
    reader: RefCell<Option<(Box<dyn Read>, u64)>>,
}

impl ISequentialStream_Impl for RemoteStream_Impl {
    fn Read(&self, pv: *mut core::ffi::c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        log::debug!("vfiles Read {} cb={} pos={}", self.name, cb, self.pos.get());
        let buf = unsafe { std::slice::from_raw_parts_mut(pv as *mut u8, cb as usize) };
        let mut filled = 0usize;
        let mut reader = self.reader.borrow_mut();
        while filled < buf.len() && self.pos.get() < self.size {
            let pos = self.pos.get();
            // (Re)open at the current position (first read, or after a seek).
            if reader.as_ref().is_none_or(|(_, at)| *at != pos) {
                match (self.fetch)(self.peer, self.offer, self.index, pos) {
                    Ok(r) => *reader = Some((r, pos)),
                    Err(e) => {
                        log::warn!("virtual file {}: {e}", self.name);
                        return STG_E_READFAULT;
                    }
                }
            }
            let (r, at) = reader.as_mut().unwrap();
            match r.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => {
                    filled += n;
                    *at += n as u64;
                    self.pos.set(*at);
                }
                Err(e) => {
                    log::warn!("virtual file {}: {e}", self.name);
                    *reader = None;
                    return STG_E_READFAULT;
                }
            }
        }
        if let Some(out) = unsafe { pcbread.as_mut() } {
            *out = filled as u32;
        }
        if filled < buf.len() { S_FALSE } else { S_OK }
    }

    fn Write(&self, _: *const core::ffi::c_void, _: u32, _: *mut u32) -> HRESULT {
        STG_E_ACCESSDENIED
    }
}

impl IStream_Impl for RemoteStream_Impl {
    fn Seek(&self, dlibmove: i64, dworigin: STREAM_SEEK, plibnewposition: *mut u64) -> windows_core::Result<()> {
        let base = match dworigin {
            STREAM_SEEK_SET => 0,
            STREAM_SEEK_CUR => self.pos.get() as i64,
            STREAM_SEEK_END => self.size as i64,
            _ => return Err(E_NOTIMPL.into()),
        };
        let new = (base + dlibmove).max(0) as u64;
        self.pos.set(new);
        if let Some(out) = unsafe { plibnewposition.as_mut() } {
            *out = new;
        }
        Ok(())
    }

    fn SetSize(&self, _: u64) -> windows_core::Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }

    fn CopyTo(&self, _: Ref<IStream>, _: u64, _: *mut u64, _: *mut u64) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Commit(&self, _: &STGC) -> windows_core::Result<()> {
        Ok(())
    }

    fn Revert(&self) -> windows_core::Result<()> {
        Ok(())
    }

    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Stat(&self, pstatstg: *mut STATSTG, grfstatflag: &STATFLAG) -> windows_core::Result<()> {
        let out = unsafe { pstatstg.as_mut() }.ok_or(E_NOTIMPL)?;
        *out = STATSTG {
            r#type: STGTY_STREAM.0 as u32,
            cbSize: self.size,
            mtime: filetime(self.modified_ms),
            ..Default::default()
        };
        if *grfstatflag != STATFLAG_NONAME {
            let wide: Vec<u16> = self.name.encode_utf16().chain(Some(0)).collect();
            unsafe {
                let p = CoTaskMemAlloc(wide.len() * 2) as *mut u16;
                if !p.is_null() {
                    std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
                    out.pwcsName = PWSTR(p);
                }
            }
        }
        Ok(())
    }

    fn Clone(&self) -> windows_core::Result<IStream> {
        Err(E_NOTIMPL.into())
    }
}
