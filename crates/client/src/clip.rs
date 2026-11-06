//! The Windows clipboard, text only.

#![allow(unsafe_code)]

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
    IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};

use crate::cliptext;

/// CF_UNICODETEXT.
const UNICODE_TEXT: u32 = 13;

/// Changes each time anything is copied on Windows.
pub fn sequence() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

/// Another program may hold the clipboard for a moment; try a few times.
fn open(owner: Option<HWND>) -> bool {
    for _ in 0..5 {
        if unsafe { OpenClipboard(owner) }.is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

/// The text on the clipboard, if there is any.
pub fn read() -> Option<String> {
    if unsafe { IsClipboardFormatAvailable(UNICODE_TEXT) }.is_err() || !open(None) {
        return None;
    }
    let text = (|| {
        let handle = unsafe { GetClipboardData(UNICODE_TEXT) }.ok()?;
        let mem = HGLOBAL(handle.0);
        let size = unsafe { GlobalSize(mem) };
        let p = unsafe { GlobalLock(mem) }.cast::<u16>();
        if p.is_null() {
            return None;
        }
        // At most the clipboard limit in UTF-16 units, and never past the
        // memory block.
        let n = (size / 2).min(ferostream_proto::MAX_CLIPBOARD);
        // SAFETY: the block is locked and at least `size` bytes long.
        let units = unsafe { std::slice::from_raw_parts(p, n) };
        let text = cliptext::from_windows(units);
        unsafe {
            let _ = GlobalUnlock(mem);
        }
        Some(text)
    })();
    unsafe {
        let _ = CloseClipboard();
    }
    text
}

/// Puts `text` on the clipboard, owned by `owner`.
pub fn write(owner: HWND, text: &str) -> bool {
    let units = cliptext::to_windows(text);
    if !open(Some(owner)) {
        return false;
    }
    let ok = (|| {
        unsafe { EmptyClipboard() }.ok()?;
        let bytes = units.len() * 2;
        let mem = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) }.ok()?;
        let p = unsafe { GlobalLock(mem) }.cast::<u16>();
        if p.is_null() {
            unsafe {
                let _ = GlobalFree(Some(mem));
            }
            return None;
        }
        // SAFETY: the block was just allocated with room for `units`.
        unsafe {
            std::ptr::copy_nonoverlapping(units.as_ptr(), p, units.len());
            let _ = GlobalUnlock(mem);
        }
        // On success the clipboard owns the memory.
        if unsafe { SetClipboardData(UNICODE_TEXT, Some(HANDLE(mem.0))) }.is_err() {
            unsafe {
                let _ = GlobalFree(Some(mem));
            }
            return None;
        }
        Some(())
    })()
    .is_some();
    unsafe {
        let _ = CloseClipboard();
    }
    ok
}
