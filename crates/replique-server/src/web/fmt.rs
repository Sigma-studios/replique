//! How numbers and times read on the website and in Discord messages.

use crate::db;

/// Decimal units, like the Hetzner Console and restic's own output.
pub fn bytes(n: i64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = n as f64;
    let mut i = 0;
    while v.abs() >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// `3 h 12 min`, `2 d 4 h`, `45 s`: two units at most.
pub fn duration(secs: i64) -> String {
    let s = secs.max(0);
    let (d, h, m) = (s / 86_400, (s % 86_400) / 3600, (s % 3600) / 60);
    if d > 0 {
        if h > 0 {
            format!("{d} d {h} h")
        } else {
            format!("{d} d")
        }
    } else if h > 0 {
        if m > 0 {
            format!("{h} h {m} min")
        } else {
            format!("{h} h")
        }
    } else if m > 0 {
        format!("{m} min")
    } else {
        format!("{s} s")
    }
}

/// UTC, which is what the server knows; the page title says so.
pub fn datetime(secs: i64) -> String {
    db::to_time(secs).format("%Y-%m-%d %H:%M UTC").to_string()
}

/// A relative time ("5 min ago") that shows the exact time when hovered. The
/// site's rule for every relative time: "ago" alone cannot be placed.
pub fn when(secs: i64) -> maud::Markup {
    let exact = db::to_time(secs);
    maud::html! {
        time datetime=(exact.to_rfc3339()) title=(exact.format("%Y-%m-%d %H:%M:%S UTC").to_string()) { (ago(secs)) }
    }
}

pub fn ago(secs: i64) -> String {
    let d = db::now() - secs;
    if d < 0 {
        return format!("in {}", duration(-d));
    }
    if d < 60 {
        return "just now".into();
    }
    format!("{} ago", duration(d))
}

pub fn percent(fraction: f64) -> String {
    format!("{:.0}%", (fraction * 100.0).clamp(0.0, 100.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(1_500_000), "1.5 MB");
        assert_eq!(bytes(452_381_234_567), "452 GB");
        assert_eq!(duration(26 * 3600), "1 d 2 h");
        assert_eq!(duration(3 * 3600 + 12 * 60), "3 h 12 min");
        assert_eq!(duration(42), "42 s");
    }
}
