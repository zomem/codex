use chrono::DateTime;
use chrono::Duration as ChronoDuration;
use chrono::Local;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;

use super::rate_limits::RateLimitWindowDisplay;

const WEEKLY_LIMIT_BAR_DAYS: usize = 7;
const WEEKLY_LIMIT_DAY_GLYPHS: [&str; 6] = ["▁", "▂", "▃", "▄", "▅", "▆"];
const WEEKLY_LIMIT_HOURS: f64 = 7.0 * 24.0;
const WEEKLY_LIMIT_DAY_PERCENT: f64 = 100.0 / WEEKLY_LIMIT_BAR_DAYS as f64;
const WEEKLY_LIMIT_GREEN_THRESHOLD_HOURS: f64 = -12.0;
const WEEKLY_LIMIT_YELLOW_THRESHOLD_HOURS: f64 = -36.0;
const WEEKLY_LIMIT_RED_THRESHOLD_HOURS: f64 = -60.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WeeklyLimitBarStyle {
    Remaining(WeeklyLimitRemainingStyle),
    Used,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WeeklyLimitRemainingStyle {
    Green,
    Warning,
    Red,
    Critical,
}

pub(crate) fn weekly_limit_status_line(
    window: &RateLimitWindowDisplay,
    now: DateTime<Local>,
) -> Line<'static> {
    let percent_remaining = (100.0 - window.used_percent).clamp(0.0, 100.0);
    let reset_remaining = window.reset_at.map(|reset_at| {
        let remaining = reset_at.signed_duration_since(now);
        remaining.max(ChronoDuration::zero())
    });
    let reset_remaining_total_hours = reset_remaining
        .map(|duration| duration.num_seconds().max(0) / 3_600)
        .unwrap_or(0);
    let reset_remaining_text = format!(
        "{}d {}h",
        reset_remaining_total_hours / 24,
        reset_remaining_total_hours % 24
    );
    let reset_remaining_hours =
        reset_remaining.map(|duration| duration.num_seconds() as f64 / 3_600.0);
    let remaining_hours = percent_remaining / 100.0 * WEEKLY_LIMIT_HOURS;
    let reset_remaining_hours = reset_remaining_hours.unwrap_or(0.0);
    let remaining_minus_reset_hours = remaining_hours - reset_remaining_hours;
    let remaining_style = if remaining_minus_reset_hours > WEEKLY_LIMIT_GREEN_THRESHOLD_HOURS {
        WeeklyLimitRemainingStyle::Green
    } else if remaining_minus_reset_hours > WEEKLY_LIMIT_YELLOW_THRESHOLD_HOURS {
        WeeklyLimitRemainingStyle::Warning
    } else if remaining_minus_reset_hours > WEEKLY_LIMIT_RED_THRESHOLD_HOURS {
        WeeklyLimitRemainingStyle::Red
    } else {
        WeeklyLimitRemainingStyle::Critical
    };

    let mut spans = Vec::with_capacity(WEEKLY_LIMIT_BAR_DAYS * 2 + 1);
    for index in 0..WEEKLY_LIMIT_BAR_DAYS {
        if index > 0 {
            spans.push(" ".into());
        }

        let cell_start = index as f64 * WEEKLY_LIMIT_DAY_PERCENT;
        let cell_remaining = (percent_remaining - cell_start).clamp(0.0, WEEKLY_LIMIT_DAY_PERCENT);
        let style = if cell_remaining > 0.0 {
            WeeklyLimitBarStyle::Remaining(remaining_style)
        } else {
            WeeklyLimitBarStyle::Used
        };
        spans.push(styled_bar_cell(cell_remaining, style));
    }
    spans.push(format!(" {reset_remaining_text}").into());
    Line::from(spans)
}

fn styled_bar_cell(percent_remaining: f64, style: WeeklyLimitBarStyle) -> Span<'static> {
    let glyph = if percent_remaining <= 0.0 {
        "▆"
    } else {
        let level = ((percent_remaining / WEEKLY_LIMIT_DAY_PERCENT).clamp(0.0, 1.0)
            * WEEKLY_LIMIT_DAY_GLYPHS.len() as f64)
            .round() as usize;
        WEEKLY_LIMIT_DAY_GLYPHS[level
            .max(1)
            .saturating_sub(1)
            .min(WEEKLY_LIMIT_DAY_GLYPHS.len() - 1)]
    };

    match style {
        WeeklyLimitBarStyle::Remaining(WeeklyLimitRemainingStyle::Green) => {
            Span::from(glyph).green()
        }
        #[allow(clippy::disallowed_methods)]
        WeeklyLimitBarStyle::Remaining(WeeklyLimitRemainingStyle::Warning) => {
            Span::from(glyph).yellow()
        }
        WeeklyLimitBarStyle::Remaining(
            WeeklyLimitRemainingStyle::Red | WeeklyLimitRemainingStyle::Critical,
        ) => Span::from(glyph).red(),
        WeeklyLimitBarStyle::Used => Span::from(glyph),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use pretty_assertions::assert_eq;
    use ratatui::style::Color;

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn weekly_window(
        used_percent: f64,
        reset_at: Option<DateTime<Local>>,
    ) -> RateLimitWindowDisplay {
        RateLimitWindowDisplay {
            used_percent,
            resets_at: None,
            reset_at,
            window_minutes: Some(10_080),
        }
    }

    #[test]
    fn renders_remaining_days_and_used_days() {
        let now = Local
            .with_ymd_and_hms(2026, 4, 30, 12, 0, 0)
            .single()
            .expect("timestamp");
        let line = weekly_limit_status_line(
            &weekly_window(/*used_percent*/ 35.0, /*reset_at*/ None),
            now,
        );

        assert_eq!(line_text(&line), "▆ ▆ ▆ ▆ ▃ ▆ ▆ 0d 0h");
    }

    #[test]
    fn renders_partial_remaining_day_and_reset_countdown() {
        let now = Local
            .with_ymd_and_hms(2026, 4, 30, 12, 0, 0)
            .single()
            .expect("timestamp");
        let reset_at = now + ChronoDuration::days(5) + ChronoDuration::hours(23);
        let line =
            weekly_limit_status_line(&weekly_window(/*used_percent*/ 65.0, Some(reset_at)), now);

        assert_eq!(line_text(&line), "▆ ▆ ▃ ▆ ▆ ▆ ▆ 5d 23h");
    }

    #[test]
    fn clamps_past_reset_countdown_to_zero() {
        let now = Local
            .with_ymd_and_hms(2026, 4, 30, 12, 0, 0)
            .single()
            .expect("timestamp");
        let reset_at = now - ChronoDuration::hours(2);
        let line =
            weekly_limit_status_line(&weekly_window(/*used_percent*/ 100.0, Some(reset_at)), now);

        assert_eq!(line_text(&line), "▆ ▆ ▆ ▆ ▆ ▆ ▆ 0d 0h");
    }

    #[test]
    fn colors_remaining_green_when_remaining_time_is_within_half_day_of_reset() {
        let now = Local
            .with_ymd_and_hms(2026, 4, 30, 12, 0, 0)
            .single()
            .expect("timestamp");
        let reset_at = now + ChronoDuration::hours(68);
        let line =
            weekly_limit_status_line(&weekly_window(/*used_percent*/ 65.0, Some(reset_at)), now);

        let cells: Vec<&Span<'_>> = line
            .spans
            .iter()
            .filter(|span| span.content.as_ref() != " ")
            .take(WEEKLY_LIMIT_BAR_DAYS)
            .collect();
        assert_eq!(cells[0].style.fg, Some(Color::Green));
        assert_eq!(cells[2].style.fg, Some(Color::Green));
        assert_eq!(cells[3].style.fg, None);
    }

    #[test]
    fn colors_remaining_yellow_when_remaining_time_is_within_one_and_half_days_of_reset() {
        let now = Local
            .with_ymd_and_hms(2026, 4, 30, 12, 0, 0)
            .single()
            .expect("timestamp");
        let reset_at = now + ChronoDuration::hours(90);
        let line =
            weekly_limit_status_line(&weekly_window(/*used_percent*/ 65.0, Some(reset_at)), now);

        let cells: Vec<&Span<'_>> = line
            .spans
            .iter()
            .filter(|span| span.content.as_ref() != " ")
            .take(WEEKLY_LIMIT_BAR_DAYS)
            .collect();
        assert_eq!(cells[0].style.fg, Some(Color::Yellow));
        assert_eq!(cells[2].style.fg, Some(Color::Yellow));
        assert_eq!(cells[3].style.fg, None);
    }

    #[test]
    fn colors_remaining_red_when_remaining_time_is_within_two_and_half_days_of_reset() {
        let now = Local
            .with_ymd_and_hms(2026, 4, 30, 12, 0, 0)
            .single()
            .expect("timestamp");
        let reset_at = now + ChronoDuration::hours(110);
        let line =
            weekly_limit_status_line(&weekly_window(/*used_percent*/ 65.0, Some(reset_at)), now);

        let cells: Vec<&Span<'_>> = line
            .spans
            .iter()
            .filter(|span| span.content.as_ref() != " ")
            .take(WEEKLY_LIMIT_BAR_DAYS)
            .collect();
        assert_eq!(cells[0].style.fg, Some(Color::Red));
        assert_eq!(cells[2].style.fg, Some(Color::Red));
        assert_eq!(cells[3].style.fg, None);
    }

    #[test]
    fn colors_remaining_red_when_remaining_time_lags_past_two_and_half_days() {
        let now = Local
            .with_ymd_and_hms(2026, 4, 30, 12, 0, 0)
            .single()
            .expect("timestamp");
        let reset_at = now + ChronoDuration::hours(120);
        let line =
            weekly_limit_status_line(&weekly_window(/*used_percent*/ 65.0, Some(reset_at)), now);

        let cells: Vec<&Span<'_>> = line
            .spans
            .iter()
            .filter(|span| span.content.as_ref() != " ")
            .take(WEEKLY_LIMIT_BAR_DAYS)
            .collect();
        assert_eq!(cells[0].style.fg, Some(Color::Red));
        assert_eq!(cells[2].style.fg, Some(Color::Red));
        assert_eq!(cells[3].style.fg, None);
    }
}
