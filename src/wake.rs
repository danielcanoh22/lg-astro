//! Lets worker threads end the SDL thread's idle wait as soon as a result is queued,
//! instead of leaving it for the next timed wake.
use sdl2::event::{Event, EventSender};
use std::sync::Mutex;

static SENDER: Mutex<Option<(EventSender, u32)>> = Mutex::new(None);

/// Registers the SDL user event that workers push. Until this runs, `wake` does nothing.
pub fn install(sender: EventSender, event_type: u32) {
    if let Ok(mut slot) = SENDER.lock() {
        *slot = Some((sender, event_type));
    }
}

/// Call after queuing a result for the SDL thread. The event carries no data: the loop
/// drains its channels on every turn.
pub fn wake() {
    if let Ok(slot) = SENDER.lock()
        && let Some((sender, event_type)) = slot.as_ref()
    {
        let _ = sender.push_event(Event::User {
            timestamp: 0,
            window_id: 0,
            type_: *event_type,
            code: 0,
            data1: std::ptr::null_mut(),
            data2: std::ptr::null_mut(),
        });
    }
}
