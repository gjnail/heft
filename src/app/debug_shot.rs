//! Debug builds only: save a screenshot of the window and quit, for the
//! README and the website.
//!
//! HEFT_DEBUG_SCREENSHOT=<file.png> takes the picture after
//! HEFT_DEBUG_SCREENSHOT_DELAY seconds (default 3). HEFT_DEBUG_THEME=dark or
//! light and HEFT_DEBUG_SIZE=<width>x<height> set the window up first. The
//! Hardware page times its own picture by sensor samples instead.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use eframe::egui;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

/// Call once per frame. `own_timing` leaves the picture to the page.
pub fn frame(ctx: &egui::Context, own_timing: bool) {
    static START: OnceLock<Instant> = OnceLock::new();
    static ASKED: AtomicBool = AtomicBool::new(false);
    let Some(path) = env("HEFT_DEBUG_SCREENSHOT") else { return };
    let start = *START.get_or_init(|| {
        match env("HEFT_DEBUG_THEME").as_deref() {
            Some("dark") => ctx.set_theme(egui::Theme::Dark),
            Some("light") => ctx.set_theme(egui::Theme::Light),
            _ => {}
        }
        if let Some((w, h)) = env("HEFT_DEBUG_SIZE").as_deref().and_then(|s| s.split_once('x'))
            && let (Ok(w), Ok(h)) = (w.parse::<f32>(), h.parse::<f32>())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(w, h)));
        }
        Instant::now()
    });
    if own_timing {
        return;
    }
    if let Some(img) = taken(ctx) {
        save(&path, &img);
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        return;
    }
    let delay: f32 = env("HEFT_DEBUG_SCREENSHOT_DELAY").and_then(|v| v.parse().ok()).unwrap_or(3.0);
    if start.elapsed().as_secs_f32() >= delay && !ASKED.swap(true, Ordering::Relaxed) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
    }
    ctx.request_repaint_after(Duration::from_millis(200));
}

/// The screenshot delivered this frame, if any.
pub fn taken(ctx: &egui::Context) -> Option<std::sync::Arc<egui::ColorImage>> {
    ctx.input(|i| {
        i.events.iter().find_map(|e| match e {
            egui::Event::Screenshot { image, .. } => Some(image.clone()),
            _ => None,
        })
    })
}

pub fn save(path: &str, img: &egui::ColorImage) {
    let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
    if let Ok(file) = std::fs::File::create(path) {
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), img.size[0] as u32, img.size[1] as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        if let Ok(mut w) = enc.write_header() {
            let _ = w.write_image_data(&bytes);
        }
    }
}
