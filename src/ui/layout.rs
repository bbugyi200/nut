use std::time::Duration;

pub fn terminal_width(columns: Option<&str>, terminal_columns: Option<usize>) -> usize {
    columns
        .and_then(|value| value.trim().parse::<usize>().ok())
        .or(terminal_columns)
        .unwrap_or(80)
        .clamp(60, 120)
}

pub fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 3600 {
        format!("{}h {:02}m", seconds / 3600, (seconds % 3600) / 60)
    } else if seconds >= 60 && !seconds.is_multiple_of(60) {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else if seconds >= 60 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

pub fn render_bar(value: f64, width: usize, ascii: bool) -> String {
    if width == 0 {
        return String::new();
    }
    let ratio = if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    };
    if ascii {
        let filled = (ratio * width as f64).round() as usize;
        return format!("{}{}", "#".repeat(filled), ".".repeat(width - filled));
    }
    const BLOCKS: [&str; 9] = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];
    let eighths = (ratio * width as f64 * 8.0).round() as usize;
    let complete = eighths / 8;
    let partial = eighths % 8;
    let mut output = "█".repeat(complete);
    if partial > 0 && complete < width {
        output.push_str(BLOCKS[partial]);
    }
    output.push_str(&" ".repeat(width.saturating_sub(complete + usize::from(partial > 0))));
    output
}

pub fn label_value(label: &str, value: &str, width: usize) -> String {
    const LABEL_COLUMN: usize = 16;
    let available = width.saturating_sub(LABEL_COLUMN).max(1);
    let clipped: String = value.chars().take(available).collect();
    format!("{label:<LABEL_COLUMN$}{clipped}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_are_readable_and_bounded() {
        assert_eq!(terminal_width(None, None), 80);
        assert_eq!(terminal_width(Some("40"), None), 60);
        assert_eq!(terminal_width(Some("160"), None), 120);
        assert_eq!(terminal_width(Some("invalid"), Some(95)), 95);
    }

    #[test]
    fn durations_read_like_speech() {
        assert_eq!(format_duration(Duration::from_secs(4)), "4s");
        assert_eq!(format_duration(Duration::from_secs(1550)), "25m 50s");
        assert_eq!(format_duration(Duration::from_secs(2700)), "45m");
        assert_eq!(format_duration(Duration::from_secs(3720)), "1h 02m");
    }

    #[test]
    fn bars_handle_clamping_and_ascii() {
        assert_eq!(render_bar(0.5, 8, true), "####....");
        assert_eq!(render_bar(2.0, 4, true), "####");
        assert_eq!(render_bar(-1.0, 4, false), "    ");
        assert_eq!(render_bar(0.5, 4, false).chars().count(), 4);
    }
}
