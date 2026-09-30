//! Settings: appearance and free space alerts everywhere, and on a Mac how
//! items go to the Trash, where update checks look, and Full Disk Access.
//! On a Mac it opens with ⌘, (Heft › Settings…), like any other Mac app.

use eframe::egui::{self, RichText};

use super::HeftApp;

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(4.0);
    ui.label(RichText::new(title).strong());
}

#[cfg(target_os = "macos")]
fn hint(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).weak().size(11.5));
}

impl HeftApp {
    pub(super) fn settings_modal(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut close = false;
        let resp = egui::Modal::new(egui::Id::new("settings")).show(ctx, |ui| {
            ui.set_width(540.0);
            ui.heading("Settings");
            ui.add_space(4.0);

            section(ui, "Appearance");
            let before = ctx.options(|o| o.theme_preference);
            let mut pref = before;
            ui.horizontal(|ui| {
                ui.radio_value(&mut pref, egui::ThemePreference::System, "Match the system");
                ui.radio_value(&mut pref, egui::ThemePreference::Light, "Light");
                ui.radio_value(&mut pref, egui::ThemePreference::Dark, "Dark");
            });
            if pref != before {
                ctx.set_theme(pref);
            }
            ui.separator();

            self.alert_menu(ui);

            #[cfg(target_os = "macos")]
            self.mac_settings(ui);

            ui.add_space(10.0);
            if ui.button(RichText::new("Done").strong()).clicked() {
                close = true;
            }
        });
        if close || resp.should_close() {
            self.settings_open = false;
        }
    }

    #[cfg(target_os = "macos")]
    fn mac_settings(&mut self, ui: &mut egui::Ui) {
        use crate::mac::apps::updaters;
        use crate::platform;

        ui.separator();
        section(ui, "Moving to the Trash");
        let mut finder = platform::trash_through_finder();
        if ui.checkbox(&mut finder, "Move items to the Trash through Finder, so Finder's Put Back works too").changed() {
            platform::set_trash_through_finder(finder);
        }
        hint(
            ui,
            "macOS asks once to let Heft control Finder. Heft's Removed tab can put items back either way. Items on \
             other disks, and ones that need your password, still go to the Trash the usual way.",
        );
        if finder && let Some(e) = platform::finder_refused() {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(super::tools::AMBER, format!("Finder didn't take the last item ({e}), so it went the usual way."));
                if ui.small_button("Automation settings").clicked() {
                    crate::mac::open_url("x-apple.systempreferences:com.apple.preference.security?Privacy_Automation");
                }
            });
        }

        ui.separator();
        section(ui, "Checking for updates");
        let mut feeds = updaters::check_feeds();
        if ui.checkbox(&mut feeds, "Also ask apps that update themselves whether there's a newer version").changed() {
            updaters::set_check_feeds(feeds);
        }
        hint(
            ui,
            "When you check for updates on the Apps page, Heft then asks each app's own update server (the address \
             inside the app, for apps that use Sparkle) for its newest version. Otherwise Heft only asks Homebrew and, \
             if it's installed, mas for App Store apps.",
        );

        ui.separator();
        section(ui, "Full Disk Access");
        ui.horizontal_wrapped(|ui| {
            match crate::mac::has_full_disk_access() {
                Some(true) => ui.colored_label(super::tools::GREEN, "Heft has Full Disk Access."),
                Some(false) => ui.label("Heft doesn't have Full Disk Access, so macOS keeps Mail, Messages, Safari, the Trash and other apps' data from it."),
                None => ui.label("Heft can't tell whether it has Full Disk Access."),
            };
            if ui.small_button("Privacy & Security").clicked() {
                crate::mac::open_url(crate::mac::FULL_DISK_ACCESS_SETTINGS);
            }
        });
    }
}
