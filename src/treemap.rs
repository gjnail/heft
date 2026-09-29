//! Squarified cushion treemap, rendered to a pixel buffer on a background
//! thread.
//!
//! Layout: Bruls, Huizing & van Wijk, "Squarified Treemaps" (2000).
//! Shading: van Wijk & van de Wetering, "Cushion Treemaps" (1999), the look
//! WinDirStat made famous: every level of nesting adds a parabolic ridge, so
//! the hierarchy is visible without drawing borders.

use std::sync::Arc;

use crossbeam_channel::{unbounded, Receiver, Sender};
use eframe::egui::{Color32, ColorImage};

use crate::colors::{self, Category};
use crate::history::Diff;
use crate::tree::{NodeId, Tree};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMode {
    Extension,
    Category,
    Age,
    Growth,
}

impl ColorMode {
    pub fn label(self) -> &'static str {
        match self {
            ColorMode::Extension => "File type",
            ColorMode::Category => "Category",
            ColorMode::Age => "Age",
            ColorMode::Growth => "Growth",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Highlight {
    None,
    Ext(u16),
    Category(Category),
}

pub struct Style {
    pub mode: ColorMode,
    pub ext_colors: Arc<Vec<[f32; 3]>>,
    pub now: i64,
    pub diff: Option<Arc<Diff>>,
    pub highlight: Highlight,
}

impl Style {
    fn leaf_color(&self, tree: &Tree, id: NodeId) -> [f32; 3] {
        let n = tree.node(id);
        let base = if n.is_dir() {
            colors::DIR_COLOR
        } else {
            match self.mode {
                ColorMode::Extension => self.ext_colors[n.ext as usize],
                ColorMode::Category => tree.exts[n.ext as usize].category.color(),
                ColorMode::Age => {
                    if n.mtime <= 0 {
                        colors::UNKNOWN_COLOR
                    } else {
                        colors::age_color((self.now - n.mtime) as f64 / 86_400.0)
                    }
                }
                ColorMode::Growth => match &self.diff {
                    Some(d) => growth_color(d, tree, id),
                    None => colors::UNKNOWN_COLOR,
                },
            }
        };
        let lit = match self.highlight {
            Highlight::None => true,
            Highlight::Ext(e) => !n.is_dir() && n.ext == e,
            Highlight::Category(c) => !n.is_dir() && tree.exts[n.ext as usize].category == c,
        };
        if lit {
            base
        } else {
            let g = (base[0] + base[1] + base[2]) / 3.0 * 0.35;
            [g, g, g]
        }
    }
}

fn growth_color(d: &Diff, tree: &Tree, id: NodeId) -> [f32; 3] {
    // Use the file's own history if we have it, else its folder's.
    let mut cur = id;
    for _ in 0..2 {
        match d.old_size(cur) {
            crate::history::Old::New => return colors::NEW_COLOR,
            crate::history::Old::Size(old) => {
                let now = tree.node(cur).size;
                let ratio = (now as f64 - old as f64) / (now.max(old).max(1) as f64);
                return colors::growth_color(ratio as f32);
            }
            crate::history::Old::Unknown => {
                if cur == crate::tree::ROOT {
                    break;
                }
                cur = tree.node(cur).parent;
            }
        }
    }
    colors::UNKNOWN_COLOR
}

/// A laid-out rectangle in image pixel coordinates.
#[derive(Clone, Copy, Debug)]
pub struct TmRect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    pub node: NodeId,
    pub depth: u16,
}

impl TmRect {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }
}

pub struct Request {
    pub seq: u64,
    pub tree: Arc<Tree>,
    pub root: NodeId,
    pub width: usize,
    pub height: usize,
    pub style: Style,
}

pub struct Rendered {
    pub seq: u64,
    pub root: NodeId,
    pub tree_version: u64,
    pub image: ColorImage,
    pub rects: Vec<TmRect>,
}

impl Rendered {
    pub fn size(&self) -> [usize; 2] {
        self.image.size
    }

    /// Deepest node under an image-space point.
    pub fn hit(&self, x: f32, y: f32) -> Option<NodeId> {
        let mut best: Option<&TmRect> = None;
        for r in &self.rects {
            if r.contains(x, y) && best.is_none_or(|b| r.depth > b.depth) {
                best = Some(r);
            }
        }
        best.map(|r| r.node)
    }

    pub fn rect_of(&self, node: NodeId) -> Option<TmRect> {
        self.rects.iter().find(|r| r.node == node).copied()
    }
}

// Cushion parameters (close to WinDirStat's defaults).
const HEIGHT: f64 = 0.40;
const SCALE: f64 = 0.90;
const AMBIENT: f64 = 0.18;
const BRIGHTNESS: f64 = 1.18;
const LIGHT: [f64; 3] = [-0.098_538_8, -0.098_538_8, 0.985_388]; // normalize(-1, -1, 10)

pub fn render(req: &Request) -> Rendered {
    let (w, h) = (req.width.max(1), req.height.max(1));
    let mut ctx = Ctx {
        tree: &req.tree,
        style: &req.style,
        px: vec![Color32::from_gray(24); w * h],
        w,
        h,
        rects: Vec::with_capacity(4096),
    };
    let rect = [0.0, 0.0, w as f64, h as f64];
    if req.tree.node(req.root).size > 0 || !req.tree.node(req.root).is_dir() {
        ctx.draw(req.root, rect, [0.0; 4], HEIGHT, 0);
    } else {
        ctx.rects.push(TmRect { x0: 0.0, y0: 0.0, x1: w as f32, y1: h as f32, node: req.root, depth: 0 });
    }
    Rendered {
        seq: req.seq,
        root: req.root,
        tree_version: req.tree.version,
        image: ColorImage::new([w, h], ctx.px),
        rects: ctx.rects,
    }
}

struct Ctx<'a> {
    tree: &'a Tree,
    style: &'a Style,
    px: Vec<Color32>,
    w: usize,
    h: usize,
    rects: Vec<TmRect>,
}

impl Ctx<'_> {
    fn draw(&mut self, id: NodeId, r: [f64; 4], surface: [f64; 4], height: f64, depth: u16) {
        let (rw, rh) = (r[2] - r[0], r[3] - r[1]);
        if rw <= 0.0 || rh <= 0.0 {
            return;
        }
        let mut s = surface;
        add_ridge(&mut s, r, height);
        if rw * rh >= 1.0 || depth == 0 {
            self.rects.push(TmRect {
                x0: r[0] as f32,
                y0: r[1] as f32,
                x1: r[2] as f32,
                y1: r[3] as f32,
                node: id,
                depth,
            });
        }
        let n = self.tree.node(id);
        let kids = self.tree.children(id);
        if !n.is_dir() || kids.is_empty() || n.size == 0 || rw < 2.0 || rh < 2.0 {
            let c = self.style.leaf_color(self.tree, id);
            self.paint(r, &s, c);
            return;
        }
        for (child, cr) in squarify(self.tree, kids, n.size, r) {
            self.draw(child, cr, s, height * SCALE, depth + 1);
        }
    }

    fn paint(&mut self, r: [f64; 4], s: &[f64; 4], color: [f32; 3]) {
        let x0 = (r[0].round() as isize).clamp(0, self.w as isize) as usize;
        let x1 = (r[2].round() as isize).clamp(0, self.w as isize) as usize;
        let y0 = (r[1].round() as isize).clamp(0, self.h as isize) as usize;
        let y1 = (r[3].round() as isize).clamp(0, self.h as isize) as usize;
        let (cr, cg, cb) = (color[0] as f64 * 255.0, color[1] as f64 * 255.0, color[2] as f64 * 255.0);
        for iy in y0..y1 {
            let ny = -(2.0 * s[1] * (iy as f64 + 0.5) + s[3]);
            let row = &mut self.px[iy * self.w..(iy + 1) * self.w];
            for (ix, p) in row.iter_mut().enumerate().take(x1).skip(x0) {
                let nx = -(2.0 * s[0] * (ix as f64 + 0.5) + s[2]);
                let cosa = (nx * LIGHT[0] + ny * LIGHT[1] + LIGHT[2]) / (nx * nx + ny * ny + 1.0).sqrt();
                let l = (AMBIENT + (1.0 - AMBIENT) * cosa.max(0.0)) * BRIGHTNESS;
                *p = Color32::from_rgb(
                    (cr * l).min(255.0) as u8,
                    (cg * l).min(255.0) as u8,
                    (cb * l).min(255.0) as u8,
                );
            }
        }
    }
}

fn add_ridge(s: &mut [f64; 4], r: [f64; 4], h: f64) {
    let (w, hh) = (r[2] - r[0], r[3] - r[1]);
    let h4 = 4.0 * h;
    let wf = h4 / w;
    s[2] += wf * (r[2] + r[0]);
    s[0] -= wf;
    let hf = h4 / hh;
    s[3] += hf * (r[3] + r[1]);
    s[1] -= hf;
}

/// Lay out `kids` (sorted biggest first) inside `r`, rows along the shorter
/// side, greedily keeping aspect ratios close to 1.
pub fn squarify(tree: &Tree, kids: &[NodeId], total: u64, r: [f64; 4]) -> Vec<(NodeId, [f64; 4])> {
    let mut out = Vec::with_capacity(kids.len());
    let sizes: Vec<f64> = kids.iter().map(|&k| tree.node(k).size as f64).take_while(|&s| s > 0.0).collect();
    if sizes.is_empty() || total == 0 {
        return out;
    }
    let mut remaining: f64 = sizes.iter().sum();
    let [mut x0, mut y0, x1, y1] = r;
    let mut i = 0;
    while i < sizes.len() {
        let (w, h) = (x1 - x0, y1 - y0);
        if w <= 0.0 || h <= 0.0 {
            break;
        }
        let scale = w * h / remaining;
        let short = w.min(h);
        let short2 = short * short;
        let rmax = sizes[i] * scale;

        let mut end = i;
        let mut row = 0.0;
        let mut worst = f64::INFINITY;
        while end < sizes.len() {
            let s = sizes[end] * scale;
            let sum = row + s;
            let sum2 = sum * sum;
            let ratio = (short2 * rmax / sum2).max(sum2 / (short2 * s));
            if ratio > worst {
                break;
            }
            worst = ratio;
            row = sum;
            end += 1;
        }

        let last_row = end == sizes.len();
        let thick = if last_row { if w >= h { w } else { h } } else { row / short };
        let mut pos = if w >= h { y0 } else { x0 };
        let limit = if w >= h { y1 } else { x1 };
        for j in i..end {
            let len = if j + 1 == end { limit - pos } else { sizes[j] * scale / thick };
            let rect = if w >= h {
                [x0, pos, x0 + thick, pos + len]
            } else {
                [pos, y0, pos + len, y0 + thick]
            };
            out.push((kids[j], rect));
            pos += len;
        }
        if w >= h {
            x0 += thick;
        } else {
            y0 += thick;
        }
        remaining -= sizes[i..end].iter().sum::<f64>();
        i = end;
    }
    out
}

/// Long-lived render thread. Stale requests are dropped in favour of the
/// newest one, so resizing the window never queues up work.
pub struct Worker {
    tx: Sender<Request>,
    pub rx: Receiver<Rendered>,
}

impl Worker {
    pub fn new(repaint: impl Fn() + Send + 'static) -> Self {
        let (tx, req_rx) = unbounded::<Request>();
        let (res_tx, rx) = unbounded();
        std::thread::Builder::new()
            .name("treemap".into())
            .spawn(move || {
                while let Ok(mut req) = req_rx.recv() {
                    while let Ok(newer) = req_rx.try_recv() {
                        req = newer;
                    }
                    if res_tx.send(render(&req)).is_err() {
                        return;
                    }
                    repaint();
                }
            })
            .expect("spawn treemap thread");
        Worker { tx, rx }
    }

    pub fn request(&self, req: Request) {
        let _ = self.tx.send(req);
    }
}

pub fn save_png(r: &Rendered, path: &std::path::Path) -> Result<(), String> {
    let [w, h] = r.image.size;
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    let data: Vec<u8> = r.image.pixels.iter().flat_map(|c| [c.r(), c.g(), c.b()]).collect();
    writer.write_image_data(&data).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{flags, ScanInfo, ScanMode, TreeBuilder, ROOT};

    #[test]
    fn squarify_tiles_exactly() {
        let mut b = TreeBuilder::new("X:\\");
        for (i, s) in [600u64, 300, 200, 100, 50, 25, 25].iter().enumerate() {
            b.add(ROOT, &format!("f{i}"), 0, *s, *s, 0);
        }
        let d = b.add(ROOT, "empty", flags::DIR, 0, 0, 0);
        let _ = d;
        let t = b.finish(
            "X:\\".into(),
            ScanInfo { mode: ScanMode::Walk, duration_ms: 0, finished_at: 0, unreadable_dirs: 0, note: None, phases: Vec::new() },
        );
        let rects = squarify(&t, t.children(ROOT), t.node(ROOT).size, [0.0, 0.0, 400.0, 300.0]);
        assert_eq!(rects.len(), 7, "zero-size children are skipped");
        let area: f64 = rects.iter().map(|(_, r)| (r[2] - r[0]) * (r[3] - r[1])).sum();
        assert!((area - 120_000.0).abs() < 1e-6, "area {area}");
        for (id, r) in &rects {
            let expect = t.node(*id).size as f64 / 1300.0 * 120_000.0;
            let got = (r[2] - r[0]) * (r[3] - r[1]);
            assert!((got - expect).abs() / expect < 1e-6, "{id}: {got} vs {expect}");
            assert!(r[0] >= -1e-9 && r[1] >= -1e-9 && r[2] <= 400.0 + 1e-9 && r[3] <= 300.0 + 1e-9);
        }
    }
}
