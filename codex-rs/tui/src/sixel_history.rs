use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use std::collections::HashMap;
use std::io::Write as _;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use unicode_width::UnicodeWidthStr;

use icy_sixel::BackgroundMode;
use icy_sixel::EncodeOptions;
use icy_sixel::SixelImage;

const SIXEL_LINE_PREFIX: &str = "\u{e000}codex-sixel:";
const SIXEL_RESERVE_PREFIX: &str = "\u{e000}codex-sixel-reserve:";
const SAVE_CURSOR: &str = "\x1b7";
const RESTORE_CURSOR: &str = "\x1b8";
const SIXEL_CURSOR_TO_THE_RIGHT_ON: &str = "\x1b[?8452h";
const SIXEL_CURSOR_TO_THE_RIGHT_OFF: &str = "\x1b[?8452l";
const SIXEL_DEBUG_LOG_ENV: &str = "CODEX_SIXEL_DEBUG_LOG";

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct SixelCellSize {
    pub(crate) width: u16,
    pub(crate) height: u16,
}

#[derive(Clone, Debug)]
pub(crate) struct SixelHistoryImage {
    pub(crate) data: String,
    pub(crate) columns: u16,
    pub(crate) cell_size: SixelCellSize,
    pub(crate) clear_background: (u8, u8, u8),
    pub(crate) is_tmux: bool,
    pub(crate) redraw_on_scroll: bool,
    pub(crate) rows: u16,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct SixelClearKey {
    columns: u16,
    rows: u16,
    cell_width: u16,
    cell_height: u16,
    background: (u8, u8, u8),
    is_tmux: bool,
}

static NEXT_SIXEL_IMAGE_ID: AtomicU64 = AtomicU64::new(1);
static SIXEL_HISTORY_IMAGES: LazyLock<Mutex<HashMap<u64, SixelHistoryImage>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static SIXEL_CLEAR_IMAGES: LazyLock<Mutex<HashMap<SixelClearKey, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn sixel_debug_log(args: std::fmt::Arguments<'_>) {
    let Ok(path) = std::env::var(SIXEL_DEBUG_LOG_ENV) else {
        return;
    };
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let _ = writeln!(file, "[{timestamp}] {args}");
}

pub(crate) fn sixel_to_history_lines(
    data: String,
    area: Rect,
    cell_size: SixelCellSize,
    clear_background: (u8, u8, u8),
    is_tmux: bool,
    redraw_on_scroll: bool,
) -> Vec<Line<'static>> {
    let id = NEXT_SIXEL_IMAGE_ID.fetch_add(1, Ordering::Relaxed);
    sixel_debug_log(format_args!(
        "history-store id={id} area={}x{} cell={}x{} bg=#{:02x}{:02x}{:02x} tmux={} data_bytes={}",
        area.width,
        area.height,
        cell_size.width,
        cell_size.height,
        clear_background.0,
        clear_background.1,
        clear_background.2,
        is_tmux,
        data.len()
    ));
    let wraps_sixel_data = data.contains("\x1bP") || data.contains('\u{90}');
    let data = if wraps_sixel_data {
        let clear_area = sixel_clear_area(area);
        format!(
            "{SAVE_CURSOR}{SIXEL_CURSOR_TO_THE_RIGHT_ON}{clear_area}{data}{SIXEL_CURSOR_TO_THE_RIGHT_OFF}{RESTORE_CURSOR}"
        )
    } else {
        data
    };

    if let Ok(mut images) = SIXEL_HISTORY_IMAGES.lock() {
        images.insert(
            id,
            SixelHistoryImage {
                data,
                columns: area.width.max(1),
                cell_size,
                clear_background,
                is_tmux,
                redraw_on_scroll,
                rows: area.height.max(1),
            },
        );
    }
    let mut lines = Vec::with_capacity(area.height as usize);
    lines.push(Line::from(Span::raw(format!("{SIXEL_LINE_PREFIX}{id}"))));
    for _ in 1..area.height {
        lines.push(Line::from(Span::raw(format!("{SIXEL_RESERVE_PREFIX}{id}"))));
    }
    lines
}

fn sixel_clear_area(area: Rect) -> String {
    let mut data = String::new();
    for _ in 0..area.height {
        data.push_str("\x1b[");
        data.push_str(&area.width.to_string());
        data.push_str("X\x1b[1B");
    }
    data.push_str("\x1b[");
    data.push_str(&area.height.to_string());
    data.push('A');
    data
}

pub(crate) fn sixel_clear_area_sequence(
    area: Rect,
    cell_size: SixelCellSize,
    background: (u8, u8, u8),
    is_tmux: bool,
) -> String {
    if area.width == 0 || area.height == 0 {
        return String::new();
    }
    sixel_debug_log(format_args!(
        "clear-request area={}x{} cell={}x{} bg=#{:02x}{:02x}{:02x} tmux={}",
        area.width,
        area.height,
        cell_size.width,
        cell_size.height,
        background.0,
        background.1,
        background.2,
        is_tmux
    ));
    let clear = sixel_clear_image(area, cell_size, background, is_tmux)
        .unwrap_or_else(|| sixel_clear_area(area));
    wrap_cursor_side_effect(&clear)
}

fn sixel_clear_image(
    area: Rect,
    cell_size: SixelCellSize,
    background: (u8, u8, u8),
    is_tmux: bool,
) -> Option<String> {
    let key = SixelClearKey {
        columns: area.width,
        rows: area.height,
        cell_width: cell_size.width,
        cell_height: cell_size.height,
        background,
        is_tmux,
    };
    if let Some(data) = SIXEL_CLEAR_IMAGES.lock().ok()?.get(&key).cloned() {
        sixel_debug_log(format_args!(
            "clear-image-cache-hit area={}x{} cell={}x{} bytes={}",
            area.width,
            area.height,
            cell_size.width,
            cell_size.height,
            data.len()
        ));
        return Some(data);
    }

    let width = usize::from(area.width) * usize::from(cell_size.width.max(1));
    let height = usize::from(area.height) * usize::from(cell_size.height.max(1));
    let data = encode_solid_sixel(width, height, background, is_tmux)?;
    sixel_debug_log(format_args!(
        "clear-image-cache-miss area={}x{} pixels={}x{} bytes={}",
        area.width,
        area.height,
        width,
        height,
        data.len()
    ));
    SIXEL_CLEAR_IMAGES.lock().ok()?.insert(key, data.clone());
    Some(data)
}

fn encode_solid_sixel(
    width: usize,
    height: usize,
    background: (u8, u8, u8),
    is_tmux: bool,
) -> Option<String> {
    let pixel_count = width.checked_mul(height)?;
    let byte_count = pixel_count.checked_mul(4)?;
    let mut pixels = Vec::with_capacity(byte_count);
    for _ in 0..pixel_count {
        pixels.extend_from_slice(&[background.0, background.1, background.2, 255]);
    }

    let encode_options = EncodeOptions {
        diffusion: 0.0,
        max_colors: 2,
        ..Default::default()
    };
    let mut data = SixelImage::from_rgba(pixels, width, height)
        .with_background_mode(BackgroundMode::Opaque)
        .encode_with(&encode_options)
        .ok()?;
    insert_sixel_raster_attributes(&mut data, width, height)?;
    if is_tmux {
        escape_sixel_for_tmux(&mut data)?;
    }
    Some(data)
}

pub(crate) fn insert_sixel_raster_attributes(
    data: &mut String,
    width: usize,
    height: usize,
) -> Option<()> {
    let payload_start = data.find('q')? + 1;
    data.insert_str(payload_start, &format!("\"1;1;{width};{height}"));
    Some(())
}

pub(crate) fn escape_sixel_for_tmux(data: &mut String) -> Option<()> {
    data.strip_prefix('\x1b')?;
    data.insert_str(0, "\x1b\x1b");
    data.insert_str(0, "\x1bPtmux;");
    data.push_str("\x1b\\");
    Some(())
}

fn wrap_cursor_side_effect(data: &str) -> String {
    format!(
        "{SAVE_CURSOR}{SIXEL_CURSOR_TO_THE_RIGHT_ON}{data}{SIXEL_CURSOR_TO_THE_RIGHT_OFF}{RESTORE_CURSOR}"
    )
}

pub(crate) fn sixel_history_image_from_line(line: &Line<'_>) -> Option<SixelHistoryImage> {
    sixel_history_image_with_prefix_width_from_line(line).map(|(image, _)| image)
}

pub(crate) fn sixel_history_image_with_prefix_width_from_line(
    line: &Line<'_>,
) -> Option<(SixelHistoryImage, u16)> {
    if line.style != Style::default() {
        return None;
    }
    let content = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    let marker_start = content.find(SIXEL_LINE_PREFIX)?;
    let id = content[marker_start..]
        .strip_prefix(SIXEL_LINE_PREFIX)?
        .parse()
        .ok()?;
    let prefix_width = UnicodeWidthStr::width(&content[..marker_start]);
    let prefix_width = u16::try_from(prefix_width).unwrap_or(u16::MAX);
    let image = SIXEL_HISTORY_IMAGES.lock().ok()?.get(&id).cloned()?;
    Some((image, prefix_width))
}

pub(crate) fn is_sixel_history_reserve_line(line: &Line<'_>) -> bool {
    sixel_line_id(line, SIXEL_RESERVE_PREFIX).is_some()
}

fn sixel_line_id(line: &Line<'_>, prefix: &str) -> Option<u64> {
    if line.style != Style::default() {
        return None;
    }
    let content = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    content.trim_start().strip_prefix(prefix)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cell_size() -> SixelCellSize {
        SixelCellSize {
            width: 1,
            height: 1,
        }
    }

    fn history_lines(data: &str, area: Rect) -> Vec<Line<'static>> {
        sixel_to_history_lines(
            data.to_string(),
            area,
            test_cell_size(),
            (0, 0, 0),
            false,
            false,
        )
    }

    #[test]
    fn marker_allows_markdown_indent_prefix() {
        let mut lines = history_lines("sixel-data", Rect::new(0, 0, 4, 2));
        for line in &mut lines {
            line.spans.insert(0, Span::raw("  "));
        }

        let (image, prefix_width) = sixel_history_image_with_prefix_width_from_line(&lines[0])
            .expect("image marker should parse");

        assert_eq!(image.data, "sixel-data");
        assert_eq!(image.columns, 4);
        assert_eq!(image.rows, 2);
        assert_eq!(prefix_width, 2);
        assert!(is_sixel_history_reserve_line(&lines[1]));
    }

    #[test]
    fn marker_wraps_sixel_data_to_restore_cursor_position() {
        let lines = history_lines("\x1bPqdata\x1b\\", Rect::new(0, 0, 4, 1));

        let image = sixel_history_image_from_line(&lines[0]).expect("image marker should parse");

        assert_eq!(
            image.data,
            "\x1b7\x1b[?8452h\x1b[4X\x1b[1B\x1b[1A\x1bPqdata\x1b\\\x1b[?8452l\x1b8"
        );
    }

    #[test]
    fn marker_clears_reserved_area_before_sixel_data() {
        let lines = history_lines("\x1bPqdata\x1b\\", Rect::new(0, 0, 4, 3));

        let image = sixel_history_image_from_line(&lines[0]).expect("image marker should parse");

        assert!(
            image
                .data
                .contains("\x1b[4X\x1b[1B\x1b[4X\x1b[1B\x1b[4X\x1b[1B\x1b[3A\x1bP")
        );
    }

    #[test]
    fn clear_area_sequence_preserves_cursor_position() {
        let sequence =
            sixel_clear_area_sequence(Rect::new(0, 0, 4, 2), test_cell_size(), (0, 0, 0), false);

        assert!(sequence.starts_with("\x1b7\x1b[?8452h"));
        assert!(sequence.contains("\x1bP9;0;0q\"1;1;4;2"));
        assert!(sequence.ends_with("\x1b[?8452l\x1b8"));
    }
}
