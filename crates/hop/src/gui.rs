//! hop's window.
//!
//! What the tray could not do: say whether the link is actually up, show
//! why it is not, and let the settings be changed without opening a text
//! editor on a TOML file.
//!
//! Like the tray and the Mac's menu bar item, this does NOT run the
//! engine. Start spawns `hop run` as a child with no console and Stop
//! kills it, so a fault in the engine cannot take the window down, Stop
//! always works, and the engine runs the identical code path it runs
//! from a terminal.
//!
//! The window reads two things from that child: whether the process is
//! alive, and the lines it writes to stderr. The log view is those lines;
//! the status line is the newest link transition among them. hop's own
//! logging is the interface, which is why `run` prints an explicit
//! marker for each transition rather than the window pattern matching on
//! prose that could be reworded at any time.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::{Arc, Mutex};

use eframe::egui;

use hop::settings::Settings;
use hop_platform::windows::autostart;

/// Tells Windows to give the child no console window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How many log lines the window keeps. Enough to see what happened
/// across a few reconnects, small enough that a hop left running for a
/// week does not grow without bound.
const LOG_LINES: usize = 500;

/// The marker `hop run` prints on every link transition, so the window
/// reads a state rather than parsing prose.
const STATUS_MARKER: &str = "hop-status:";

/// Everything the window shows that comes from the running engine.
#[derive(Default)]
struct EngineOutput {
    lines: std::collections::VecDeque<String>,
    status: Option<String>,
}

pub struct HopApp {
    config_path: PathBuf,
    settings: Settings,
    /// What was last loaded or saved, so the window can tell whether
    /// there is anything to save.
    saved: Settings,
    child: Option<Child>,
    output: Arc<Mutex<EngineOutput>>,
    autostart: bool,
    message: Option<(String, bool)>,
    anchor_enabled: bool,
    anchor_value: f32,
}

impl HopApp {
    pub fn new(config_path: PathBuf) -> Self {
        let settings = Settings::load(&config_path);
        let anchor_value = settings.anchor.unwrap_or(0.5);
        Self {
            saved: settings.clone(),
            anchor_enabled: settings.anchor.is_some(),
            anchor_value,
            settings,
            config_path,
            child: None,
            output: Arc::new(Mutex::new(EngineOutput::default())),
            autostart: autostart::is_enabled(),
            message: None,
        }
    }

    fn is_running(&mut self) -> bool {
        let Some(child) = self.child.as_mut() else {
            return false;
        };
        match child.try_wait() {
            Ok(Some(_)) => {
                self.child = None;
                if let Ok(mut output) = self.output.lock() {
                    output.status = Some("Stopped".into());
                }
                false
            }
            Ok(None) => true,
            // Unknowable: report it as running, since claiming a live
            // engine is stopped would leave the user unable to stop it.
            Err(_) => true,
        }
    }

    fn start(&mut self) {
        if self.is_running() {
            return;
        }
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(error) => {
                self.message = Some((format!("could not find hop: {error}"), true));
                return;
            }
        };

        use std::os::windows::process::CommandExt;
        let spawned = Command::new(exe)
            .arg("run")
            .arg("--config")
            .arg(&self.config_path)
            .creation_flags(CREATE_NO_WINDOW)
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .spawn();

        match spawned {
            Ok(mut child) => {
                if let Some(stderr) = child.stderr.take() {
                    self.pump_output(stderr);
                }
                self.child = Some(child);
                self.message = None;
            }
            Err(error) => self.message = Some((format!("could not start hop: {error}"), true)),
        }
    }

    /// Reads the child's stderr on its own thread.
    ///
    /// A thread rather than polling: a pipe read blocks, and doing it on
    /// the UI thread would freeze the window between log lines, which on
    /// an idle link is most of the time.
    fn pump_output(&self, stderr: ChildStderr) {
        let output = Arc::clone(&self.output);
        std::thread::Builder::new()
            .name("hop-log-reader".into())
            .spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let Ok(mut output) = output.lock() else {
                        return;
                    };
                    if let Some(status) = line.split(STATUS_MARKER).nth(1) {
                        output.status = Some(status.trim().to_string());
                    }
                    output.lines.push_back(line);
                    while output.lines.len() > LOG_LINES {
                        output.lines.pop_front();
                    }
                }
            })
            .ok();
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Ok(mut output) = self.output.lock() {
            output.status = Some("Stopped".into());
        }
    }

    fn save(&mut self) {
        self.settings.anchor = self.anchor_enabled.then_some(self.anchor_value);
        match self.settings.save(&self.config_path) {
            Ok(()) => {
                self.saved = self.settings.clone();
                let running = self.is_running();
                self.message = Some((
                    if running {
                        "Saved. Restarting hop to pick it up.".into()
                    } else {
                        "Saved.".into()
                    },
                    false,
                ));
                // Settings are read once at startup, so a save that did
                // not restart would show the user a value hop is not
                // actually using.
                if running {
                    self.stop();
                    self.start();
                }
            }
            Err(error) => self.message = Some((error, true)),
        }
    }

    fn set_autostart(&mut self, wanted: bool) {
        let result = if wanted {
            std::env::current_exe()
                .map_err(|e| e.to_string())
                .and_then(|exe| {
                    autostart::enable(&autostart::startup_command(&exe, &self.config_path))
                })
        } else {
            autostart::disable()
        };
        match result {
            Ok(()) => self.autostart = wanted,
            Err(error) => self.message = Some((error, true)),
        }
    }
}

impl eframe::App for HopApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let running = self.is_running();
        let (status, log) = match self.output.lock() {
            Ok(output) => (
                output.status.clone(),
                output.lines.iter().cloned().collect::<Vec<_>>(),
            ),
            Err(_) => (None, Vec::new()),
        };
        let status = status.unwrap_or_else(|| "Stopped".into());
        let connected = status.starts_with("Connected");

        egui::TopBottomPanel::top("status").show(ctx, |ui| {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let (colour, label) = if connected {
                    (egui::Color32::from_rgb(46, 160, 67), status.as_str())
                } else if running {
                    (egui::Color32::from_rgb(210, 153, 34), status.as_str())
                } else {
                    (egui::Color32::from_gray(120), "Stopped")
                };
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 5.0, colour);
                ui.heading(label);

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if running {
                        if ui.button("Stop").clicked() {
                            self.stop();
                        }
                    } else if ui.button("Start").clicked() {
                        self.start();
                    }
                });
            });
            ui.add_space(10.0);
        });

        egui::TopBottomPanel::bottom("about").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("hop {}", env!("CARGO_PKG_VERSION")))
                        .color(egui::Color32::from_gray(130)),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Check for updates").clicked() {
                        self.message = Some(match crate::update::update(true) {
                            Ok(Some(version)) => (
                                format!("hop {version} is available. Restart hop to install it."),
                                false,
                            ),
                            Ok(None) => ("hop is up to date.".into(), false),
                            Err(error) => (error.to_string(), true),
                        });
                    }
                });
            });
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some((text, is_error)) = &self.message {
                let colour = if *is_error {
                    egui::Color32::from_rgb(200, 60, 60)
                } else {
                    egui::Color32::from_rgb(46, 160, 67)
                };
                ui.label(egui::RichText::new(text).color(colour));
                ui.add_space(6.0);
            }

            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading("Connection");
                egui::Grid::new("connection")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("The Mac's address");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.server)
                                .hint_text("192.168.1.42:24810")
                                .desired_width(220.0),
                        );
                        ui.end_row();

                        ui.label("This machine's id");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.id).desired_width(220.0),
                        );
                        ui.end_row();

                        ui.label("Key file");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.key_file)
                                .desired_width(220.0),
                        );
                        ui.end_row();
                    });
                ui.label(
                    egui::RichText::new("Both machines must hold the same key file.")
                        .small()
                        .color(egui::Color32::from_gray(130)),
                );

                ui.add_space(14.0);
                ui.heading("Pointer");
                egui::Grid::new("pointer")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Edge facing the Mac");
                        egui::ComboBox::from_id_salt("return_edge")
                            .selected_text(&self.settings.return_edge)
                            .show_ui(ui, |ui| {
                                for edge in ["top", "bottom", "left", "right"] {
                                    ui.selectable_value(
                                        &mut self.settings.return_edge,
                                        edge.to_string(),
                                        edge,
                                    );
                                }
                            });
                        ui.end_row();

                        ui.label("Pointer speed");
                        ui.add(
                            egui::Slider::new(&mut self.settings.mouse_scale, 0.1..=2.0)
                                .fixed_decimals(2),
                        );
                        ui.end_row();

                        ui.label("Where the Mac sits");
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut self.anchor_enabled, "Set by hand");
                            ui.add_enabled(
                                self.anchor_enabled,
                                egui::Slider::new(&mut self.anchor_value, 0.0..=1.0)
                                    .fixed_decimals(2),
                            );
                        });
                        ui.end_row();
                    });
                ui.label(
                    egui::RichText::new(
                        "Leave unset for centred under the primary monitor. Set it if the \
                         cursor arrives on the wrong screen.",
                    )
                    .small()
                    .color(egui::Color32::from_gray(130)),
                );

                ui.add_space(14.0);
                let mut autostart_now = self.autostart;
                if ui
                    .checkbox(&mut autostart_now, "Start hop when Windows starts")
                    .changed()
                {
                    self.set_autostart(autostart_now);
                }

                ui.add_space(14.0);
                let dirty = {
                    let mut candidate = self.settings.clone();
                    candidate.anchor = self.anchor_enabled.then_some(self.anchor_value);
                    candidate != self.saved
                };
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(dirty, egui::Button::new("Save settings"))
                        .clicked()
                    {
                        self.save();
                    }
                    if ui
                        .add_enabled(dirty, egui::Button::new("Discard"))
                        .clicked()
                    {
                        self.settings = self.saved.clone();
                        self.anchor_enabled = self.saved.anchor.is_some();
                        self.anchor_value = self.saved.anchor.unwrap_or(0.5);
                        self.message = None;
                    }
                });

                ui.add_space(14.0);
                ui.collapsing("Log", |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(180.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for line in &log {
                                ui.label(egui::RichText::new(line).monospace().small());
                            }
                        });
                });
            });
        });

        // The status and the log come from another thread, so the window
        // has to be told to repaint rather than waiting for a click.
        ctx.request_repaint_after(std::time::Duration::from_millis(400));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Never leave the engine orphaned: closing the window must not
        // leave input captured by a process the user can no longer see.
        self.stop();
    }
}

/// Show the window and run until it is closed.
pub fn run(config_path: PathBuf) -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([470.0, 620.0])
            .with_min_inner_size([420.0, 420.0])
            .with_title("hop"),
        ..Default::default()
    };

    eframe::run_native(
        "hop",
        options,
        Box::new(move |_cc| {
            let mut app = HopApp::new(config_path);
            // Someone opening hop wants it running; making them press
            // Start first would be a step for its own sake.
            app.start();
            Ok(Box::new(app) as Box<dyn eframe::App>)
        }),
    )
    .map_err(|error| error.to_string())
}
