//! Formatting helpers.

pub fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["KB", "MB", "GB", "TB", "PB", "EB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64 / 1024.0;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else if v >= 10.0 {
        format!("{v:.1} {}", UNITS[u])
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}

pub fn fmt_delta(delta: i64) -> String {
    if delta == 0 {
        return "±0".into();
    }
    let sign = if delta > 0 { '+' } else { '−' };
    format!("{sign}{}", fmt_size(delta.unsigned_abs()))
}

pub fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn fmt_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        format!("{}m {:02}s", ms / 60_000, (ms / 1000) % 60)
    }
}

pub fn fmt_ago(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=86_399 => format!("{} h ago", s / 3600),
        86_400..=2_591_999 => format!("{} days ago", s / 86_400),
        2_592_000..=31_535_999 => format!("{} months ago", s / 2_592_000),
        _ => format!("{:.1} years ago", s as f64 / 31_536_000.0),
    }
}

pub fn pct(part: u64, whole: u64) -> f32 {
    if whole == 0 { 0.0 } else { (part as f64 / whole as f64 * 100.0) as f32 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(fmt_size(0), "0 B");
        assert_eq!(fmt_size(1023), "1023 B");
        assert_eq!(fmt_size(1536), "1.50 KB");
        assert_eq!(fmt_size(15 * 1024 * 1024), "15.0 MB");
        assert_eq!(fmt_size(512 * 1024 * 1024 * 1024), "512 GB");
        assert_eq!(fmt_count(1234567), "1,234,567");
        assert_eq!(fmt_count(123), "123");
        assert_eq!(fmt_delta(-2048), "−2.00 KB");
    }
}
