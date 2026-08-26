//! Small display helpers shared by the render layer.

use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::layout::Rect;

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn fmt_uptime(start: u64) -> String {
    if start == 0 {
        return "—".into();
    }
    let s = unix_now().saturating_sub(start);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}

pub(crate) fn fmt_mem(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1}GB", bytes as f64 / GB as f64)
    } else {
        format!("{}MB", bytes / MB)
    }
}

pub(crate) fn tildify(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_string(),
    }
}

/// A `w`×`h` rectangle centered within `area` (clamped to it).
pub(crate) fn centered_rect(w: u16, h: u16, area: Rect) -> Rect {
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w.min(area.width),
        height: h.min(area.height),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_scales_to_gb() {
        assert_eq!(fmt_mem(300 * 1024 * 1024), "300MB");
        assert_eq!(fmt_mem(1024 * 1024 * 1024 + 512 * 1024 * 1024), "1.5GB");
    }
}
