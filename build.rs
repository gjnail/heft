//! Embeds the app icon and version details in heft.exe, so Explorer, the
//! taskbar and Task Manager show them. Other platforms need nothing from this.

#[path = "src/icon.rs"]
mod icon;

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/icon.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let ico = Path::new(&std::env::var("OUT_DIR").unwrap()).join("heft.ico");
    let result = write_ico(&ico).and_then(|()| {
        winresource::WindowsResource::new()
            .set_icon(ico.to_str().unwrap())
            .set("ProductName", "Heft")
            .set("FileDescription", "Heft")
            .set("OriginalFilename", "heft.exe")
            .set("LegalCopyright", "Copyright (c) 2026 Greg Nail and the Heft contributors")
            .compile()
            .map_err(|e| e.to_string())
    });
    // A cross-check from macOS or Linux usually has no resource compiler. The
    // exe works without the icon, and the release workflow checks it's there.
    if let Err(e) = result {
        println!("cargo:warning=heft.exe will have no icon or version details: {e}");
    }
}

/// Writes a `.ico` with one PNG image per size, the format Windows uses for
/// icons since Vista.
fn write_ico(path: &Path) -> Result<(), String> {
    const SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];
    let mut images = Vec::new();
    for size in SIZES {
        let png = path.with_file_name(format!("icon-{size}.png"));
        icon::save_png(&png, size)?;
        images.push(std::fs::read(&png).map_err(|e| e.to_string())?);
    }

    let mut out = vec![0, 0, 1, 0]; // reserved, type 1 (icon)
    out.extend((SIZES.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * SIZES.len() as u32;
    for (size, image) in SIZES.iter().zip(&images) {
        let side = if *size >= 256 { 0 } else { *size as u8 }; // 0 means 256
        out.extend([side, side, 0, 0]); // width, height, palette size, reserved
        out.extend(1u16.to_le_bytes()); // color planes
        out.extend(32u16.to_le_bytes()); // bits per pixel
        out.extend((image.len() as u32).to_le_bytes());
        out.extend(offset.to_le_bytes());
        offset += image.len() as u32;
    }
    for image in images {
        out.extend(image);
    }
    std::fs::write(path, out).map_err(|e| e.to_string())
}
