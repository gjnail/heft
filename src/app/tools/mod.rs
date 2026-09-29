//! The Windows maintenance workspaces: junk cleaner, startup programs,
//! installed programs (uninstall + updates) and registry issues.

mod cleaner;
mod programs;
mod registry;
mod startup;

use eframe::egui::{self, vec2, Color32, Rect, Response, RichText};

use super::Workspace;

/// Things a workspace asks the main app to do.
pub enum Event {
    Toast(String, bool),
    /// Switch to Disk usage and scan this folder.
    Scan(String),
    /// Restart elevated, reopening the current workspace.
    Elevate,
}

pub struct Tools {
    elevated: bool,
    cleaner: cleaner::State,
    startup: startup::State,
    programs: programs::State,
    registry: registry::State,
    events: Vec<Event>,
}

impl Tools {
    pub fn new(elevated: bool) -> Self {
        Tools {
            elevated,
            cleaner: cleaner::State::new(),
            startup: startup::State::default(),
            programs: programs::State::default(),
            registry: registry::State::default(),
            events: Vec::new(),
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, ws: Workspace) {
        let cx = Cx { elevated: self.elevated, events: &mut self.events };
        match ws {
            Workspace::Cleaner => self.cleaner.show(ui, cx),
            Workspace::Startup => self.startup.show(ui, cx),
            Workspace::Programs => self.programs.show(ui, cx),
            Workspace::Registry => self.registry.show(ui, cx),
            Workspace::Disk => {}
        }
    }

    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }
}

/// What every workspace gets while drawing.
struct Cx<'a> {
    elevated: bool,
    events: &'a mut Vec<Event>,
}

impl Cx<'_> {
    fn toast(&mut self, msg: impl Into<String>, is_error: bool) {
        self.events.push(Event::Toast(msg.into(), is_error));
    }

    /// A "needs administrator" notice with a restart button.
    fn admin_notice(&mut self, ui: &mut egui::Ui, text: &str) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("🛡").color(Color32::from_rgb(250, 200, 70)));
            ui.label(RichText::new(text).weak());
            if ui.small_button("Restart as administrator").clicked() {
                self.events.push(Event::Elevate);
            }
        });
    }
}

pub const GREEN: Color32 = Color32::from_rgb(110, 200, 120);
pub const AMBER: Color32 = Color32::from_rgb(240, 180, 60);

/// Workspace title and one-line explanation.
fn heading(ui: &mut egui::Ui, title: &str, subtitle: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(title).size(20.0).strong());
    ui.label(RichText::new(subtitle).weak());
    ui.add_space(8.0);
}

/// Hover / selection background for a custom-painted row.
fn row_background(ui: &egui::Ui, rect: Rect, resp: &Response, selected: bool) {
    if selected {
        ui.painter().rect_filled(rect, 3.0, ui.visuals().selection.bg_fill);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 3.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
}

fn share_bar(ui: &egui::Ui, rect: Rect, frac: f32, color: Color32) {
    let p = ui.painter();
    p.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
    let mut f = rect;
    f.set_width(rect.width() * frac.clamp(0.0, 1.0));
    p.rect_filled(f, 2.0, color);
}

/// A tinted box for summaries and notices.
fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::group(ui.style())
        .fill(ui.visuals().faint_bg_color)
        .corner_radius(8.0)
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

fn big_button(ui: &mut egui::Ui, text: &str, enabled: bool) -> Response {
    ui.add_enabled(enabled, egui::Button::new(RichText::new(text).strong()).min_size(vec2(120.0, 30.0)))
}
