//! The app icon, a tiny cushion treemap, drawn at any size so the window
//! icon and the packaged icons (Windows `.ico` via `build.rs`, Linux `.png`,
//! macOS `.icns`) share one source.

/// Blocks on a 64-unit grid: (x0, y0, x1, y1, color).
const BLOCKS: [(f32, f32, f32, f32, [u8; 3]); 5] = [
    (4.0, 4.0, 36.0, 60.0, [77, 140, 250]),
    (40.0, 4.0, 60.0, 34.0, [242, 92, 82]),
    (40.0, 34.0, 60.0, 48.0, [92, 209, 107]),
    (40.0, 48.0, 50.0, 60.0, [250, 204, 66]),
    (50.0, 48.0, 60.0, 60.0, [184, 112, 245]),
];

/// Square RGBA image, `size` × `size`.
pub fn rgba(size: u32) -> Vec<u8> {
    let scale = size as f32 / 64.0;
    let mut out = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let (gx, gy) = ((x as f32 + 0.5) / scale, (y as f32 + 0.5) / scale);
            // One grid unit of gutter on the right/bottom of every block.
            let Some(&(x0, y0, x1, y1, c)) =
                BLOCKS.iter().find(|b| gx >= b.0 && gx < b.2 - 1.0 && gy >= b.1 && gy < b.3 - 1.0)
            else {
                continue;
            };
            // Cushion-like shading: brighter towards the top-left.
            let (fx, fy) = ((gx - x0) / (x1 - x0), (gy - y0) / (y1 - y0));
            let l = 1.15 - 0.45 * (fx * fx + fy * fy).sqrt() / std::f32::consts::SQRT_2;
            let i = ((y * size + x) * 4) as usize;
            out[i] = (c[0] as f32 * l).min(255.0) as u8;
            out[i + 1] = (c[1] as f32 * l).min(255.0) as u8;
            out[i + 2] = (c[2] as f32 * l).min(255.0) as u8;
            out[i + 3] = 255;
        }
    }
    out
}

pub fn save_png(path: &std::path::Path, size: u32) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), size, size);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().map_err(|e| e.to_string())?;
    w.write_image_data(&rgba(size)).map_err(|e| e.to_string())
}
