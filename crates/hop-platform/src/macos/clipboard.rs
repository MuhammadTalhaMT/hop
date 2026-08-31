//! Reading and writing the macOS clipboard, and noticing when it changes.
//!
//! Uses `NSPasteboard` through the Objective-C runtime directly rather
//! than through a binding crate, to keep this project's "no new
//! dependencies" rule. The calls involved are few and stable.
//!
//! Change detection uses `changeCount`, a counter `NSPasteboard` bumps on
//! every write by anyone. Polling that integer is far cheaper than
//! reading the clipboard contents on a timer, and it is how every tool
//! that watches the pasteboard does it: there is no notification API for
//! pasteboard changes.

use std::ffi::CString;
use std::os::raw::{c_char, c_void};

type Id = *mut c_void;

unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Id;
    fn objc_msgSend();
}

/// SAFETY for every helper below: `objc_getClass` and `sel_registerName`
/// return process-lifetime pointers or null, and `objc_msgSend` is
/// transmuted to the exact signature of the selector being sent, which is
/// how the Objective-C runtime is meant to be called from C. Every object
/// passed is either a class object or one obtained from an autoreleased
/// method in the same call, so nothing can dangle within a single call.
unsafe fn class(name: &str) -> Id {
    let c = match CString::new(name) {
        Ok(c) => c,
        Err(_) => return std::ptr::null_mut(),
    };
    unsafe { objc_getClass(c.as_ptr()) }
}

unsafe fn selector(name: &str) -> Id {
    let c = match CString::new(name) {
        Ok(c) => c,
        Err(_) => return std::ptr::null_mut(),
    };
    unsafe { sel_registerName(c.as_ptr()) }
}

unsafe fn general_pasteboard() -> Id {
    unsafe {
        let send: extern "C" fn(Id, Id) -> Id = std::mem::transmute(objc_msgSend as *const ());
        let cls = class("NSPasteboard");
        if cls.is_null() {
            return std::ptr::null_mut();
        }
        send(cls, selector("generalPasteboard"))
    }
}

/// The pasteboard's change counter. Bumped by any process that writes to
/// the clipboard, so a change is "this differs from what we last saw".
/// Returns 0 if the pasteboard cannot be reached, which simply reads as
/// "no change" and disables syncing rather than failing anything.
pub fn change_count() -> i64 {
    unsafe {
        let pb = general_pasteboard();
        if pb.is_null() {
            return 0;
        }
        let send: extern "C" fn(Id, Id) -> i64 = std::mem::transmute(objc_msgSend as *const ());
        send(pb, selector("changeCount"))
    }
}

/// Current clipboard contents as text, or `None` if the clipboard holds
/// something that is not text (an image, a file) or is empty.
pub fn get_text() -> Option<String> {
    unsafe {
        let pb = general_pasteboard();
        if pb.is_null() {
            return None;
        }
        let send_type: extern "C" fn(Id, Id, Id) -> Id =
            std::mem::transmute(objc_msgSend as *const ());
        let ns_string = class("NSString");
        if ns_string.is_null() {
            return None;
        }
        // stringForType: NSPasteboardTypeString, which is the constant
        // string "public.utf8-plain-text".
        let utf8_type = ns_string_from("public.utf8-plain-text")?;
        let value = send_type(pb, selector("stringForType:"), utf8_type);
        if value.is_null() {
            return None;
        }
        rust_string_from(value)
    }
}

/// Replace the clipboard contents with `text`.
///
/// Returns whether it succeeded. A failure is not worth propagating as an
/// error: the worst outcome is that one copy does not appear on the other
/// machine, and the caller logs it.
pub fn set_text(text: &str) -> bool {
    unsafe {
        let pb = general_pasteboard();
        if pb.is_null() {
            return false;
        }
        let clear: extern "C" fn(Id, Id) -> i64 = std::mem::transmute(objc_msgSend as *const ());
        clear(pb, selector("clearContents"));

        let value = match ns_string_from(text) {
            Some(v) => v,
            None => return false,
        };
        let ns_string = class("NSString");
        if ns_string.is_null() {
            return false;
        }
        let utf8_type = match ns_string_from("public.utf8-plain-text") {
            Some(v) => v,
            None => return false,
        };
        let send: extern "C" fn(Id, Id, Id, Id) -> bool =
            std::mem::transmute(objc_msgSend as *const ());
        send(pb, selector("setString:forType:"), value, utf8_type)
    }
}

/// Build an autoreleased `NSString` from a Rust string.
unsafe fn ns_string_from(s: &str) -> Option<Id> {
    unsafe {
        let cls = class("NSString");
        if cls.is_null() {
            return None;
        }
        let c = CString::new(s).ok()?;
        // stringWithUTF8String: copies, so the CString may drop after.
        let send: extern "C" fn(Id, Id, *const c_char) -> Id =
            std::mem::transmute(objc_msgSend as *const ());
        let value = send(cls, selector("stringWithUTF8String:"), c.as_ptr());
        if value.is_null() {
            None
        } else {
            Some(value)
        }
    }
}

/// Copy an `NSString` back out into an owned Rust string.
unsafe fn rust_string_from(value: Id) -> Option<String> {
    unsafe {
        let send: extern "C" fn(Id, Id) -> *const c_char =
            std::mem::transmute(objc_msgSend as *const ());
        let ptr = send(value, selector("UTF8String"));
        if ptr.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr(ptr)
            .to_str()
            .ok()
            .map(String::from)
    }
}

/// The macOS clipboard, as `hop-core` sees it.
pub struct MacClipboard;

impl hop_core::Clipboard for MacClipboard {
    fn change_count(&self) -> i64 {
        change_count()
    }
    fn get_text(&self) -> Option<String> {
        get_text()
    }
    fn set_text(&mut self, text: &str) -> bool {
        set_text(text)
    }
    fn get_file_paths(&self) -> Vec<String> {
        get_file_paths()
    }
    fn set_file_path(&mut self, path: &str) -> bool {
        set_file_path(path)
    }
}

/// Paths of files currently on the clipboard, if it holds files rather
/// than text.
///
/// Reads `NSPasteboardTypeFileURL` and, for the multiple-file case, the
/// pasteboard's item list. Returns an empty vector when the clipboard
/// holds something else, which is the common case.
pub fn get_file_paths() -> Vec<String> {
    unsafe {
        let pb = general_pasteboard();
        if pb.is_null() {
            return Vec::new();
        }
        // propertyListForType: NSFilenamesPboardType returns an NSArray of
        // NSString paths. It is the long standing type for this and is
        // still what Finder puts on the pasteboard for a copied file.
        let Some(kind) = ns_string_from("NSFilenamesPboardType") else {
            return Vec::new();
        };
        let send: extern "C" fn(Id, Id, Id) -> Id = std::mem::transmute(objc_msgSend as *const ());
        let array = send(pb, selector("propertyListForType:"), kind);
        if array.is_null() {
            return Vec::new();
        }
        let count_of: extern "C" fn(Id, Id) -> usize =
            std::mem::transmute(objc_msgSend as *const ());
        let count = count_of(array, selector("count"));
        let at: extern "C" fn(Id, Id, usize) -> Id = std::mem::transmute(objc_msgSend as *const ());
        let mut out = Vec::new();
        for i in 0..count {
            let item = at(array, selector("objectAtIndex:"), i);
            if item.is_null() {
                continue;
            }
            if let Some(path) = rust_string_from(item) {
                out.push(path);
            }
        }
        out
    }
}

/// Put a file on the clipboard, so pasting in Finder copies it wherever
/// the user pastes.
///
/// The file must already exist at `path`: the clipboard holds a reference,
/// not the contents, which is exactly what makes "paste it where I want
/// it" work without hop needing to know the destination.
pub fn set_file_path(path: &str) -> bool {
    unsafe {
        let pb = general_pasteboard();
        if pb.is_null() {
            return false;
        }
        let clear: extern "C" fn(Id, Id) -> i64 = std::mem::transmute(objc_msgSend as *const ());
        clear(pb, selector("clearContents"));

        let Some(kind) = ns_string_from("NSFilenamesPboardType") else {
            return false;
        };
        let Some(path_string) = ns_string_from(path) else {
            return false;
        };
        // NSFilenamesPboardType wants an array of paths even for one file.
        let array_cls = class("NSArray");
        if array_cls.is_null() {
            return false;
        }
        let with_object: extern "C" fn(Id, Id, Id) -> Id =
            std::mem::transmute(objc_msgSend as *const ());
        let array = with_object(array_cls, selector("arrayWithObject:"), path_string);
        if array.is_null() {
            return false;
        }
        let set: extern "C" fn(Id, Id, Id, Id) -> bool =
            std::mem::transmute(objc_msgSend as *const ());
        set(pb, selector("setPropertyList:forType:"), array, kind)
    }
}
