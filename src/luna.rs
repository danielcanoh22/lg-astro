//! Makes bounded LS2 (Luna) calls from Astro's own native process (webOS builds only).
//!
//! Each [`call`] registers an anonymous connection, attaches it to a private GLib context
//! that only the calling worker iterates, and waits for one reply. The hub sees the installed
//! native executable and applies its role. `LSCallFromApplication` (the `luna-send-pub -a`
//! path) would require an impersonation privilege Astro lacks. [`subscribe_as_app`] is the one
//! exception: as root, it holds a subscription through the system bus `luna-send -a`, which
//! may act for an app ID.
use crate::app_subscription::{self, AppSubscription};
use libc::{c_char, c_int, c_ulong, c_void};
use serde_json::Value;
use std::{
    ffi::{CStr, CString},
    process::Command,
    ptr, thread,
    time::{Duration, Instant},
};

/// Largest reply payload accepted; longer replies fail instead of being parsed.
const MAX_REPLY: usize = 2 * 1024 * 1024;

/// `LSError`, with the layout from the NDK's public `luna-service2/lunaservice.h`.
///
/// Created by [`Error::new`] and freed on drop, so every instance passed to LS2 is initialized.
#[repr(C)]
struct Error {
    code: c_int,
    message: *mut c_char,
    file: *const c_char,
    line: c_int,
    function: *const c_char,
    padding: *mut c_void,
    magic: c_ulong,
}

#[link(name = "luna-service2")]
unsafe extern "C" {
    fn LSErrorInit(error: *mut Error) -> bool;
    fn LSErrorFree(error: *mut Error);
    fn LSRegister(name: *const c_char, handle: *mut *mut c_void, error: *mut Error) -> bool;
    fn LSUnregister(handle: *mut c_void, error: *mut Error) -> bool;
    fn LSGmainContextAttach(handle: *mut c_void, context: *mut c_void, error: *mut Error) -> bool;
    fn LSCallOneReply(
        handle: *mut c_void,
        uri: *const c_char,
        payload: *const c_char,
        callback: extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> bool,
        userdata: *mut c_void,
        token: *mut c_ulong,
        error: *mut Error,
    ) -> bool;
    fn LSCallCancel(handle: *mut c_void, token: c_ulong, error: *mut Error) -> bool;
    fn LSMessageGetPayload(message: *mut c_void) -> *const c_char;
}

#[link(name = "glib-2.0")]
unsafe extern "C" {
    fn g_main_context_new() -> *mut c_void;
    fn g_main_context_iteration(context: *mut c_void, may_block: c_int) -> c_int;
    fn g_main_context_unref(context: *mut c_void);
}

impl Error {
    /// Returns an error initialized by `LSErrorInit`, ready to pass to any LS2 call.
    fn new() -> Self {
        // SAFETY: the public error structure is zero-initializable; LS2 then
        // initializes its cookie before any other error API uses it.
        unsafe {
            let mut error = std::mem::zeroed();
            LSErrorInit(&mut error);
            error
        }
    }

    /// Formats the LS2 code and the first 240 characters of its message for `stage`.
    fn describe(&self, stage: &str) -> String {
        let detail = if self.message.is_null() {
            String::new()
        } else {
            // SAFETY: LS2 owns this NUL-terminated string until LSErrorFree.
            unsafe { CStr::from_ptr(self.message) }
                .to_string_lossy()
                .chars()
                .take(240)
                .collect()
        };
        format!("LS2 {stage} failed ({}): {detail}", self.code)
    }
}

impl Drop for Error {
    fn drop(&mut self) {
        // SAFETY: every Error was initialized and is freed exactly once.
        unsafe { LSErrorFree(self) };
    }
}

/// Heap slot the reply callback fills; its address is the LS2 call's user data.
#[derive(Default)]
struct Reply {
    result: Option<Result<Value, String>>,
}

/// LS2 reply callback: parses the payload into the [`Reply`] behind `userdata`.
extern "C" fn receive(_handle: *mut c_void, message: *mut c_void, userdata: *mut c_void) -> bool {
    if message.is_null() || userdata.is_null() {
        return true;
    }
    // SAFETY: LS2 invokes this only while this connection's context is iterated
    // on the worker. The heap slot outlives cancellation/unregistration.
    unsafe {
        let reply = &mut *userdata.cast::<Reply>();
        let payload = LSMessageGetPayload(message);
        reply.result = Some(if payload.is_null() {
            Err("Luna reply has no payload".into())
        } else {
            let size = libc::strnlen(payload, MAX_REPLY + 1);
            if size > MAX_REPLY {
                Err("TV response is too large".into())
            } else {
                let bytes = std::slice::from_raw_parts(payload.cast::<u8>(), size);
                serde_json::from_slice(bytes).map_err(|error| format!("Invalid Luna JSON: {error}"))
            }
        });
    }
    true
}

/// An anonymous LS2 registration attached to its own GLib context.
///
/// Dropping it unregisters before releasing the context and the reply slot, which the
/// callback may still reference until then.
struct Connection {
    handle: *mut c_void,
    context: *mut c_void,
    reply: Option<Box<Reply>>,
}

impl Connection {
    /// Registers an anonymous LS2 handle and attaches it to a new private GLib context.
    ///
    /// # Errors
    ///
    /// Returns a description when the context cannot be allocated or LS2 refuses the
    /// registration or attachment.
    fn open() -> Result<Self, String> {
        let mut error = Error::new();
        let mut connection = Self {
            handle: ptr::null_mut(),
            // SAFETY: GLib returns a new context owned by this connection.
            context: unsafe { g_main_context_new() },
            reply: None,
        };
        if connection.context.is_null() {
            return Err("Could not allocate a Luna event context".into());
        }
        // Anonymous registration still authenticates the executable and its LS2
        // role, and avoids colliding with SDL's own app-named registration.
        // SAFETY: output/error pointers are valid; no fake application ID is sent.
        if !unsafe { LSRegister(ptr::null(), &mut connection.handle, &mut error) } {
            return Err(error.describe("registration"));
        }
        // SAFETY: the registered handle and private context stay alive together.
        if !unsafe { LSGmainContextAttach(connection.handle, connection.context, &mut error) } {
            return Err(error.describe("context attachment"));
        }
        Ok(connection)
    }

    /// Sends one call and polls the private context until its reply or `timeout`.
    ///
    /// # Errors
    ///
    /// Returns a message for an invalid URI or payload, a refused submission, a missing,
    /// oversized or non-JSON reply, or a timeout (after cancelling the call).
    fn request(&mut self, uri: &str, payload: &Value, timeout: Duration) -> Result<Value, String> {
        let uri = CString::new(uri).map_err(|_| "Invalid Luna URI")?;
        let payload = CString::new(payload.to_string()).map_err(|_| "Invalid Luna payload")?;
        let mut error = Error::new();
        let mut reply = Box::<Reply>::default();
        let userdata = ptr::from_mut(reply.as_mut()).cast();
        self.reply = Some(reply);
        let mut token = 0;
        // SAFETY: strings and the stable reply allocation outlive this request.
        if !unsafe {
            LSCallOneReply(
                self.handle,
                uri.as_ptr(),
                payload.as_ptr(),
                receive,
                userdata,
                &mut token,
                &mut error,
            )
        } {
            return Err(error.describe("call submission"));
        }
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            // SAFETY: only this worker drives this private context. Iteration is
            // nonblocking, so a missing service cannot hold the UI or this loop.
            unsafe { g_main_context_iteration(self.context, 0) };
            if let Some(result) = self.reply.as_mut().and_then(|reply| reply.result.take()) {
                return result;
            }
            thread::sleep(Duration::from_millis(2));
        }
        let mut cancel_error = Error::new();
        // SAFETY: handle/token belong to this live request. Even a refused cancel
        // leaves its callback allocation alive until unregistering on drop.
        if !unsafe { LSCallCancel(self.handle, token, &mut cancel_error) } {
            eprintln!("{}", cancel_error.describe("timeout cancellation"));
        }
        Err("The TV service timed out".into())
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            let mut error = Error::new();
            // SAFETY: unregister before freeing the context or callback storage.
            if !unsafe { LSUnregister(self.handle, &mut error) } {
                eprintln!("{}", error.describe("unregistration"));
                // A refused unregister can retain the callback. Preserve its
                // context and heap slot rather than leave dangling pointers.
                std::mem::forget(self.reply.take());
                return;
            }
        }
        if !self.context.is_null() {
            // SAFETY: no live LS2 connection can now dispatch on this context.
            unsafe { g_main_context_unref(self.context) };
        }
    }
}

/// Calls a Luna `uri` with `payload` and returns its JSON reply, waiting at most 6.5 s.
///
/// Blocks the calling thread; call it only from a worker, never the SDL thread.
///
/// # Errors
///
/// Returns a readable message when registration, submission or the reply fails or times out.
/// A reply with `returnValue: false` is returned as `Ok`; callers check it.
pub fn call(uri: &str, payload: &Value) -> Result<Value, String> {
    call_with_timeout(uri, payload, Duration::from_millis(6500))
}

/// Like [`call`], with a caller-chosen timeout.
///
/// # Errors
///
/// Same as [`call`].
pub fn call_with_timeout(uri: &str, payload: &Value, timeout: Duration) -> Result<Value, String> {
    Connection::open()?.request(uri, payload, timeout)
}

/// Starts a subscription kept for the life of the process, made as `app_id` by a root
/// `luna-send -a` child, which the system bus lets act for an app.
///
/// Used for the Home manifest staged under LG's app-ID based role, where Astro's own
/// connection cannot be that app. Requires root. Does not wait for the service; see
/// [`app_subscription::subscribe`] for how replies and failures are reported.
///
/// # Errors
///
/// Same as [`app_subscription::subscribe`].
pub fn subscribe_as_app(
    app_id: &str,
    uri: &str,
    payload: &Value,
    timeout: Duration,
    on_message: impl FnMut(Value) + Send + 'static,
    on_end: impl FnOnce(bool) + Send + 'static,
) -> Result<AppSubscription, String> {
    let mut command = Command::new("/usr/bin/luna-send");
    command.args(["-a", app_id, "-i", uri, &payload.to_string()]);
    app_subscription::subscribe(
        command,
        timeout,
        on_message,
        on_end,
        app_subscription::spawn_thread,
    )
}
