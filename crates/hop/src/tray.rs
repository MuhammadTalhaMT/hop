//! A Windows notification area icon for starting and stopping hop.
//!
//! The direct counterpart to `menubar` on the Mac, and it makes the same
//! trade for the same reasons: it does NOT run the engine itself. Start
//! spawns `hop run` as a child process and Stop kills it. That costs a
//! process boundary and buys three things worth more. A panic or a wedge
//! inside hop cannot take the tray icon down with it. Stop is reliable,
//! because killing a process always works where unwinding a stuck async
//! runtime may not. And the engine runs as exactly the same code path it
//! runs from the command line, so nothing in this file can change how hop
//! behaves.
//!
//! The child is spawned with no console window, which is the entire point
//! of the exercise: hop stops being a command prompt you leave open.

use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Mutex;

use hop_platform::windows::tray::{Tray, TrayEvent};

/// Tells Windows to give the child process no console window at all.
/// Without it, starting hop from the tray pops up the console this whole
/// module exists to get rid of.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The child `hop run` process, if one is currently running.
static CHILD: Mutex<Option<Child>> = Mutex::new(None);

fn lock() -> std::sync::MutexGuard<'static, Option<Child>> {
    match CHILD.lock() {
        Ok(guard) => guard,
        // A poisoned lock here means a previous handler panicked. The
        // child handle is still perfectly usable, and refusing to give it
        // back would strand a running engine with no way to stop it.
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn start_child(config: &PathBuf) {
    let mut guard = lock();
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

    use std::os::windows::process::CommandExt;
    match Command::new(exe)
        .arg("run")
        .arg("--config")
        .arg(config)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(child) => {
            tracing::info!(pid = child.id(), "started hop");
            *guard = Some(child);
        }
        Err(error) => tracing::error!(%error, "could not start hop"),
    }
}

fn stop_child() {
    let mut guard = lock();
    if let Some(mut child) = guard.take() {
        let _ = child.kill();
        let _ = child.wait();
        tracing::info!("stopped hop");
    }
}

/// Whether the child is still alive, reaping it if it has exited, so a
/// crashed engine reads as stopped rather than as still running.
fn child_is_running() -> bool {
    let mut guard = lock();
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
        // Unknowable: treat it as running, since claiming a live engine
        // is stopped would leave the user unable to stop it.
        Err(_) => true,
    }
}

/// Show the tray icon and run until the user quits.
pub fn run(config: PathBuf) -> Result<(), String> {
    let mut tray = Tray::new("hop: stopped")?;

    // Start the engine straight away. Someone launching the tray wants
    // hop running; making them pick Start first would be a step for its
    // own sake.
    start_child(&config);
    tray.set_running(child_is_running(), status_text(child_is_running()));

    tray.run(|tray, event| {
        match event {
            TrayEvent::Start => start_child(&config),
            TrayEvent::Stop => stop_child(),
            // Never leave the engine orphaned: quitting the tray must not
            // leave input captured by a process the user can no longer
            // see or stop.
            TrayEvent::Quit => stop_child(),
        }
        let running = child_is_running();
        tray.set_running(running, status_text(running));
    });

    stop_child();
    Ok(())
}

fn status_text(running: bool) -> &'static str {
    if running {
        "hop: running"
    } else {
        "hop: stopped"
    }
}
