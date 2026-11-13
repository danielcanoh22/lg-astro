//! Polls raw SDL events and maps SDL-webOS extension keys before rust-sdl2 sees them.
//!
//! rust-sdl2 converts scancodes into a closed enum and can abort on the TV's extension
//! values, and the TV keymap can leave their keycode at `SDLK_UNKNOWN` (zero). This module
//! reads the raw event, rewrites Home, Back and Exit to known keys and blanks any other
//! scancode outside SDL2's standard range.
use sdl2::{EventPump, event::Event, keyboard::Keycode, sys};
use std::mem::MaybeUninit;

/// Keycode given to the remote's Exit key (webOS scancode 505), outside SDL's own keycodes.
pub const WEBOS_EXIT_KEYCODE: i32 = (1 << 30) | 505;
/// Keycode given to the remote's Home key (webOS scancode 384), outside SDL's own keycodes.
pub const WEBOS_HOME_KEYCODE: i32 = (1 << 30) | 384;

/// Converts a raw SDL event into a rust-sdl2 [`Event`], first rewriting webOS key scancodes
/// when `webos` is set and blanking any scancode that is not a standard SDL2 value.
pub fn decode(mut raw: sys::SDL_Event, webos: bool) -> Event {
    // SAFETY: SDL always initializes the type tag; only keyboard variants have
    // a keysym. Read the scancode as its ABI integer, since native webOS values
    // are not valid discriminants of the host-generated sys::SDL_Scancode enum.
    unsafe {
        if raw.type_ == sys::SDL_EventType::SDL_KEYDOWN as u32
            || raw.type_ == sys::SDL_EventType::SDL_KEYUP as u32
        {
            let scancode = std::ptr::addr_of!(raw.key.keysym.scancode)
                .cast::<u32>()
                .read();
            if webos && scancode == 384 {
                raw.key.keysym.scancode = sys::SDL_Scancode::SDL_SCANCODE_AC_HOME;
                raw.key.keysym.sym = WEBOS_HOME_KEYCODE;
            } else if webos && scancode == 482 {
                raw.key.keysym.scancode = sys::SDL_Scancode::SDL_SCANCODE_AC_BACK;
                raw.key.keysym.sym = i32::from(Keycode::AcBack);
            } else if webos && scancode == 505 {
                raw.key.keysym.scancode = sys::SDL_Scancode::SDL_SCANCODE_ESCAPE;
                raw.key.keysym.sym = WEBOS_EXIT_KEYCODE;
            } else if webos && matches!(scancode, 484 | 485) {
                // Cursor show/hide are notifications, not another physical key
                // cancelling an in-progress Back hold.
                raw.key.keysym.scancode = sys::SDL_Scancode::SDL_SCANCODE_UNKNOWN;
                raw.key.keysym.sym = 0;
            } else if !matches!(scancode, 0 | 4..=129 | 133..=164 | 176..=221 | 224..=231 | 257..=290 | 512)
            {
                // Do not probe Scancode::from_i32 here: rust-sdl2 transmutes
                // into the host enum before checking it and can abort on a TV
                // extension. These are SDL2's standard ABI discriminants.
                raw.key.keysym.scancode = sys::SDL_Scancode::SDL_SCANCODE_UNKNOWN;
            }
        }
    }
    Event::from_ll(raw)
}

/// Returns the next queued event, decoded with [`decode`], without waiting.
///
/// The pump is borrowed only to prove SDL's event subsystem is alive on this thread.
pub fn poll(_pump: &mut EventPump, webos: bool) -> Option<Event> {
    let mut raw = MaybeUninit::uninit();
    // SAFETY: EventPump keeps SDL events initialized on this SDL thread. SDL
    // writes a complete event only when the call succeeds, just as poll_iter.
    unsafe { (sys::SDL_PollEvent(raw.as_mut_ptr()) == 1).then(|| decode(raw.assume_init(), webos)) }
}

/// Waits up to `timeout` milliseconds for the next event, decoded with [`decode`].
pub fn wait(_pump: &mut EventPump, timeout: u32, webos: bool) -> Option<Event> {
    let mut raw = MaybeUninit::uninit();
    // SAFETY: same ownership/initialization contract as poll; preserve webOS
    // keys on the timed-wait path as well as already-queued events.
    unsafe {
        (sys::SDL_WaitEventTimeout(raw.as_mut_ptr(), timeout.min(i32::MAX as u32) as i32) == 1)
            .then(|| decode(raw.assume_init(), webos))
    }
}
