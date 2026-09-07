//! One HTTPS GET, through Windows's own HTTP stack.
//!
//! hop needs exactly one network call that is not its own protocol: the
//! self-updater asking GitHub what the newest release is, and then
//! downloading it. That could have been a Rust HTTP client, and was
//! briefly, but every Rust TLS backend compiles C, and this project's
//! Windows checks are run by cross-compiling from a Mac (see
//! `CLAUDE.md`), where that C build fails. WinHTTP is already on every
//! Windows machine, needs no dependency beyond the `windows-sys` this
//! crate already has, and cross-compiles because it is nothing but
//! declarations.
//!
//! Deliberately not a general HTTP client. It does one GET over TLS and
//! reads the whole body into memory, because that is all the updater
//! needs and anything more is surface area for no gain. Redirects are
//! followed by WinHTTP itself, which matters: a GitHub release asset URL
//! always redirects to a storage host.

use std::ffi::c_void;
use windows_sys::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryDataAvailable,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, INTERNET_DEFAULT_HTTPS_PORT,
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE,
};

/// Closes a WinHTTP handle when it goes out of scope, on every path
/// including the early returns below. Hand-written rather than one more
/// dependency, and small enough to be obviously correct.
struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the handle is non-null and was returned by one of
            // the WinHttp open calls below, each of which documents
            // WinHttpCloseHandle as the way to release it. Every Handle
            // is owned by exactly one binding, so this cannot double
            // close.
            unsafe {
                WinHttpCloseHandle(self.0);
            }
        }
    }
}

/// UTF-16, null terminated, which is what every `*W` Windows API wants.
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Fetches `https://{host}{path}` and returns the body.
///
/// `limit` caps how much is read, so a hostile or broken server cannot
/// make hop allocate without bound. `user_agent` is required rather than
/// optional because GitHub rejects requests that do not send one.
pub fn get(host: &str, path: &str, user_agent: &str, limit: usize) -> Result<Vec<u8>, String> {
    let (host_w, path_w, agent_w) = (wide(host), wide(path), wide(user_agent));

    // SAFETY: every pointer below is a null terminated UTF-16 buffer that
    // lives in this function for longer than the call it is passed to,
    // and each returned handle is immediately wrapped in `Handle` so it
    // is closed on every path out. The WINHTTP_NO_* nulls are what the
    // documentation specifies for "no proxy list" and "no additional
    // headers", not oversights.
    unsafe {
        let session = Handle(WinHttpOpen(
            agent_w.as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            std::ptr::null(),
            std::ptr::null(),
            0,
        ));
        if session.0.is_null() {
            return Err("could not start an HTTP session".into());
        }

        let connection = Handle(WinHttpConnect(
            session.0,
            host_w.as_ptr(),
            INTERNET_DEFAULT_HTTPS_PORT,
            0,
        ));
        if connection.0.is_null() {
            return Err(format!("could not connect to {host}"));
        }

        let request = Handle(WinHttpOpenRequest(
            connection.0,
            wide("GET").as_ptr(),
            path_w.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ));
        if request.0.is_null() {
            return Err(format!("could not open a request for {path}"));
        }

        if WinHttpSendRequest(request.0, std::ptr::null(), 0, std::ptr::null(), 0, 0, 0) == 0 {
            return Err(format!("could not send the request to {host}"));
        }
        if WinHttpReceiveResponse(request.0, std::ptr::null_mut()) == 0 {
            return Err(format!("no reply from {host}"));
        }

        let mut body: Vec<u8> = Vec::new();
        loop {
            let mut available: u32 = 0;
            if WinHttpQueryDataAvailable(request.0, &mut available) == 0 {
                return Err("the connection failed part way through the reply".into());
            }
            if available == 0 {
                break;
            }
            if body.len().saturating_add(available as usize) > limit {
                return Err(format!("the reply is larger than the {limit} byte limit"));
            }

            let start = body.len();
            body.resize(start + available as usize, 0);
            let mut read: u32 = 0;
            // Reading into `body`'s own spare capacity, which was just
            // resized to hold exactly `available` more bytes.
            if WinHttpReadData(
                request.0,
                body.as_mut_ptr().add(start) as *mut c_void,
                available,
                &mut read,
            ) == 0
            {
                return Err("the reply could not be read".into());
            }
            body.truncate(start + read as usize);
        }
        Ok(body)
    }
}
