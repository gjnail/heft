//! How risk warnings look: colors, the marker drawn in rows, and the
//! explanation shown in tooltips, menus and the delete dialog.

use eframe::egui::{self, Color32, RichText};

use crate::risk::{Level, Risk};

pub const CAUTION: Color32 = Color32::from_rgb(230, 160, 40);
pub const DANGER: Color32 = Color32::from_rgb(225, 70, 60);

pub fn color(level: Level) -> Color32 {
    match level {
        Level::Caution => CAUTION,
        Level::Danger => DANGER,
    }
}

/// "Caution" / "Danger" plus the title, colored.
pub fn heading(risk: &Risk) -> RichText {
    let word = match risk.level {
        Level::Caution => "Caution",
        Level::Danger => "Danger",
    };
    RichText::new(format!("⚠ {word}: {}", risk.title)).color(color(risk.level)).strong()
}

/// Heading and explanation, for tooltips and menus.
pub fn explain(ui: &mut egui::Ui, risk: &Risk) {
    ui.label(heading(risk));
    ui.label(RichText::new(risk.detail).weak());
}

/// A checkbox without a visible label that screen readers can still name.
pub(super) fn checkbox(ui: &mut egui::Ui, on: &mut bool, name: &str) -> egui::Response {
    let r = ui.add(egui::Checkbox::without_text(on));
    let (v, name) = (*on, name.to_string());
    r.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, v, name.clone()));
    r
}
