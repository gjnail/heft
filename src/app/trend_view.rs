//! "Size over time" chart for the folder shown in the treemap, built from the
//! snapshot Heft saves after every scan.

use std::time::Duration;

use crossbeam_channel::Receiver;
use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Rect, RichText, Sense, Shape, Stroke};

use super::HeftApp;
use crate::history;
use crate::platform;
use crate::tree::ROOT;
use crate::util::{fmt_delta, fmt_size};

#[derive(Default)]
pub(super) struct TrendState {
    key: Option<(String, String, usize)>,
    running: Option<Receiver<Vec<(i64, u64)>>>,
    points: Vec<(i64, u64)>,
}

impl HeftApp {
    pub(super) fn trend_chart(&mut self, ui: &mut egui::Ui) {
        let Some(tree) = self.tree.clone() else { return };
        let view = self.view_root;
        let view_path = tree.path(view);
        let key = (tree.root_path.clone(), view_path.clone(), self.snapshots.len());
        if self.trend.key.as_ref() != Some(&key) {
            let rel: Vec<String> = tree.ancestors(view).into_iter().skip(1).map(|a| tree.name(a).to_string()).collect();
            let (tx, rx) = crossbeam_channel::bounded(1);
            let (root, ctx) = (tree.root_path.clone(), ui.ctx().clone());
            std::thread::spawn(move || {
                let _ = tx.send(history::size_history(&root, &rel));
                ctx.request_repaint();
            });
            self.trend.running = Some(rx);
            self.trend.key = Some(key);
        }
        if let Some(rx) = &self.trend.running {
            match rx.try_recv() {
                Ok(p) => {
                    self.trend.points = p;
                    self.trend.running = None;
                }
                Err(_) => ui.ctx().request_repaint_after(Duration::from_millis(100)),
            }
        }

        // Saved snapshots, plus this scan if it isn't saved yet.
        let mut points = self.trend.points.clone();
        let now = (tree.info.finished_at, tree.node(view).size);
        if points.last().is_none_or(|p| p.0 < now.0) {
            points.push(now);
        }
        let name = if view == ROOT { tree.root_path.clone() } else { tree.name(view).to_string() };
        if points.len() < 2 {
            ui.label(RichText::new(format!("{name} over time")).strong());
            ui.label(RichText::new("Scan this location again later to see how it changes.").weak());
            ui.add_space(8.0);
            return;
        }

        let (first, last) = (points[0], points[points.len() - 1]);
        let change = last.1 as i64 - first.1 as i64;
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{name} over time")).strong());
            let color = if change > 0 { Color32::from_rgb(240, 110, 90) } else { Color32::from_rgb(100, 210, 120) };
            ui.label(RichText::new(format!("{} since {}", fmt_delta(change), platform::fmt_date(first.0))).color(color));
        });

        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 130.0), Sense::hover());
        let p = ui.painter_at(rect);
        let v = ui.visuals();
        let plot = Rect::from_min_max(rect.min + vec2(64.0, 8.0), rect.max - vec2(8.0, 20.0));
        let (lo, hi) = points.iter().fold((u64::MAX, 0u64), |(lo, hi), p| (lo.min(p.1), hi.max(p.1)));
        let pad = ((hi - lo) / 10).max(1 << 20);
        let (lo, hi) = (lo.saturating_sub(pad), hi + pad);
        let (t0, t1) = (first.0, last.0.max(first.0 + 1));
        let at = |t: i64, s: u64| {
            pos2(
                plot.left() + plot.width() * (t - t0) as f32 / (t1 - t0) as f32,
                plot.bottom() - plot.height() * (s - lo) as f32 / (hi - lo) as f32,
            )
        };

        let grid = v.widgets.noninteractive.bg_stroke.color;
        for i in 0..=2 {
            let s = lo + (hi - lo) * i / 2;
            let y = at(t0, s).y;
            p.line_segment([pos2(plot.left(), y), pos2(plot.right(), y)], Stroke::new(1.0, grid));
            p.text(pos2(plot.left() - 6.0, y), Align2::RIGHT_CENTER, fmt_size(s), FontId::proportional(10.5), v.weak_text_color());
        }
        let when = |t: i64| if t1 - t0 < 2 * 86_400 { platform::fmt_datetime(t) } else { platform::fmt_date(t) };
        p.text(plot.left_bottom() + vec2(0.0, 4.0), Align2::LEFT_TOP, when(t0), FontId::proportional(10.5), v.weak_text_color());
        p.text(plot.right_bottom() + vec2(0.0, 4.0), Align2::RIGHT_TOP, when(t1), FontId::proportional(10.5), v.weak_text_color());

        let line: Vec<_> = points.iter().map(|&(t, s)| at(t, s)).collect();
        let accent = v.selection.stroke.color;
        p.add(Shape::line(line.clone(), Stroke::new(2.0, accent)));
        for pt in &line {
            p.circle_filled(*pt, 3.0, accent);
        }

        // Hover: the nearest point by time.
        if let Some(pos) = resp.hover_pos()
            && let Some((i, pt)) = line.iter().enumerate().min_by(|a, b| (a.1.x - pos.x).abs().total_cmp(&(b.1.x - pos.x).abs()))
        {
            p.circle_stroke(*pt, 5.0, Stroke::new(1.5, v.strong_text_color()));
            let (t, s) = points[i];
            let delta = if i > 0 { format!(" ({} from the scan before)", fmt_delta(s as i64 - points[i - 1].1 as i64)) } else { String::new() };
            resp.on_hover_text(format!("{}: {}{delta}", platform::fmt_datetime(t), fmt_size(s)));
        }
        ui.add_space(8.0);
    }
}
