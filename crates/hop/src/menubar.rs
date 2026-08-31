//! A macOS menu bar item for starting and stopping hop.
//!
//! Deliberately does NOT run the engine itself. Selecting Start spawns
//! `hop run` as a child process and Stop terminates it. That costs a
//! process boundary and buys three things worth more than it: a panic or
//! wedge inside hop cannot take the menu bar item down with it, Stop is
//! reliable because killing a process always works where unwinding a
//! stuck async runtime may not, and the engine keeps running as exactly
//! the same code path it runs from the command line, so nothing about
//! this file can change how hop behaves.

use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Mutex;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength,
};
use objc2_foundation::{NSObject, NSString};

/// The child `hop run` process, if one is currently running.
///
/// A global because the menu callbacks are Objective-C methods reached
/// from the run loop, with no way to thread state in through them.
static CHILD: Mutex<Option<Child>> = Mutex::new(None);

struct Ivars {
    config: PathBuf,
    status_item: Retained<NSStatusItem>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HopMenuTarget"]
    #[ivars = Ivars]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(start:))]
        fn start(&self, _sender: Option<&AnyObject>) {
            self.start_child();
            self.refresh();
        }

        #[unsafe(method(stop:))]
        fn stop(&self, _sender: Option<&AnyObject>) {
            stop_child();
            self.refresh();
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            // Never leave the engine orphaned: quitting the menu bar item
            // must not leave input captured by a process the user can no
            // longer see or stop.
            stop_child();
            let mtm = MainThreadMarker::new().expect("menu actions run on the main thread");
            let app = NSApplication::sharedApplication(mtm);
            app.terminate(None);
        }
    }
);

impl MenuTarget {
    fn new(
        mtm: MainThreadMarker,
        config: PathBuf,
        status_item: Retained<NSStatusItem>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars {
            config,
            status_item,
        });
        unsafe { msg_send![super(this), init] }
    }

    fn start_child(&self) {
        let mut guard = match CHILD.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.is_some() {
            return;
        }
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(error) => {
                tracing::error!(%error, "could not find hop's own binary to launch");
                return;
            }
        };
        match Command::new(exe)
            .arg("run")
            .arg("--config")
            .arg(&self.ivars().config)
            .spawn()
        {
            Ok(child) => {
                tracing::info!(pid = child.id(), "started hop");
                *guard = Some(child);
            }
            Err(error) => tracing::error!(%error, "could not start hop"),
        }
    }

    /// Update the menu bar title to reflect whether hop is running.
    fn refresh(&self) {
        let running = child_is_running();
        let title = if running { "hop ●" } else { "hop ○" };
        let item = &self.ivars().status_item;
        if let Some(mtm) = MainThreadMarker::new() {
            if let Some(button) = item.button(mtm) {
                button.setTitle(&NSString::from_str(title));
            }
        }
    }
}

/// Whether the child is still alive, reaping it if it has exited so a
/// crashed engine is reported as stopped rather than as still running.
fn child_is_running() -> bool {
    let mut guard = match CHILD.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let Some(child) = guard.as_mut() else {
        return false;
    };
    match child.try_wait() {
        Ok(Some(status)) => {
            tracing::warn!(?status, "hop exited on its own");
            *guard = None;
            false
        }
        Ok(None) => true,
        Err(_) => true,
    }
}

fn stop_child() {
    let mut guard = match CHILD.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(mut child) = guard.take() {
        let _ = child.kill();
        let _ = child.wait();
        tracing::info!("stopped hop");
    }
}

/// Show the menu bar item and run until the user quits. Never returns.
pub fn run(config: PathBuf) -> ! {
    let mtm = MainThreadMarker::new().expect("the menu bar must be built on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    // Accessory rather than Regular: a menu bar item with no Dock icon
    // and no window, which is what this is.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let status_item =
        NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    let target = MenuTarget::new(mtm, config, status_item.clone());

    let menu = NSMenu::new(mtm);
    add_item(&menu, mtm, "Start hop", sel!(start:), &target);
    add_item(&menu, mtm, "Stop hop", sel!(stop:), &target);
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    add_item(&menu, mtm, "Quit", sel!(quit:), &target);
    status_item.setMenu(Some(&menu));

    target.refresh();
    app.run();
    unreachable!("NSApplication::run does not return");
}

fn add_item(
    menu: &NSMenu,
    mtm: MainThreadMarker,
    title: &str,
    action: Sel,
    target: &Retained<MenuTarget>,
) {
    let item = NSMenuItem::new(mtm);
    item.setTitle(&NSString::from_str(title));
    unsafe {
        item.setAction(Some(action));
        item.setTarget(Some(target));
    }
    menu.addItem(&item);
}
