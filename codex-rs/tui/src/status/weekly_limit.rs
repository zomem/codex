use chrono::DateTime;
use chrono::Duration as ChronoDuration;
use chrono::Local;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;

use super::rate_limits::RateLimitWindowDisplay;

const WEEKLY_LIMIT_BAR_CELLS: usize = 20;
const WEEKLY_LIMIT_HOURS: f64 = 7.0 * 24.0;
const WEEKLY_LIMIT_PERCENT_PER_HOUR: f64 = 100.0 / WEEKLY_LIMIT_HOURS;
const WEEKLY_LIMIT_CELL_PERCENT: f64 = 100.0 / WEEKLY_LIMIT_BAR_CELLS as f64;
const WEEKLY_LIMIT_WARNING_PERCENT: f64 = 5.0;
const WEEKLY_LIMIT_CRITICAL_PERCENT: f64 = 10.0;
const EPSILON: f64 = 0.000_001;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WeeklyLimitBarStyle {
    Scheduled(WeeklyLimitReserveStyle),
    Surplus,
    Used,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WeeklyLimitReserveStyle {
    Healthy,
    Warning,
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
    let reset_remaining_hours =
        reset_remaining.map(|duration| duration.num_seconds() as f64 / 3_600.0);

    let scheduled_target_remaining = reset_remaining_hours
        .map(|hours| (hours.min(WEEKLY_LIMIT_HOURS) * WEEKLY_LIMIT_PERCENT_PER_HOUR).max(0.0))
        .unwrap_or(0.0)
        .min(100.0);
    let scheduled_remaining = scheduled_target_remaining.min(percent_remaining);
    let reserve_consumed = (scheduled_target_remaining - percent_remaining).max(0.0);
    let reserve_style = if reserve_consumed > WEEKLY_LIMIT_CRITICAL_PERCENT {
        WeeklyLimitReserveStyle::Critical
    } else if reserve_consumed + EPSILON >= WEEKLY_LIMIT_WARNING_PERCENT {
        WeeklyLimitReserveStyle::Warning
    } else {
        WeeklyLimitReserveStyle::Healthy
    };

    let mut spans = Vec::with_capacity(WEEKLY_LIMIT_BAR_CELLS + 2);
    spans.push("[".into());
    for index in 0..WEEKLY_LIMIT_BAR_CELLS {
        let cell_start = index as f64 * WEEKLY_LIMIT_CELL_PERCENT;
        let cell_remaining = (percent_remaining - cell_start).clamp(0.0, WEEKLY_LIMIT_CELL_PERCENT);
        let style = if cell_remaining <= 0.0 {
            WeeklyLimitBarStyle::Used
        } else {
            let visible_midpoint = cell_start + (cell_remaining / 2.0);
            if visible_midpoint <= scheduled_remaining + EPSILON {
                WeeklyLimitBarStyle::Scheduled(reserve_style)
            } else {
                WeeklyLimitBarStyle::Surplus
            }
        };
        spans.push(styled_bar_cell(cell_remaining, style));
    }
    spans.push("]".into());
    Line::from(spans)
}

fn styled_bar_cell(percent_remaining: f64, style: WeeklyLimitBarStyle) -> Span<'static> {
    let glyph = if percent_remaining <= 0.0 {
        " "
    } else {
        match percent_remaining.ceil() as i32 {
            1 => "▁",
            2 => "▂",
            3 => "▃",
            4 => "▄",
            _ => "▆",
        }
    };

    match style {
        WeeklyLimitBarStyle::Scheduled(WeeklyLimitReserveStyle::Healthy) => {
            Span::from(glyph).green().dim()
        }
        // User-requested warning color for eating into scheduled weekly reserve.
        #[allow(clippy::disallowed_methods)]
        WeeklyLimitBarStyle::Scheduled(WeeklyLimitReserveStyle::Warning) => {
            Span::from(glyph).yellow().dim()
        }
        WeeklyLimitBarStyle::Scheduled(WeeklyLimitReserveStyle::Critical) => {
            Span::from(glyph).red().dim()
        }
        WeeklyLimitBarStyle::Surplus | WeeklyLimitBarStyle::Used => Span::from(glyph),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use ratatui::style::Color;
    use ratatui::style::Modifier;

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
    fn renders_remaining_blocks_and_used_spaces() {
        let now = Local
            .with_ymd_and_hms(2026, 4, 30, 12, 0, 0)
            .single()
            .expect("timestamp");
        let line = weekly_limit_status_line(
            &weekly_window(/*used_percent*/ 35.0, /*reset_at*/ None),
            now,
        );

        assert_eq!(line_text(&line), "[▆▆▆▆▆▆▆▆▆▆▆▆▆       ]");
    }

    #[test]
    fn renders_partial_remaining_cell() {
        let now = Local
            .with_ymd_and_hms(2026, 4, 30, 12, 0, 0)
            .single()
            .expect("timestamp");
        let line = weekly_limit_status_line(
            &weekly_window(/*used_percent*/ 36.0, /*reset_at*/ None),
            now,
        );

        assert_eq!(line_text(&line), "[▆▆▆▆▆▆▆▆▆▆▆▆▄       ]");
    }

    #[test]
    fn colors_scheduled_remaining_time_and_surplus() {
        let now = Local
            .with_ymd_and_hms(2026, 4, 30, 12, 0, 0)
            .single()
            .expect("timestamp");
        let reset_at = now + ChronoDuration::hours(72);
        let line =
            weekly_limit_status_line(&weekly_window(/*used_percent*/ 35.0, Some(reset_at)), now);

        let cells = &line.spans[1..=WEEKLY_LIMIT_BAR_CELLS];
        assert_eq!(cells[0].style.fg, Some(Color::Green));
        assert!(cells[0].style.add_modifier.contains(Modifier::DIM));
        assert_eq!(cells[6].style.fg, Some(Color::Green));
        assert!(cells[6].style.add_modifier.contains(Modifier::DIM));
        assert_eq!(cells[10].style.fg, None);
        assert!(!cells[10].style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn turns_scheduled_region_yellow_after_consuming_five_percent_of_reserve() {
        let now = Local
            .with_ymd_and_hms(2026, 4, 30, 12, 0, 0)
            .single()
            .expect("timestamp");
        let reset_at = now + ChronoDuration::hours(84);
        let line =
            weekly_limit_status_line(&weekly_window(/*used_percent*/ 55.0, Some(reset_at)), now);

        let cells = &line.spans[1..=WEEKLY_LIMIT_BAR_CELLS];
        assert_eq!(cells[0].style.fg, Some(Color::Yellow));
        assert!(cells[0].style.add_modifier.contains(Modifier::DIM));
        assert_eq!(cells[8].style.fg, Some(Color::Yellow));
        assert!(cells[8].style.add_modifier.contains(Modifier::DIM));
        assert_eq!(cells[9].style.fg, None);
    }

    #[test]
    fn turns_scheduled_region_red_after_consuming_more_than_ten_percent_of_reserve() {
        let now = Local
            .with_ymd_and_hms(2026, 4, 30, 12, 0, 0)
            .single()
            .expect("timestamp");
        let reset_at = now + ChronoDuration::hours(84);
        let line =
            weekly_limit_status_line(&weekly_window(/*used_percent*/ 61.0, Some(reset_at)), now);

        let cells = &line.spans[1..=WEEKLY_LIMIT_BAR_CELLS];
        assert_eq!(cells[0].style.fg, Some(Color::Red));
        assert!(cells[0].style.add_modifier.contains(Modifier::DIM));
        assert_eq!(cells[7].style.fg, Some(Color::Red));
        assert!(cells[7].style.add_modifier.contains(Modifier::DIM));
        assert_eq!(cells[8].style.fg, None);
    }
}
