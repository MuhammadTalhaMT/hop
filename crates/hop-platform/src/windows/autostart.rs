//! Starting hop when Windows starts.
//!
//! Uses the per user `Run` key rather than a service or a scheduled
//! task. hop needs a desktop session to inject input into, so starting
//! before login would be pointless, and a per user key needs no
//! administrator rights to write, which means turning this on is a
//! checkbox rather than an elevation prompt.
//!
//! The value is written under `HKEY_CURRENT_USER`, so it affects only the
//! person who set it, and it survives a self-update: the updater replaces
//! the file at the same path rather than moving it.

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ,
};

/// Where Windows looks for things to start at login.
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// The value name under that key. Stable, so turning autostart off finds
/// what turning it on wrote, including across versions.
const VALUE_NAME: &str = "hop";

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Opens the Run key with the given access, closing it on every path out.
struct RunKey(HKEY);

impl RunKey {
    fn open(access: u32) -> Option<Self> {
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: the subkey name is a null terminated UTF-16 buffer that
        // outlives the call, and `key` is a local the call fills in. A
        // non-success return leaves it untouched, which is why the result
        // is checked before the handle is used.
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                wide(RUN_KEY).as_ptr(),
                0,
                access,
                &mut key,
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        Some(Self(key))
    }
}

impl Drop for RunKey {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from a successful RegOpenKeyExW and is
        // owned solely by this value, so this cannot double close.
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

/// Whether hop is currently set to start with Windows.
pub fn is_enabled() -> bool {
    let Some(key) = RunKey::open(KEY_READ) else {
        return false;
    };
    let mut size: u32 = 0;
    // SAFETY: asking for the size only. A null data pointer with a live
    // size pointer is the documented way to query a value's presence and
    // length without reading it.
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            wide(VALUE_NAME).as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    status == ERROR_SUCCESS
}

/// Sets hop to start with Windows, running `command`.
pub fn enable(command: &str) -> Result<(), String> {
    let key = RunKey::open(KEY_WRITE).ok_or("could not open the Windows startup settings")?;
    let value = wide(command);

    // SAFETY: `value` is a null terminated UTF-16 buffer that outlives
    // the call, and the byte count includes the terminator, which REG_SZ
    // requires: without it, reading the value back walks past the end.
    let status = unsafe {
        RegSetValueExW(
            key.0,
            wide(VALUE_NAME).as_ptr(),
            0,
            REG_SZ,
            value.as_ptr() as *const u8,
            (value.len() * std::mem::size_of::<u16>()) as u32,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "Windows refused the startup entry (error {status})"
        ));
    }
    Ok(())
}

/// Stops hop starting with Windows. Succeeds when there was nothing to
/// remove, so a user turning it off twice sees no error.
pub fn disable() -> Result<(), String> {
    let Some(key) = RunKey::open(KEY_WRITE) else {
        return Ok(());
    };
    // SAFETY: a null terminated value name that outlives the call.
    // Deleting a value that is not there returns a not-found status,
    // which is not a failure worth reporting to someone whose intent was
    // "do not start with Windows".
    unsafe {
        RegDeleteValueW(key.0, wide(VALUE_NAME).as_ptr());
    }
    Ok(())
}

/// The command line to register: this executable, showing its window,
/// with the config it was told to use.
///
/// Quoted because `Program Files` and every other path with a space in it
/// would otherwise be read as several arguments.
pub fn startup_command(exe: &std::path::Path, config: &std::path::Path) -> String {
    format!(
        "\"{}\" gui --config \"{}\"",
        exe.display(),
        config.display()
    )
}
