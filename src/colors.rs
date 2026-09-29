//! File categories and treemap color schemes.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Category {
    Video,
    Image,
    Audio,
    Archive,
    Document,
    Code,
    Executable,
    GameData,
    System,
    Other,
}

pub const CATEGORIES: [Category; 10] = [
    Category::Video,
    Category::Image,
    Category::Audio,
    Category::Archive,
    Category::Document,
    Category::Code,
    Category::Executable,
    Category::GameData,
    Category::System,
    Category::Other,
];

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Video => "Video",
            Category::Image => "Images",
            Category::Audio => "Audio",
            Category::Archive => "Archives & disk images",
            Category::Document => "Documents",
            Category::Code => "Code & text",
            Category::Executable => "Programs & libraries",
            Category::GameData => "Game & app data",
            Category::System => "System, cache & databases",
            Category::Other => "Other",
        }
    }

    pub fn color(self) -> [f32; 3] {
        match self {
            Category::Video => [0.93, 0.33, 0.33],
            Category::Image => [0.98, 0.72, 0.24],
            Category::Audio => [0.93, 0.45, 0.78],
            Category::Archive => [0.62, 0.45, 0.95],
            Category::Document => [0.30, 0.78, 0.95],
            Category::Code => [0.40, 0.85, 0.50],
            Category::Executable => [0.32, 0.52, 0.98],
            Category::GameData => [0.98, 0.52, 0.22],
            Category::System => [0.55, 0.62, 0.70],
            Category::Other => [0.62, 0.62, 0.62],
        }
    }
}

pub fn category_of(ext: &str) -> Category {
    use Category::*;
    match ext {
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "m4v" | "mpg" | "mpeg" | "ts" | "m2ts"
        | "mts" | "vob" | "3gp" | "ogv" | "rmvb" | "divx" | "prproj" | "braw" | "r3d" => Video,
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "tif" | "tiff" | "webp" | "heic" | "heif" | "avif" | "raw"
        | "cr2" | "cr3" | "nef" | "arw" | "dng" | "orf" | "rw2" | "psd" | "psb" | "svg" | "ico" | "xcf"
        | "kra" | "exr" | "hdr" | "tga" | "dds" | "jxl" | "blend" | "fbx" | "obj" | "ktx" | "ktx2" => Image,
        "mp3" | "flac" | "wav" | "aac" | "ogg" | "m4a" | "wma" | "opus" | "aiff" | "aif" | "alac" | "ape"
        | "mid" | "midi" | "als" | "flp" | "wv" | "dsf" | "m4b" => Audio,
        "zip" | "rar" | "7z" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "lz4" | "cab" | "iso" | "img"
        | "vhd" | "vhdx" | "vmdk" | "vdi" | "qcow2" | "wim" | "esd" | "dmg" | "swm" | "appx" | "msix" | "deb" | "rpm" | "appimage" | "snap" | "flatpak" | "pkg" | "xip" | "sparseimage" | "apkg"
        | "appxbundle" | "msixbundle" | "nupkg" | "whl" | "jar" | "apk" | "xapk" | "avhdx" | "tib" => Archive,
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "xlsm" | "ppt" | "pptx" | "odt" | "ods" | "odp" | "rtf"
        | "epub" | "mobi" | "azw3" | "one" | "pst" | "ost" | "msg" | "eml" | "pages" | "numbers" | "key"
        | "vsdx" | "pub" | "xps" | "djvu" | "cbz" | "cbr" => Document,
        "rs" | "c" | "cc" | "cpp" | "cxx" | "h" | "hpp" | "cs" | "js" | "mjs" | "cjs" | "jsx" | "tsx" | "py"
        | "pyc" | "java" | "class" | "kt" | "go" | "rb" | "php" | "html" | "htm" | "css" | "scss" | "json"
        | "xml" | "yaml" | "yml" | "toml" | "ini" | "cfg" | "conf" | "md" | "txt" | "log" | "csv" | "tsv"
        | "sql" | "sh" | "ps1" | "bat" | "cmd" | "lua" | "swift" | "m" | "vue" | "svelte" | "map" | "ipynb"
        | "rlib" | "rmeta" | "d" | "pdb" | "ilk" | "ipch" | "pch" | "tlog" | "o" | "lock" => Code,
        "exe" | "dll" | "sys" | "msi" | "msp" | "so" | "dylib" | "lib" | "a" | "ocx" | "drv" | "efi" | "mui"
        | "winmd" | "node" | "pyd" | "com" | "scr" | "cpl" | "ax" | "tlb" | "bin" | "elf" | "wasm" | "ko" => Executable,
        "pak" | "vpk" | "bundle" | "assets" | "resource" | "ress" | "bsa" | "ba2" | "esm" | "esp" | "uasset"
        | "ucas" | "utoc" | "umap" | "forge" | "pck" | "arc" | "big" | "wad" | "gcf" | "ncf" | "ff" | "rpf"
        | "unity3d" | "sav" | "save" | "nsp" | "xci" | "rom" | "gguf" | "safetensors" | "ckpt" | "pt"
        | "pth" | "onnx" | "h5" | "tflite" | "bnk" | "wem" | "vfs" => GameData,
        "tmp" | "temp" | "cache" | "dat" | "db" | "sqlite" | "sqlite3" | "db-wal" | "db-shm" | "etl" | "evtx"
        | "dmp" | "mdmp" | "hiberfil" | "edb" | "jrs" | "chk" | "blf" | "regtrans-ms" | "ldf" | "mdf"
        | "ndf" | "bak" | "old" | "idx" | "pf" | "cat" | "manifest" | "ldb" | "leveldb" | "journal" | "wal"
        | "pma" | "localstorage" | "bdic" | "ttf" | "otf" | "ttc" | "fon" | "woff" | "woff2" | "plist" | "swp" | "pacnew" | "rpmnew" | "dpkg-old" => System,
        _ => Other,
    }
}

/// Distinct colors handed to the largest extensions (by total size).
const EXT_PALETTE: [[f32; 3]; 14] = [
    [0.30, 0.55, 0.98],
    [0.95, 0.36, 0.32],
    [0.36, 0.82, 0.42],
    [0.98, 0.80, 0.26],
    [0.72, 0.44, 0.96],
    [0.26, 0.84, 0.86],
    [0.98, 0.56, 0.20],
    [0.96, 0.46, 0.76],
    [0.62, 0.86, 0.30],
    [0.48, 0.62, 0.99],
    [0.86, 0.66, 0.46],
    [0.40, 0.76, 0.66],
    [0.84, 0.34, 0.56],
    [0.72, 0.72, 0.36],
];

/// Per-extension colors: the top extensions get distinct palette colors,
/// everything else a muted version of its category color.
pub fn extension_colors(exts: &[crate::tree::ExtStat]) -> Vec<[f32; 3]> {
    let mut order: Vec<usize> = (0..exts.len()).collect();
    order.sort_unstable_by(|&a, &b| exts[b].size.cmp(&exts[a].size));
    let mut out: Vec<[f32; 3]> = exts.iter().map(|e| muted(e.category.color())).collect();
    let mut next = 0;
    for idx in order {
        if next >= EXT_PALETTE.len() || exts[idx].size == 0 {
            break;
        }
        if exts[idx].name.is_empty() {
            continue; // files without extension keep the neutral color
        }
        out[idx] = EXT_PALETTE[next];
        next += 1;
    }
    out
}

fn muted(c: [f32; 3]) -> [f32; 3] {
    let g = 0.58;
    [c[0] * 0.45 + g * 0.55, c[1] * 0.45 + g * 0.55, c[2] * 0.45 + g * 0.55]
}

pub const DIR_COLOR: [f32; 3] = [0.50, 0.50, 0.52];
pub const UNKNOWN_COLOR: [f32; 3] = [0.40, 0.40, 0.42];
pub const NEW_COLOR: [f32; 3] = [0.98, 0.34, 0.86];
pub const GREW_COLOR: [f32; 3] = [0.98, 0.30, 0.22];
pub const SHRANK_COLOR: [f32; 3] = [0.30, 0.86, 0.44];
pub const SAME_COLOR: [f32; 3] = [0.52, 0.53, 0.56];

/// Age gradient stops: brand new → very old.
const AGE_STOPS: [[f32; 3]; 5] = [
    [1.00, 0.42, 0.20],
    [1.00, 0.80, 0.28],
    [0.46, 0.86, 0.44],
    [0.30, 0.68, 0.92],
    [0.40, 0.38, 0.86],
];

/// `age_days` → color on a log scale (0 days … ~10 years).
pub fn age_color(age_days: f64) -> [f32; 3] {
    let t = ((1.0 + age_days.max(0.0)).ln() / (1.0f64 + 3650.0).ln()).clamp(0.0, 1.0) as f32;
    gradient(&AGE_STOPS, t)
}

pub const AGE_LEGEND: [(&str, f64); 6] =
    [("today", 0.0), ("1 week", 7.0), ("1 month", 30.0), ("6 months", 182.0), ("2 years", 730.0), ("10 years", 3650.0)];

/// Growth ratio in [-1, 1] → color.
pub fn growth_color(ratio: f32) -> [f32; 3] {
    let t = (ratio.abs() * 2.5).clamp(0.0, 1.0);
    let target = if ratio >= 0.0 { GREW_COLOR } else { SHRANK_COLOR };
    lerp(SAME_COLOR, target, t)
}

pub fn gradient(stops: &[[f32; 3]], t: f32) -> [f32; 3] {
    let x = t * (stops.len() - 1) as f32;
    let i = (x.floor() as usize).min(stops.len() - 2);
    lerp(stops[i], stops[i + 1], x - i as f32)
}

pub fn lerp(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

pub fn to_color32(c: [f32; 3]) -> eframe::egui::Color32 {
    eframe::egui::Color32::from_rgb((c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8)
}
