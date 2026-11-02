//! The system clipboard on Windows, through the Win32 API.

use super::{from_clipboard_text, to_clipboard_text};
use std::{
    io::{self, Write},
    ptr, slice,
    time::Duration,
};
use windows_sys::{
    Win32::{
        Foundation::{GlobalFree, HGLOBAL, HWND},
        System::{
            DataExchange::{
                CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
                OpenClipboard, SetClipboardData,
            },
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
            Ole::CF_UNICODETEXT,
        },
        UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, HWND_MESSAGE},
    },
    w,
};

const TEXT: u32 = CF_UNICODETEXT as u32;
/// Another program may hold the clipboard briefly, so opening it tries for up to 100 ms.
const OPEN_ATTEMPTS: u32 = 10;
const OPEN_RETRY: Duration = Duration::from_millis(10);

pub fn read() -> io::Result<String> {
    let _clipboard = Clipboard::open(ptr::null_mut())?;
    // SAFETY: the clipboard stays open for this block, which keeps its data alive, and the
    // data is a locked block of `GlobalSize` bytes, read as whole UTF-16 units.
    unsafe {
        if IsClipboardFormatAvailable(TEXT) == 0 {
            return Ok(String::new());
        }
        let data = GetClipboardData(TEXT);
        if data.is_null() {
            return Err(io::Error::last_os_error());
        }
        let units = GlobalLock(data).cast::<u16>();
        if units.is_null() {
            return Err(io::Error::last_os_error());
        }
        let text = from_clipboard_text(slice::from_raw_parts(units, GlobalSize(data) / 2));
        GlobalUnlock(data);
        Ok(text)
    }
}

/// The clipboard takes one block of memory, so unlike the helpers on other platforms this
/// holds the whole text in memory.
pub fn write_with(write: impl FnOnce(&mut dyn Write) -> io::Result<()>) -> io::Result<()> {
    let mut bytes = Vec::new();
    write(&mut bytes)?;
    let text = std::str::from_utf8(&bytes).map_err(io::Error::other)?;
    let memory = global_copy(&to_clipboard_text(text))?;
    let result = place(memory);
    if result.is_err() {
        // SAFETY: the system only takes ownership when placing the memory succeeds.
        unsafe { GlobalFree(memory) };
    }
    result
}

/// A movable block of global memory holding `units`, as `SetClipboardData` requires.
fn global_copy(units: &[u16]) -> io::Result<HGLOBAL> {
    // SAFETY: the block is allocated for exactly `units`, and is locked while copying.
    unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, size_of_val(units));
        if memory.is_null() {
            return Err(io::Error::last_os_error());
        }
        let target = GlobalLock(memory).cast::<u16>();
        if target.is_null() {
            let error = io::Error::last_os_error();
            GlobalFree(memory);
            return Err(error);
        }
        ptr::copy_nonoverlapping(units.as_ptr(), target, units.len());
        GlobalUnlock(memory);
        Ok(memory)
    }
}

fn place(memory: HGLOBAL) -> io::Result<()> {
    let owner = Owner::new()?;
    let _clipboard = Clipboard::open(owner.0)?;
    // SAFETY: the clipboard is open, and `memory` is a valid unlocked movable block.
    unsafe {
        if EmptyClipboard() == 0 || SetClipboardData(TEXT, memory).is_null() {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The window that owns what is copied. `SetClipboardData` is documented to fail without one,
/// and text stays on the clipboard after its owner is destroyed.
struct Owner(HWND);

impl Owner {
    fn new() -> io::Result<Self> {
        // SAFETY: a hidden message-only window of a built-in class needs no other setup.
        let window = unsafe {
            CreateWindowExW(
                0,
                w!("STATIC"),
                ptr::null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if window.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(window))
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        // SAFETY: this thread created the window, and nothing else holds its handle.
        unsafe { DestroyWindow(self.0) };
    }
}

/// The open clipboard, which other programs cannot use until this is dropped.
struct Clipboard;

impl Clipboard {
    fn open(owner: HWND) -> io::Result<Self> {
        for attempt in 1..=OPEN_ATTEMPTS {
            // SAFETY: `owner` is null or a live window on this thread.
            if unsafe { OpenClipboard(owner) } != 0 {
                return Ok(Self);
            }
            if attempt < OPEN_ATTEMPTS {
                std::thread::sleep(OPEN_RETRY);
            }
        }
        Err(io::Error::last_os_error())
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        // SAFETY: this value exists only while this thread has the clipboard open.
        unsafe { CloseClipboard() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "replaces the contents of the system clipboard"]
    fn text_survives_a_trip_through_the_system_clipboard() {
        let text = "first line\nсекунда 👩‍💻\n";
        write_with(|out| out.write_all(text.as_bytes())).unwrap();
        assert_eq!(read().unwrap(), text);
    }
}
