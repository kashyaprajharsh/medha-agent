//! Takes the OSC 11 reply out of the Windows console input queue and puts every other event back.

use std::time::Instant;
use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Console::{
    CONSOLE_SCREEN_BUFFER_INFOEX, GetConsoleMode, GetConsoleScreenBufferInfoEx, GetConsoleWindow,
    GetNumberOfConsoleInputEvents, GetStdHandle, INPUT_RECORD, KEY_EVENT, ReadConsoleInputW,
    STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, WriteConsoleInputW,
};
use windows_sys::Win32::System::Threading::WaitForSingleObject;
use windows_sys::Win32::UI::WindowsAndMessaging::GetClassNameW;

/// Far more than a person types in a quarter second; reading stops past it.
const MOST_EVENTS: usize = 4096;
const READ_BATCH: usize = 64;

pub(super) fn read_reply(deadline: Instant) -> Option<Vec<u8>> {
    let mut queue = Taken::new(console(STD_INPUT_HANDLE)?);
    queue.read_until(deadline);
    // Dropping `queue` after this puts back everything that was not the reply.
    super::reply_range(&queue.bytes).map(|range| queue.bytes[range].to_vec())
}

/// Only conhost shows its colour table; a table behind ConPTY says nothing about the theme.
pub(super) fn console_palette_luma() -> Option<f32> {
    let mut class = [0u16; 32];
    let length =
        unsafe { GetClassNameW(GetConsoleWindow(), class.as_mut_ptr(), class.len() as i32) };
    if length <= 0 || String::from_utf16_lossy(&class[..length as usize]) != "ConsoleWindowClass" {
        return None;
    }
    let output = console(STD_OUTPUT_HANDLE)?;
    let mut info = unsafe { std::mem::zeroed::<CONSOLE_SCREEN_BUFFER_INFOEX>() };
    info.cbSize = std::mem::size_of::<CONSOLE_SCREEN_BUFFER_INFOEX>() as u32;
    if unsafe { GetConsoleScreenBufferInfoEx(output, &mut info) } == 0 {
        return None;
    }
    // COLORREF is 0x00BBGGRR; the background index sits in bits 4 to 7 of the attributes.
    let colour = info.ColorTable[((info.wAttributes >> 4) & 0x0f) as usize];
    let channel = |shift: u32| ((colour >> shift) & 0xff) as f32 / 255.0;
    Some(0.299 * channel(0) + 0.587 * channel(8) + 0.114 * channel(16))
}

fn console(kind: u32) -> Option<HANDLE> {
    let handle = unsafe { GetStdHandle(kind) };
    let mut mode = 0;
    (!handle.is_null()
        && handle != INVALID_HANDLE_VALUE
        && unsafe { GetConsoleMode(handle, &mut mode) } != 0)
        .then_some(handle)
}

struct Taken {
    handle: HANDLE,
    events: Vec<INPUT_RECORD>,
    bytes: Vec<u8>,
    owner: Vec<usize>,
}

impl Taken {
    fn new(handle: HANDLE) -> Self {
        Self {
            handle,
            events: Vec::new(),
            bytes: Vec::new(),
            owner: Vec::new(),
        }
    }

    fn read_until(&mut self, deadline: Instant) {
        while super::reply_range(&self.bytes).is_none() && self.events.len() < MOST_EVENTS {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            let millis = left.as_millis().clamp(1, u32::MAX as u128) as u32;
            if unsafe { WaitForSingleObject(self.handle, millis) } != WAIT_OBJECT_0
                || self.read_waiting() == 0
            {
                return;
            }
        }
    }

    fn read_waiting(&mut self) -> usize {
        let mut waiting = 0;
        if unsafe { GetNumberOfConsoleInputEvents(self.handle, &mut waiting) } == 0 || waiting == 0
        {
            return 0;
        }
        let room = MOST_EVENTS.saturating_sub(self.events.len());
        let wanted = (waiting as usize).min(READ_BATCH).min(room);
        if wanted == 0 {
            return 0;
        }
        let mut batch = [unsafe { std::mem::zeroed::<INPUT_RECORD>() }; READ_BATCH];
        let mut read = 0;
        if unsafe { ReadConsoleInputW(self.handle, batch.as_mut_ptr(), wanted as u32, &mut read) }
            == 0
        {
            return 0;
        }
        for event in batch.into_iter().take(read as usize) {
            self.take(event);
        }
        read as usize
    }

    fn take(&mut self, event: INPUT_RECORD) {
        let index = self.events.len();
        self.events.push(event);
        if event.EventType != KEY_EVENT as u16 {
            return;
        }
        let key = unsafe { event.Event.KeyEvent };
        let unit = unsafe { key.uChar.UnicodeChar };
        if key.bKeyDown != 0
            && let Ok(byte) = u8::try_from(unit)
            && byte != 0
            && byte.is_ascii()
        {
            self.bytes.push(byte);
            self.owner.push(index);
        }
    }

    fn kept(&self) -> Vec<INPUT_RECORD> {
        let mut reply = vec![false; self.events.len()];
        if let Some(range) = super::reply_range(&self.bytes) {
            for byte in range {
                reply[self.owner[byte]] = true;
            }
        }
        self.events
            .iter()
            .zip(&reply)
            .filter_map(|(event, &is_reply)| (!is_reply).then_some(*event))
            .collect()
    }

    fn put_back(&mut self) {
        if self.events.is_empty() {
            return;
        }
        // Writing appends to the queue, so what arrived meanwhile is taken first and kept behind.
        while self.events.len() < MOST_EVENTS && self.read_waiting() > 0 {}
        let kept = self.kept();
        let mut written_so_far = 0;
        while written_so_far < kept.len() {
            let rest = &kept[written_so_far..];
            let mut written = 0;
            if unsafe {
                WriteConsoleInputW(self.handle, rest.as_ptr(), rest.len() as u32, &mut written)
            } == 0
                || written == 0
            {
                break;
            }
            written_so_far += written as usize;
        }
        self.events.clear();
    }
}

impl Drop for Taken {
    fn drop(&mut self) {
        self.put_back();
    }
}

#[cfg(test)]
#[path = "termbg_windows_tests.rs"]
mod tests;
