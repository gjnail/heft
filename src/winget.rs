//! Software updates through winget (the Windows Package Manager).
//!
//! Heft only reads the list of available upgrades itself. Installing runs
//! winget in a visible console window, so installer prompts and license
//! agreements are shown to the user rather than accepted silently.

use std::os::windows::process::CommandExt;

use crate::winsys;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upgrade {
    pub name: String,
    pub id: String,
    pub version: String,
    pub available: String,
    pub source: String,
}

impl Upgrade {
    /// winget shortens long ids with "…"; those can't be targeted exactly.
    pub fn id_is_exact(&self) -> bool {
        !self.id.ends_with('…') && !self.id.is_empty()
    }
}

pub fn available() -> bool {
    std::process::Command::new("winget.exe")
        .arg("--version")
        .creation_flags(winsys::CREATE_NO_WINDOW)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Ask winget which installed packages have upgrades.
pub fn list_upgrades() -> Result<Vec<Upgrade>, String> {
    let out = std::process::Command::new("winget.exe")
        .args(["upgrade", "--include-unknown", "--disable-interactivity"])
        .creation_flags(winsys::CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("winget isn't available: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let ups = parse(&text);
    if ups.is_empty() && !out.status.success() {
        // Surface winget's own explanation (e.g. source agreements pending).
        let last = text.lines().map(clean_line).rfind(|l| !l.trim().is_empty()).unwrap_or_default();
        return Err(if last.is_empty() { "winget failed".into() } else { last.trim().to_string() });
    }
    Ok(ups)
}

/// Upgrade one package in a console window.
pub fn upgrade(id: &str) -> std::io::Result<std::process::Child> {
    winsys::run_in_console("Heft - updating", &format!("winget upgrade --id \"{id}\" --exact"))
}

pub fn upgrade_all() -> std::io::Result<std::process::Child> {
    winsys::run_in_console("Heft - updating", "winget upgrade --all --include-unknown")
}

/// Progress spinners are redrawn with carriage returns; keep what's visible.
fn clean_line(line: &str) -> &str {
    line.rsplit('\r').find(|s| !s.trim().is_empty()).unwrap_or("")
}

/// Terminal columns a character occupies.
fn width(c: char) -> usize {
    let u = c as u32;
    let wide = matches!(u,
        0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 | 0x1F300..=0x1FAFF | 0x20000..=0x3FFFD);
    if wide { 2 } else { 1 }
}

/// Split a row at the header's column positions (in terminal columns).
fn cut(row: &str, starts: &[usize]) -> Vec<String> {
    let mut fields = vec![String::new(); starts.len()];
    let mut col = 0;
    for c in row.chars() {
        let i = starts.iter().rposition(|&s| s <= col).unwrap_or(0);
        fields[i].push(c);
        col += width(c);
    }
    fields.into_iter().map(|f| f.trim().to_string()).collect()
}

/// Parse `winget upgrade` tables. Column positions come from each header, so
/// localized headers work too.
pub fn parse(text: &str) -> Vec<Upgrade> {
    let lines: Vec<&str> = text.lines().map(clean_line).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < lines.len() {
        let rule = lines[i + 1].trim();
        if rule.len() < 10 || !rule.chars().all(|c| c == '-') {
            i += 1;
            continue;
        }
        let header = lines[i];
        let mut starts = Vec::new();
        let mut col = 0;
        let mut prev_space = true;
        for c in header.chars() {
            if !c.is_whitespace() && prev_space {
                starts.push(col);
            }
            prev_space = c.is_whitespace();
            col += width(c);
        }
        i += 2;
        if starts.len() < 4 {
            continue;
        }
        while i < lines.len() {
            let row = lines[i];
            let row_width: usize = row.chars().map(width).sum();
            if row.trim().is_empty() || row_width <= starts[2] {
                break;
            }
            let f = cut(row, &starts);
            if !f[1].is_empty() {
                out.push(Upgrade {
                    name: f[0].clone(),
                    id: f[1].clone(),
                    version: f[2].clone(),
                    available: f[3].clone(),
                    source: f.get(4).cloned().unwrap_or_default(),
                });
            }
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pad cells to terminal-column widths, like winget does.
    fn table(widths: &[usize], rows: &[&[&str]]) -> String {
        let mut out = String::new();
        for (i, r) in rows.iter().enumerate() {
            for (cell, w) in r.iter().zip(widths) {
                let used: usize = cell.chars().map(width).sum();
                out.push_str(cell);
                out.push_str(&" ".repeat(w.saturating_sub(used)));
            }
            out = out.trim_end().to_string() + "\n";
            if i == 0 {
                out.push_str(&"-".repeat(widths.iter().sum()));
                out.push('\n');
            }
        }
        out
    }

    #[test]
    fn parses_tables() {
        let w = [23, 19, 15, 15, 6];
        // A progress spinner redrawn with carriage returns precedes the table.
        let mut text = String::from("   - \r   \\ \r");
        text += &table(&w, &[
            &["Name", "Id", "Version", "Available", "Source"],
            &["7-Zip 24.06 (x64)", "7zip.7zip", "24.06", "26.03", "winget"],
            &["Unity Hub 3.15.1", "Unity.UnityHub", "< 3.21.0.65535", "3.21.3.65535", "winget"],
            &["記事 App", "Some.Wide", "1.0", "2.0", "winget"],
            &["Long Name That Is Cut…", "Publisher.VeryLon…", "1.0", "1.1", "winget"],
        ]);
        text += "4 upgrades available.\n\nThe following packages require explicit targeting:\n";
        text += &table(&[10, 16, 8, 10, 6], &[
            &["Name", "Id", "Version", "Available", "Source"],
            &["Discord", "Discord.Discord", "1.0", "1.1", "winget"],
        ]);
        let u = parse(&text);
        assert_eq!(u.len(), 5, "{u:#?}");
        assert_eq!(
            u[0],
            Upgrade { name: "7-Zip 24.06 (x64)".into(), id: "7zip.7zip".into(), version: "24.06".into(), available: "26.03".into(), source: "winget".into() }
        );
        assert_eq!(u[1].version, "< 3.21.0.65535");
        assert_eq!(u[2].name, "記事 App");
        assert_eq!(u[2].id, "Some.Wide");
        assert!(!u[3].id_is_exact());
        assert_eq!(u[4].id, "Discord.Discord");
        assert_eq!(u[4].available, "1.1");
    }
}
