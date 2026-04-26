use std::fmt;
use std::io;
use std::io::Write;
use std::sync::LazyLock;
use std::sync::Mutex;

use crate::render::line_utils::line_to_static;
use crate::sixel_history::SixelCellSize;
use crate::sixel_history::is_sixel_history_reserve_line;
use crate::sixel_history::sixel_clear_area_sequence;
use crate::sixel_history::sixel_debug_log;
use crate::sixel_history::sixel_history_image_with_prefix_width_from_line;
use crate::wrapping::RtOptions;
use crate::wrapping::adaptive_wrap_line;
use crate::wrapping::line_contains_url_like;
use crate::wrapping::line_has_mixed_url_and_non_url_tokens;
use crossterm::Command;
use crossterm::cursor::MoveDown;
use crossterm::cursor::MoveTo;
use crossterm::cursor::MoveToColumn;
use crossterm::cursor::MoveUp;
use crossterm::cursor::RestorePosition;
use crossterm::cursor::SavePosition;
use crossterm::queue;
use crossterm::style::Color as CColor;
use crossterm::style::Colors;
use crossterm::style::Print;
use crossterm::style::SetAttribute;
use crossterm::style::SetBackgroundColor;
use crossterm::style::SetColors;
use crossterm::style::SetForegroundColor;
use crossterm::terminal::Clear;
use crossterm::terminal::ClearType;
use icy_sixel::BackgroundMode;
use icy_sixel::EncodeOptions;
use icy_sixel::SixelImage;
use ratatui::layout::Size;
use ratatui::prelude::Backend;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

const IMAGE_LOADING_PLACEHOLDER: &str = "[加载图片中..]";

enum PreparedHistoryLine {
    Text(Line<'static>),
    Sixel {
        cell_size: SixelCellSize,
        clear_background: (u8, u8, u8),
        columns: u16,
        data: String,
        is_tmux: bool,
        redraw_on_scroll: bool,
        rows: u16,
        prefix_width: u16,
    },
}

#[derive(Clone)]
struct DeferredSixelDraw {
    cell_size: SixelCellSize,
    clear_background: (u8, u8, u8),
    columns: u16,
    data: String,
    is_tmux: bool,
    rows: u16,
    source_row_offset: u16,
    visible_rows: u16,
    x: u16,
    y: u16,
}

#[derive(Clone, Copy)]
struct SixelClearContext {
    cell_size: SixelCellSize,
    clear_background: (u8, u8, u8),
    is_tmux: bool,
}

#[derive(Clone, Copy)]
struct SixelHistoryImageWrite<'a> {
    cell_size: SixelCellSize,
    clear_background: (u8, u8, u8),
    columns: u16,
    data: &'a str,
    is_tmux: bool,
    prefix_width: u16,
    rows: u16,
}

#[derive(Default)]
struct VisibleSixelState {
    screen_size: Option<Size>,
    clear_context: Option<SixelClearContext>,
    draws: Vec<DeferredSixelDraw>,
}

static VISIBLE_SIXEL_STATE: LazyLock<Mutex<VisibleSixelState>> =
    LazyLock::new(|| Mutex::new(VisibleSixelState::default()));

/// Selects the terminal escape strategy for inserting history lines above the viewport.
///
/// Standard terminals support `DECSTBM` scroll regions and Reverse Index (`ESC M`),
/// which let us slide existing content down without redrawing it. Zellij silently
/// drops or mishandles those sequences, so `Zellij` mode falls back to emitting
/// newlines at the bottom of the screen and writing lines at absolute positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertHistoryMode {
    Standard,
    Zellij,
}

impl InsertHistoryMode {
    pub fn new(is_zellij: bool) -> Self {
        if is_zellij {
            Self::Zellij
        } else {
            Self::Standard
        }
    }
}

/// Insert `lines` above the viewport using the terminal's backend writer
/// (avoids direct stdout references).
pub fn insert_history_lines<B>(
    terminal: &mut crate::custom_terminal::Terminal<B>,
    lines: Vec<Line>,
) -> io::Result<()>
where
    B: Backend + Write,
{
    insert_history_lines_with_mode(terminal, lines, InsertHistoryMode::Standard)
}

/// Insert `lines` above the viewport, using the escape strategy selected by `mode`.
///
/// In `Standard` mode this manipulates DECSTBM scroll regions to slide existing
/// scrollback down and writes new lines into the freed space. In `Zellij` mode it
/// emits newlines at the screen bottom to create space (since Zellij ignores scroll
/// region escapes) and writes lines at computed absolute positions. Both modes
/// update `terminal.viewport_area` so subsequent draw passes know where the
/// viewport moved to.
pub fn insert_history_lines_with_mode<B>(
    terminal: &mut crate::custom_terminal::Terminal<B>,
    lines: Vec<Line>,
    mode: InsertHistoryMode,
) -> io::Result<()>
where
    B: Backend + Write,
{
    let screen_size = terminal.backend().size().unwrap_or(Size::new(0, 0));

    let mut area = terminal.viewport_area;
    let mut should_update_area = false;
    let last_cursor_pos = terminal.last_known_cursor_pos;
    let writer = terminal.backend_mut();

    // Pre-wrap lines for terminal scrollback. Three paths:
    //
    // - URL-only-ish lines are kept intact (no hard newlines inserted) so that
    //   terminal emulators can match them as clickable links. The
    //   terminal will character-wrap these lines at the viewport
    //   boundary.
    // - Mixed lines (URL + non-URL prose) are adaptively wrapped so
    //   non-URL text still wraps naturally while URL tokens remain
    //   unsplit.
    // - Non-URL lines also flow through adaptive wrapping; behavior is
    //   equivalent to standard wrapping when no URL is present.
    let wrap_width = area.width.max(1) as usize;
    let (wrapped, wrapped_rows) = prepare_history_lines(&lines, wrap_width);
    let wrapped_lines = u16::try_from(wrapped_rows).unwrap_or(u16::MAX);
    let sixel_count = wrapped
        .iter()
        .filter(|line| matches!(line, PreparedHistoryLine::Sixel { .. }))
        .count();
    let redraw_sixel_count = wrapped
        .iter()
        .filter(|line| {
            matches!(
                line,
                PreparedHistoryLine::Sixel {
                    redraw_on_scroll: true,
                    ..
                }
            )
        })
        .count();
    if sixel_count > 0 {
        sixel_debug_log(format_args!(
            "insert-history mode={mode:?} screen={}x{} viewport={}x{}@{} rows={} sixels={} redraw_sixels={}",
            screen_size.width,
            screen_size.height,
            area.width,
            area.height,
            area.y,
            wrapped_lines,
            sixel_count,
            redraw_sixel_count
        ));
    }

    if matches!(mode, InsertHistoryMode::Zellij) {
        let space_below = screen_size.height.saturating_sub(area.bottom());
        let shift_down = wrapped_lines.min(space_below);
        let scroll_up_amount = wrapped_lines.saturating_sub(shift_down);

        if scroll_up_amount > 0 {
            // Scroll the entire screen up by emitting \n at the bottom
            queue!(writer, MoveTo(0, screen_size.height.saturating_sub(1)))?;
            for _ in 0..scroll_up_amount {
                queue!(writer, Print("\n"))?;
            }
        }

        if shift_down > 0 {
            area.y += shift_down;
            should_update_area = true;
        }

        let cursor_top = area.top().saturating_sub(scroll_up_amount + shift_down);
        queue!(writer, MoveTo(0, cursor_top))?;

        for (i, line) in wrapped.iter().enumerate() {
            if i > 0 {
                queue!(writer, Print("\r\n"))?;
            }
            write_prepared_history_line(writer, line, wrap_width, SixelInsertStrategy::DrawInline)?;
        }
    } else {
        let has_redraw_sixel = redraw_sixel_count > 0;
        let has_visible_sixel_state = visible_sixel_state_has_draws();
        let previous_visible_sixel_draws = if has_visible_sixel_state {
            VISIBLE_SIXEL_STATE
                .lock()
                .ok()
                .filter(|state| state.screen_size == Some(screen_size))
                .map(|state| state.draws.clone())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let clear_context = if has_redraw_sixel {
            first_sixel_clear_context(&wrapped).or_else(current_sixel_clear_context)
        } else if has_visible_sixel_state {
            current_sixel_clear_context()
        } else {
            None
        };
        if has_visible_sixel_state {
            clear_sixel_draw_regions(
                writer,
                screen_size,
                &previous_visible_sixel_draws,
                clear_context,
            )?;
        }

        let cursor_top = if area.bottom() < screen_size.height {
            let scroll_amount = wrapped_lines.min(screen_size.height - area.bottom());

            let top_1based = area.top() + 1;
            queue!(writer, SetScrollRegion(top_1based..screen_size.height))?;
            queue!(writer, MoveTo(0, area.top()))?;
            for _ in 0..scroll_amount {
                queue!(writer, Print("\x1bM"))?;
            }
            queue!(writer, ResetScrollRegion)?;

            let cursor_top = area.top().saturating_sub(1);
            area.y += scroll_amount;
            should_update_area = true;
            cursor_top
        } else {
            area.top().saturating_sub(1)
        };

        // Limit the scroll region to the lines from the top of the screen to the
        // top of the viewport. With this in place, when we add lines inside this
        // area, only the lines in this area will be scrolled. We place the cursor
        // at the end of the scroll region, and add lines starting there.
        //
        // ┌─Screen───────────────────────┐
        // │┌╌Scroll region╌╌╌╌╌╌╌╌╌╌╌╌╌╌┐│
        // │┆                            ┆│
        // │┆                            ┆│
        // │┆                            ┆│
        // │█╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌┘│
        // │╭─Viewport───────────────────╮│
        // ││                            ││
        // │╰────────────────────────────╯│
        // └──────────────────────────────┘
        queue!(writer, SetScrollRegion(1..area.top()))?;

        // NB: we are using MoveTo instead of set_cursor_position here to avoid messing with the
        // terminal's last_known_cursor_position, which hopefully will still be accurate after we
        // fetch/restore the cursor position. insert_history_lines should be cursor-position-neutral :)
        queue!(writer, MoveTo(0, cursor_top))?;

        let start_row = cursor_top.saturating_add(1);
        let history_scroll_rows = history_scroll_rows(start_row, wrapped_rows, area.top());
        let deferred_sixel_draws = if has_redraw_sixel {
            collect_deferred_sixel_draws(&wrapped, area.top(), start_row, wrapped_rows, wrap_width)
        } else {
            Vec::new()
        };
        let shifted_visible_sixel_draws = if has_visible_sixel_state {
            shift_visible_sixel_draws(screen_size, area.top(), history_scroll_rows)
        } else {
            Vec::new()
        };
        let strategy = if has_redraw_sixel || (sixel_count == 0 && has_visible_sixel_state) {
            SixelInsertStrategy::ReserveOnly
        } else {
            SixelInsertStrategy::DrawInline
        };
        queue!(writer, MoveTo(0, cursor_top))?;
        for line in &wrapped {
            queue!(writer, Print("\r\n"))?;
            write_prepared_history_line(writer, line, wrap_width, strategy)?;
        }

        queue!(writer, ResetScrollRegion)?;
        if has_redraw_sixel || (sixel_count == 0 && has_visible_sixel_state) {
            let mut visible_draws = shifted_visible_sixel_draws;
            visible_draws.extend(deferred_sixel_draws);
            draw_deferred_sixel_images(writer, &visible_draws)?;
            replace_visible_sixel_draws(screen_size, clear_context, visible_draws);
        } else if sixel_count > 0 && has_visible_sixel_state {
            replace_visible_sixel_draws(screen_size, None, Vec::new());
        }
    }

    // Restore the cursor position to where it was before we started.
    queue!(writer, MoveTo(last_cursor_pos.x, last_cursor_pos.y))?;

    let _ = writer;
    if should_update_area {
        terminal.set_viewport_area(area);
    }
    if wrapped_lines > 0 {
        terminal.note_history_rows_inserted(wrapped_lines);
    }

    Ok(())
}

fn prepare_history_lines(
    lines: &[Line<'_>],
    wrap_width: usize,
) -> (Vec<PreparedHistoryLine>, usize) {
    let mut wrapped = Vec::new();
    let mut wrapped_rows = 0usize;
    let mut source = lines.iter().peekable();

    while let Some(line) = source.next() {
        if let Some((image, prefix_width)) = sixel_history_image_with_prefix_width_from_line(line) {
            let rows = image.rows.max(1);
            wrapped_rows += usize::from(rows);
            wrapped.push(PreparedHistoryLine::Sixel {
                cell_size: image.cell_size,
                clear_background: image.clear_background,
                columns: image.columns.max(1),
                data: image.data,
                is_tmux: image.is_tmux,
                redraw_on_scroll: image.redraw_on_scroll,
                rows,
                prefix_width,
            });
            while source
                .peek()
                .is_some_and(|line| is_sixel_history_reserve_line(line))
            {
                source.next();
            }
            continue;
        }

        if is_sixel_history_reserve_line(line) {
            continue;
        }

        let line_wrapped =
            if line_contains_url_like(line) && !line_has_mixed_url_and_non_url_tokens(line) {
                vec![line.clone()]
            } else {
                adaptive_wrap_line(line, RtOptions::new(wrap_width))
            };
        for wrapped_line in line_wrapped {
            wrapped_rows += wrapped_line.width().max(1).div_ceil(wrap_width);
            wrapped.push(PreparedHistoryLine::Text(line_to_static(&wrapped_line)));
        }
    }

    (wrapped, wrapped_rows)
}

fn write_prepared_history_line<W: Write>(
    writer: &mut W,
    line: &PreparedHistoryLine,
    wrap_width: usize,
    strategy: SixelInsertStrategy,
) -> io::Result<()> {
    match line {
        PreparedHistoryLine::Text(line) => write_history_line(writer, line, wrap_width),
        PreparedHistoryLine::Sixel {
            cell_size,
            clear_background,
            columns,
            data,
            is_tmux,
            redraw_on_scroll: _,
            rows,
            prefix_width,
        } => match strategy {
            SixelInsertStrategy::DrawInline => write_sixel_history_image(
                writer,
                SixelHistoryImageWrite {
                    cell_size: *cell_size,
                    clear_background: *clear_background,
                    columns: *columns,
                    data,
                    is_tmux: *is_tmux,
                    prefix_width: *prefix_width,
                    rows: *rows,
                },
                wrap_width,
            ),
            SixelInsertStrategy::ReserveOnly => {
                write_sixel_history_placeholder(writer, *rows, *prefix_width, wrap_width)
            }
        },
    }
}

#[derive(Clone, Copy)]
enum SixelInsertStrategy {
    DrawInline,
    ReserveOnly,
}

fn collect_deferred_sixel_draws(
    lines: &[PreparedHistoryLine],
    region_height: u16,
    start_row: u16,
    total_rows: usize,
    wrap_width: usize,
) -> Vec<DeferredSixelDraw> {
    let scroll_rows = history_scroll_rows(start_row, total_rows, region_height);
    let mut draws = Vec::new();
    let mut row = 0usize;
    let mut image_index = 0usize;

    for line in lines {
        let height = line.height(wrap_width);
        if let PreparedHistoryLine::Sixel {
            cell_size,
            clear_background,
            columns,
            data,
            is_tmux,
            redraw_on_scroll,
            rows,
            prefix_width,
            ..
        } = line
        {
            if !*redraw_on_scroll {
                row += height;
                continue;
            }
            image_index += 1;
            let final_y = isize::try_from(usize::from(start_row) + row).unwrap_or(isize::MAX)
                - isize::try_from(scroll_rows).unwrap_or(isize::MAX);
            let final_bottom =
                final_y.saturating_add(isize::try_from(usize::from(*rows)).unwrap_or(isize::MAX));
            let region_height = isize::try_from(usize::from(region_height)).unwrap_or(isize::MAX);
            let visible_top = final_y.max(0);
            let visible_bottom = final_bottom.min(region_height);
            sixel_debug_log(format_args!(
                "insert-sixel-layout index={image_index} rows={rows} final_y={final_y} final_bottom={final_bottom} visible_top={visible_top} visible_bottom={visible_bottom}"
            ));
            if visible_top < visible_bottom {
                let source_row_offset =
                    u16::try_from(visible_top.saturating_sub(final_y)).unwrap_or(u16::MAX);
                let visible_rows =
                    u16::try_from(visible_bottom.saturating_sub(visible_top)).unwrap_or(0);
                draws.push(DeferredSixelDraw {
                    cell_size: *cell_size,
                    clear_background: *clear_background,
                    columns: *columns,
                    data: data.clone(),
                    is_tmux: *is_tmux,
                    rows: *rows,
                    source_row_offset,
                    visible_rows,
                    x: *prefix_width,
                    y: u16::try_from(visible_top).unwrap_or(0),
                });
            }
        }
        row += height;
    }

    sixel_debug_log(format_args!(
        "insert-sixel-defer start={} region_height={} total_rows={} scroll={} visible={}",
        start_row,
        region_height,
        total_rows,
        scroll_rows,
        draws.len()
    ));
    draws
}

fn history_scroll_rows(start_row: u16, total_rows: usize, region_height: u16) -> usize {
    (usize::from(start_row) + total_rows).saturating_sub(usize::from(region_height))
}

fn first_sixel_clear_context(lines: &[PreparedHistoryLine]) -> Option<SixelClearContext> {
    lines.iter().find_map(|line| match line {
        PreparedHistoryLine::Sixel {
            cell_size,
            clear_background,
            is_tmux,
            redraw_on_scroll: true,
            ..
        } => Some(SixelClearContext {
            cell_size: *cell_size,
            clear_background: *clear_background,
            is_tmux: *is_tmux,
        }),
        PreparedHistoryLine::Sixel { .. } => None,
        PreparedHistoryLine::Text(_) => None,
    })
}

fn current_sixel_clear_context() -> Option<SixelClearContext> {
    VISIBLE_SIXEL_STATE
        .lock()
        .ok()
        .and_then(|state| state.clear_context)
}

fn visible_sixel_state_has_draws() -> bool {
    VISIBLE_SIXEL_STATE
        .lock()
        .ok()
        .is_some_and(|state| !state.draws.is_empty())
}

fn shift_visible_sixel_draws(
    screen_size: Size,
    region_height: u16,
    scroll_rows: usize,
) -> Vec<DeferredSixelDraw> {
    let Some(state) = VISIBLE_SIXEL_STATE.lock().ok() else {
        return Vec::new();
    };
    if state.screen_size != Some(screen_size) {
        return Vec::new();
    }

    let scroll_rows = isize::try_from(scroll_rows).unwrap_or(isize::MAX);
    let region_height = isize::try_from(usize::from(region_height)).unwrap_or(isize::MAX);
    let mut shifted = Vec::new();
    for draw in &state.draws {
        let y = isize::try_from(usize::from(draw.y)).unwrap_or(isize::MAX) - scroll_rows;
        let bottom =
            y.saturating_add(isize::try_from(usize::from(draw.visible_rows)).unwrap_or(isize::MAX));
        if bottom <= 0 || y >= region_height {
            continue;
        }

        let mut draw = draw.clone();
        let visible_top = y.max(0);
        let visible_bottom = bottom.min(region_height);
        let clipped_top = visible_top.saturating_sub(y);
        draw.source_row_offset = draw
            .source_row_offset
            .saturating_add(u16::try_from(clipped_top).unwrap_or(u16::MAX));
        draw.visible_rows = u16::try_from(visible_bottom.saturating_sub(visible_top)).unwrap_or(0);
        if draw.visible_rows == 0 {
            continue;
        }
        draw.y = if y < 0 {
            0
        } else {
            u16::try_from(y).unwrap_or(0)
        };
        shifted.push(draw);
    }

    sixel_debug_log(format_args!(
        "insert-sixel-visible-shift scroll={} kept={}",
        scroll_rows,
        shifted.len()
    ));
    shifted
}

fn replace_visible_sixel_draws(
    screen_size: Size,
    clear_context: Option<SixelClearContext>,
    draws: Vec<DeferredSixelDraw>,
) {
    let Ok(mut state) = VISIBLE_SIXEL_STATE.lock() else {
        return;
    };
    if draws.is_empty() {
        state.screen_size = None;
        state.clear_context = None;
        state.draws.clear();
        return;
    }
    state.screen_size = Some(screen_size);
    state.clear_context = clear_context.or(state.clear_context);
    state.draws = draws;
}

pub(crate) fn clear_visible_sixel_graphics<W: Write>(
    writer: &mut W,
    screen_size: Size,
) -> io::Result<()> {
    let (draws, clear_context) = VISIBLE_SIXEL_STATE
        .lock()
        .ok()
        .map(|state| (state.draws.clone(), state.clear_context))
        .unwrap_or_default();
    sixel_debug_log(format_args!(
        "clear-visible-sixel screen={}x{} draws={} has_context={}",
        screen_size.width,
        screen_size.height,
        draws.len(),
        clear_context.is_some()
    ));
    if !draws.is_empty() {
        clear_sixel_draw_regions(writer, screen_size, &draws, clear_context)?;
    }
    replace_visible_sixel_draws(screen_size, None, Vec::new());
    Ok(())
}

fn clear_sixel_draw_regions<W: Write>(
    writer: &mut W,
    screen_size: Size,
    draws: &[DeferredSixelDraw],
    clear_context: Option<SixelClearContext>,
) -> io::Result<()> {
    let Some(clear_context) = clear_context else {
        sixel_debug_log(format_args!(
            "insert-sixel-clear-draws skipped no-context draws={}",
            draws.len()
        ));
        return Ok(());
    };

    for draw in draws {
        if draw.y >= screen_size.height {
            continue;
        }
        let columns = screen_size
            .width
            .saturating_sub(draw.x)
            .min(draw.columns)
            .max(1);
        let rows = screen_size
            .height
            .saturating_sub(draw.y)
            .min(draw.visible_rows)
            .max(1);
        let clear_area = ratatui::layout::Rect::new(0, 0, columns, rows);
        let clear_sequence = sixel_clear_area_sequence(
            clear_area,
            clear_context.cell_size,
            clear_context.clear_background,
            clear_context.is_tmux,
        );
        sixel_debug_log(format_args!(
            "insert-sixel-clear-draw x={} y={} area={}x{} clear_bytes={}",
            draw.x,
            draw.y,
            columns,
            rows,
            clear_sequence.len()
        ));
        queue!(writer, MoveTo(draw.x, draw.y), Print(clear_sequence))?;
    }

    Ok(())
}

fn draw_deferred_sixel_images<W: Write>(
    writer: &mut W,
    draws: &[DeferredSixelDraw],
) -> io::Result<()> {
    if draws.is_empty() {
        sixel_debug_log(format_args!("insert-sixel-defer-draw skipped visible=0"));
        return Ok(());
    }

    for draw in draws {
        let clear_area = ratatui::layout::Rect::new(0, 0, draw.columns.max(1), draw.visible_rows);
        let clear_sequence = sixel_clear_area_sequence(
            clear_area,
            draw.cell_size,
            draw.clear_background,
            draw.is_tmux,
        );
        queue!(writer, MoveTo(draw.x, draw.y), Print(clear_sequence))?;
        queue!(writer, MoveTo(draw.x, draw.y))?;
        write_image_loading_placeholder(writer, draw.columns)?;

        let is_partial = draw.source_row_offset != 0 || draw.visible_rows < draw.rows;
        let data = match visible_sixel_data(draw) {
            Some(data) => data,
            None => {
                sixel_debug_log(format_args!(
                    "insert-sixel-crop-fallback x={} y={} rows={} source_offset={} visible_rows={}",
                    draw.x, draw.y, draw.rows, draw.source_row_offset, draw.visible_rows
                ));
                draw.data.clone()
            }
        };
        sixel_debug_log(format_args!(
            "insert-sixel-defer-draw x={} y={} cols={} rows={} source_offset={} visible_rows={} partial={} data_bytes={}",
            draw.x,
            draw.y,
            draw.columns,
            draw.rows,
            draw.source_row_offset,
            draw.visible_rows,
            is_partial,
            data.len()
        ));
        queue!(writer, MoveTo(draw.x, draw.y), Print(data))?;
    }

    Ok(())
}

fn visible_sixel_data(draw: &DeferredSixelDraw) -> Option<String> {
    if draw.source_row_offset == 0 && draw.visible_rows >= draw.rows {
        return Some(draw.data.clone());
    }

    let sixel = extract_sixel_sequence(&draw.data)?;
    let decoded = SixelImage::decode(sixel.as_bytes()).ok()?;
    let cell_height = usize::from(draw.cell_size.height.max(1));
    let crop_top = usize::from(draw.source_row_offset).checked_mul(cell_height)?;
    let crop_height = usize::from(draw.visible_rows).checked_mul(cell_height)?;
    if crop_top >= decoded.height || crop_height == 0 {
        return None;
    }
    let crop_bottom = crop_top.saturating_add(crop_height).min(decoded.height);
    let crop_height = crop_bottom.saturating_sub(crop_top);
    if crop_height == 0 || decoded.width == 0 {
        return None;
    }

    let stride = decoded.width.checked_mul(4)?;
    let mut pixels = Vec::with_capacity(stride.checked_mul(crop_height)?);
    for row in crop_top..crop_bottom {
        let start = row.checked_mul(stride)?;
        let end = start.checked_add(stride)?;
        pixels.extend_from_slice(decoded.pixels.get(start..end)?);
    }

    let encode_options = EncodeOptions {
        diffusion: 0.0,
        ..Default::default()
    };
    let mut data = SixelImage::from_rgba(pixels, decoded.width, crop_height)
        .with_background_mode(BackgroundMode::Transparent)
        .encode_with(&encode_options)
        .ok()?;
    crate::sixel_history::insert_sixel_raster_attributes(&mut data, decoded.width, crop_height)?;
    if draw.is_tmux {
        crate::sixel_history::escape_sixel_for_tmux(&mut data)?;
    }
    sixel_debug_log(format_args!(
        "insert-sixel-crop x={} y={} width={} crop_top={} crop_height={} source_offset={} visible_rows={} bytes={}",
        draw.x,
        draw.y,
        decoded.width,
        crop_top,
        crop_height,
        draw.source_row_offset,
        draw.visible_rows,
        data.len()
    ));
    Some(format!("\x1b7\x1b[?8452h{data}\x1b[?8452l\x1b8"))
}

fn extract_sixel_sequence(data: &str) -> Option<&str> {
    let start = data.find("\x1bP")?;
    let after_start = &data[start..];
    let end = after_start.find("\x1b\\")? + "\x1b\\".len();
    Some(&after_start[..end])
}

impl PreparedHistoryLine {
    fn height(&self, wrap_width: usize) -> usize {
        match self {
            PreparedHistoryLine::Text(line) => line.width().max(1).div_ceil(wrap_width),
            PreparedHistoryLine::Sixel { rows, .. } => usize::from((*rows).max(1)),
        }
    }
}

/// Render a single wrapped history line: clear continuation rows for wide lines,
/// set foreground/background colors, and write styled spans. Caller is responsible
/// for cursor positioning and any leading `\r\n`.
fn write_history_line<W: Write>(writer: &mut W, line: &Line, wrap_width: usize) -> io::Result<()> {
    let physical_rows = line.width().max(1).div_ceil(wrap_width) as u16;
    if physical_rows > 1 {
        queue!(writer, SavePosition)?;
        for _ in 1..physical_rows {
            queue!(writer, MoveDown(1), MoveToColumn(0))?;
            queue!(writer, Clear(ClearType::UntilNewLine))?;
        }
        queue!(writer, RestorePosition)?;
    }
    queue!(
        writer,
        SetColors(Colors::new(
            line.style
                .fg
                .map(std::convert::Into::into)
                .unwrap_or(CColor::Reset),
            line.style
                .bg
                .map(std::convert::Into::into)
                .unwrap_or(CColor::Reset)
        ))
    )?;
    queue!(writer, Clear(ClearType::UntilNewLine))?;
    // Merge line-level style into each span so that ANSI colors reflect
    // line styles (e.g., blockquotes with green fg).
    let merged_spans: Vec<Span> = line
        .spans
        .iter()
        .map(|s| Span {
            style: s.style.patch(line.style),
            content: s.content.clone(),
        })
        .collect();
    write_spans(writer, merged_spans.iter())
}

fn write_sixel_history_image<W: Write>(
    writer: &mut W,
    image: SixelHistoryImageWrite<'_>,
    wrap_width: usize,
) -> io::Result<()> {
    let SixelHistoryImageWrite {
        cell_size,
        clear_background,
        columns,
        data,
        is_tmux,
        prefix_width,
        rows,
    } = image;
    let prefix_width = prefix_width.min(u16::try_from(wrap_width).unwrap_or(u16::MAX));
    let remaining_width = u16::try_from(wrap_width)
        .unwrap_or(u16::MAX)
        .saturating_sub(prefix_width);
    let clear_columns = columns.min(remaining_width).max(1);
    let clear_rows = rows.max(1);
    let clear_sequence = sixel_clear_area_sequence(
        ratatui::layout::Rect::new(0, 0, clear_columns, clear_rows),
        cell_size,
        clear_background,
        is_tmux,
    );
    sixel_debug_log(format_args!(
        "insert-sixel cols={} rows={} prefix={} wrap={} clear={}x{} data_bytes={} clear_bytes={}",
        columns,
        rows,
        prefix_width,
        wrap_width,
        clear_columns,
        clear_rows,
        data.len(),
        clear_sequence.len()
    ));

    for _ in 1..rows {
        queue!(writer, Print("\r\n"))?;
    }
    if rows > 1 {
        queue!(writer, MoveUp(rows - 1))?;
    }
    queue!(
        writer,
        SetForegroundColor(CColor::Reset),
        SetBackgroundColor(CColor::Reset),
        SetAttribute(crossterm::style::Attribute::Reset),
        Clear(ClearType::UntilNewLine),
        MoveToColumn(prefix_width),
    )?;
    write_image_loading_placeholder(writer, clear_columns)?;
    queue!(
        writer,
        MoveToColumn(prefix_width),
        Print(clear_sequence),
        Print(data),
        SetForegroundColor(CColor::Reset),
        SetBackgroundColor(CColor::Reset),
        SetAttribute(crossterm::style::Attribute::Reset),
    )?;
    if rows > 1 {
        queue!(writer, MoveDown(rows - 1), MoveToColumn(0))?;
    }
    Ok(())
}

fn write_sixel_history_placeholder<W: Write>(
    writer: &mut W,
    rows: u16,
    prefix_width: u16,
    wrap_width: usize,
) -> io::Result<()> {
    let rows = rows.max(1);
    let prefix_width = prefix_width.min(u16::try_from(wrap_width).unwrap_or(u16::MAX));
    let remaining_width = u16::try_from(wrap_width)
        .unwrap_or(u16::MAX)
        .saturating_sub(prefix_width);
    for _ in 1..rows {
        queue!(writer, Print("\r\n"))?;
    }
    if rows > 1 {
        queue!(writer, MoveUp(rows - 1))?;
    }
    for row in 0..rows {
        queue!(writer, Clear(ClearType::UntilNewLine))?;
        if row == 0 {
            queue!(writer, MoveToColumn(prefix_width))?;
            write_image_loading_placeholder(writer, remaining_width)?;
            queue!(writer, MoveToColumn(0))?;
        }
        if row + 1 < rows {
            queue!(writer, MoveDown(1), MoveToColumn(0))?;
        }
    }
    Ok(())
}

fn write_image_loading_placeholder<W: Write>(
    writer: &mut W,
    available_columns: u16,
) -> io::Result<()> {
    if UnicodeWidthStr::width(IMAGE_LOADING_PLACEHOLDER) <= usize::from(available_columns) {
        queue!(writer, Print(IMAGE_LOADING_PLACEHOLDER))?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetScrollRegion(pub std::ops::Range<u16>);

impl Command for SetScrollRegion {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[{};{}r", self.0.start, self.0.end)
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        panic!("tried to execute SetScrollRegion command using WinAPI, use ANSI instead");
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        // TODO(nornagon): is this supported on Windows?
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResetScrollRegion;

impl Command for ResetScrollRegion {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[r")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        panic!("tried to execute ResetScrollRegion command using WinAPI, use ANSI instead");
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        // TODO(nornagon): is this supported on Windows?
        true
    }
}

struct ModifierDiff {
    pub from: Modifier,
    pub to: Modifier,
}

impl ModifierDiff {
    fn queue<W>(self, mut w: W) -> io::Result<()>
    where
        W: io::Write,
    {
        use crossterm::style::Attribute as CAttribute;
        let removed = self.from - self.to;
        if removed.contains(Modifier::REVERSED) {
            queue!(w, SetAttribute(CAttribute::NoReverse))?;
        }
        if removed.contains(Modifier::BOLD) {
            queue!(w, SetAttribute(CAttribute::NormalIntensity))?;
            if self.to.contains(Modifier::DIM) {
                queue!(w, SetAttribute(CAttribute::Dim))?;
            }
        }
        if removed.contains(Modifier::ITALIC) {
            queue!(w, SetAttribute(CAttribute::NoItalic))?;
        }
        if removed.contains(Modifier::UNDERLINED) {
            queue!(w, SetAttribute(CAttribute::NoUnderline))?;
        }
        if removed.contains(Modifier::DIM) {
            queue!(w, SetAttribute(CAttribute::NormalIntensity))?;
        }
        if removed.contains(Modifier::CROSSED_OUT) {
            queue!(w, SetAttribute(CAttribute::NotCrossedOut))?;
        }
        if removed.contains(Modifier::SLOW_BLINK) || removed.contains(Modifier::RAPID_BLINK) {
            queue!(w, SetAttribute(CAttribute::NoBlink))?;
        }

        let added = self.to - self.from;
        if added.contains(Modifier::REVERSED) {
            queue!(w, SetAttribute(CAttribute::Reverse))?;
        }
        if added.contains(Modifier::BOLD) {
            queue!(w, SetAttribute(CAttribute::Bold))?;
        }
        if added.contains(Modifier::ITALIC) {
            queue!(w, SetAttribute(CAttribute::Italic))?;
        }
        if added.contains(Modifier::UNDERLINED) {
            queue!(w, SetAttribute(CAttribute::Underlined))?;
        }
        if added.contains(Modifier::DIM) {
            queue!(w, SetAttribute(CAttribute::Dim))?;
        }
        if added.contains(Modifier::CROSSED_OUT) {
            queue!(w, SetAttribute(CAttribute::CrossedOut))?;
        }
        if added.contains(Modifier::SLOW_BLINK) {
            queue!(w, SetAttribute(CAttribute::SlowBlink))?;
        }
        if added.contains(Modifier::RAPID_BLINK) {
            queue!(w, SetAttribute(CAttribute::RapidBlink))?;
        }

        Ok(())
    }
}

fn write_spans<'a, I>(mut writer: &mut impl Write, content: I) -> io::Result<()>
where
    I: IntoIterator<Item = &'a Span<'a>>,
{
    let mut fg = Color::Reset;
    let mut bg = Color::Reset;
    let mut last_modifier = Modifier::empty();
    for span in content {
        let mut modifier = Modifier::empty();
        modifier.insert(span.style.add_modifier);
        modifier.remove(span.style.sub_modifier);
        if modifier != last_modifier {
            let diff = ModifierDiff {
                from: last_modifier,
                to: modifier,
            };
            diff.queue(&mut writer)?;
            last_modifier = modifier;
        }
        let next_fg = span.style.fg.unwrap_or(Color::Reset);
        let next_bg = span.style.bg.unwrap_or(Color::Reset);
        if next_fg != fg || next_bg != bg {
            queue!(
                writer,
                SetColors(Colors::new(next_fg.into(), next_bg.into()))
            )?;
            fg = next_fg;
            bg = next_bg;
        }

        queue!(writer, Print(span.content.clone()))?;
    }

    queue!(
        writer,
        SetForegroundColor(CColor::Reset),
        SetBackgroundColor(CColor::Reset),
        SetAttribute(crossterm::style::Attribute::Reset),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown_render::render_markdown_text;
    use crate::test_backend::VT100Backend;
    use ratatui::layout::Rect;
    use ratatui::layout::Size;
    use ratatui::style::Color;

    #[test]
    fn writes_bold_then_regular_spans() {
        use ratatui::style::Stylize;

        let spans = ["A".bold(), "B".into()];

        let mut actual: Vec<u8> = Vec::new();
        write_spans(&mut actual, spans.iter()).unwrap();

        let mut expected: Vec<u8> = Vec::new();
        queue!(
            expected,
            SetAttribute(crossterm::style::Attribute::Bold),
            Print("A"),
            SetAttribute(crossterm::style::Attribute::NormalIntensity),
            Print("B"),
            SetForegroundColor(CColor::Reset),
            SetBackgroundColor(CColor::Reset),
            SetAttribute(crossterm::style::Attribute::Reset),
        )
        .unwrap();

        assert_eq!(
            String::from_utf8(actual).unwrap(),
            String::from_utf8(expected).unwrap()
        );
    }

    #[test]
    fn vt100_blockquote_line_emits_green_fg() {
        // Set up a small off-screen terminal
        let width: u16 = 40;
        let height: u16 = 10;
        let backend = VT100Backend::new(width, height);
        let mut term = crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        // Place viewport on the last line so history inserts scroll upward
        let viewport = Rect::new(0, height - 1, width, 1);
        term.set_viewport_area(viewport);

        // Build a blockquote-like line: apply line-level green style and prefix "> "
        let mut line: Line<'static> = Line::from(vec!["> ".into(), "Hello world".into()]);
        line = line.style(Color::Green);
        insert_history_lines(&mut term, vec![line])
            .expect("Failed to insert history lines in test");

        let mut saw_colored = false;
        'outer: for row in 0..height {
            for col in 0..width {
                if let Some(cell) = term.backend().vt100().screen().cell(row, col)
                    && cell.has_contents()
                    && cell.fgcolor() != vt100::Color::Default
                {
                    saw_colored = true;
                    break 'outer;
                }
            }
        }
        assert!(
            saw_colored,
            "expected at least one colored cell in vt100 output"
        );
    }

    #[test]
    fn vt100_blockquote_wrap_preserves_color_on_all_wrapped_lines() {
        // Force wrapping by using a narrow viewport width and a long blockquote line.
        let width: u16 = 20;
        let height: u16 = 8;
        let backend = VT100Backend::new(width, height);
        let mut term = crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        // Viewport is the last line so history goes directly above it.
        let viewport = Rect::new(0, height - 1, width, 1);
        term.set_viewport_area(viewport);

        // Create a long blockquote with a distinct prefix and enough text to wrap.
        let mut line: Line<'static> = Line::from(vec![
            "> ".into(),
            "This is a long quoted line that should wrap".into(),
        ]);
        line = line.style(Color::Green);

        insert_history_lines(&mut term, vec![line])
            .expect("Failed to insert history lines in test");

        // Parse and inspect the final screen buffer.
        let screen = term.backend().vt100().screen();

        // Collect rows that are non-empty; these should correspond to our wrapped lines.
        let mut non_empty_rows: Vec<u16> = Vec::new();
        for row in 0..height {
            let mut any = false;
            for col in 0..width {
                if let Some(cell) = screen.cell(row, col)
                    && cell.has_contents()
                    && cell.contents() != "\0"
                    && cell.contents() != " "
                {
                    any = true;
                    break;
                }
            }
            if any {
                non_empty_rows.push(row);
            }
        }

        // Expect at least two rows due to wrapping.
        assert!(
            non_empty_rows.len() >= 2,
            "expected wrapped output to span >=2 rows, got {non_empty_rows:?}",
        );

        // For each non-empty row, ensure all non-space cells are using a non-default fg color.
        for row in non_empty_rows {
            for col in 0..width {
                if let Some(cell) = screen.cell(row, col) {
                    let contents = cell.contents();
                    if !contents.is_empty() && contents != " " {
                        assert!(
                            cell.fgcolor() != vt100::Color::Default,
                            "expected non-default fg on row {row} col {col}, got {:?}",
                            cell.fgcolor()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn vt100_colored_prefix_then_plain_text_resets_color() {
        let width: u16 = 40;
        let height: u16 = 6;
        let backend = VT100Backend::new(width, height);
        let mut term = crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        let viewport = Rect::new(0, height - 1, width, 1);
        term.set_viewport_area(viewport);

        // First span colored, rest plain.
        let line: Line<'static> = Line::from(vec![
            Span::styled("1. ", ratatui::style::Style::default().fg(Color::LightBlue)),
            Span::raw("Hello world"),
        ]);

        insert_history_lines(&mut term, vec![line])
            .expect("Failed to insert history lines in test");

        let screen = term.backend().vt100().screen();

        // Find the first non-empty row; verify first three cells are colored, following cells default.
        'rows: for row in 0..height {
            let mut has_text = false;
            for col in 0..width {
                if let Some(cell) = screen.cell(row, col)
                    && cell.has_contents()
                    && cell.contents() != " "
                {
                    has_text = true;
                    break;
                }
            }
            if !has_text {
                continue;
            }

            // Expect "1. Hello world" starting at col 0.
            for col in 0..3 {
                let cell = screen.cell(row, col).unwrap();
                assert!(
                    cell.fgcolor() != vt100::Color::Default,
                    "expected colored prefix at col {col}, got {:?}",
                    cell.fgcolor()
                );
            }
            for col in 3..(3 + "Hello world".len() as u16) {
                let cell = screen.cell(row, col).unwrap();
                assert_eq!(
                    cell.fgcolor(),
                    vt100::Color::Default,
                    "expected default color for plain text at col {col}, got {:?}",
                    cell.fgcolor()
                );
            }
            break 'rows;
        }
    }

    #[test]
    fn vt100_deep_nested_mixed_list_third_level_marker_is_colored() {
        // Markdown with five levels (ordered → unordered → ordered → unordered → unordered).
        let md = "1. First\n   - Second level\n     1. Third level (ordered)\n        - Fourth level (bullet)\n          - Fifth level to test indent consistency\n";
        let text = render_markdown_text(md);
        let lines: Vec<Line<'static>> = text.lines.clone();

        let width: u16 = 60;
        let height: u16 = 12;
        let backend = VT100Backend::new(width, height);
        let mut term = crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        let viewport = ratatui::layout::Rect::new(0, height - 1, width, 1);
        term.set_viewport_area(viewport);

        insert_history_lines(&mut term, lines).expect("Failed to insert history lines in test");

        let screen = term.backend().vt100().screen();

        // Reconstruct screen rows as strings to locate the 3rd level line.
        let rows: Vec<String> = screen.rows(0, width).collect();

        let needle = "1. Third level (ordered)";
        let row_idx = rows
            .iter()
            .position(|r| r.contains(needle))
            .unwrap_or_else(|| {
                panic!("expected to find row containing {needle:?}, have rows: {rows:?}")
            });
        let col_start = rows[row_idx].find(needle).unwrap() as u16; // column where '1' starts

        // Verify that the numeric marker ("1.") at the third level is colored
        // (non-default fg) and the content after the following space resets to default.
        for c in [col_start, col_start + 1] {
            let cell = screen.cell(row_idx as u16, c).unwrap();
            assert!(
                cell.fgcolor() != vt100::Color::Default,
                "expected colored 3rd-level marker at row {row_idx} col {c}, got {:?}",
                cell.fgcolor()
            );
        }
        let content_col = col_start + 3; // skip '1', '.', and the space
        if let Some(cell) = screen.cell(row_idx as u16, content_col) {
            assert_eq!(
                cell.fgcolor(),
                vt100::Color::Default,
                "expected default color for 3rd-level content at row {row_idx} col {content_col}, got {:?}",
                cell.fgcolor()
            );
        }
    }

    #[test]
    fn vt100_prefixed_url_keeps_prefix_and_url_on_same_row() {
        let width: u16 = 48;
        let height: u16 = 8;
        let backend = VT100Backend::new(width, height);
        let mut term = crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        let viewport = Rect::new(0, height - 1, width, 1);
        term.set_viewport_area(viewport);

        let url = "http://a-long-url.com/this/that/blablablab/new.aspx/many_people_like_how";
        let line: Line<'static> = Line::from(vec!["  │ ".into(), url.into()]);

        insert_history_lines(&mut term, vec![line]).expect("insert history");

        let rows: Vec<String> = term.backend().vt100().screen().rows(0, width).collect();

        assert!(
            rows.iter().any(|r| r.contains("│ http://a-long-url.com")),
            "expected prefix and URL on same row, rows: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.trim_end() == "│"),
            "unexpected orphan prefix row, rows: {rows:?}"
        );
    }

    #[test]
    fn vt100_prefixed_url_like_without_scheme_keeps_prefix_and_token_on_same_row() {
        let width: u16 = 48;
        let height: u16 = 8;
        let backend = VT100Backend::new(width, height);
        let mut term = crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        let viewport = Rect::new(0, height - 1, width, 1);
        term.set_viewport_area(viewport);

        let url_like =
            "example.test/api/v1/projects/alpha-team/releases/2026-02-17/builds/1234567890";
        let line: Line<'static> = Line::from(vec!["  │ ".into(), url_like.into()]);

        insert_history_lines(&mut term, vec![line]).expect("insert history");

        let rows: Vec<String> = term.backend().vt100().screen().rows(0, width).collect();

        assert!(
            rows.iter()
                .any(|r| r.contains("│ example.test/api/v1/projects")),
            "expected prefix and URL-like token on same row, rows: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.trim_end() == "│"),
            "unexpected orphan prefix row, rows: {rows:?}"
        );
    }

    #[test]
    fn vt100_prefixed_mixed_url_line_wraps_suffix_words_together() {
        let width: u16 = 24;
        let height: u16 = 10;
        let backend = VT100Backend::new(width, height);
        let mut term = crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        let viewport = Rect::new(0, height - 1, width, 1);
        term.set_viewport_area(viewport);

        let url = "https://example.test/path/abcdef12345";
        let line: Line<'static> = Line::from(vec![
            "  │ ".into(),
            "see ".into(),
            url.into(),
            " tail words".into(),
        ]);

        insert_history_lines(&mut term, vec![line]).expect("insert mixed history");

        let rows: Vec<String> = term.backend().vt100().screen().rows(0, width).collect();
        assert!(
            rows.iter().any(|r| r.contains("│ see")),
            "expected prefixed prose before URL, rows: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("tail words")),
            "expected suffix words to wrap as a phrase, rows: {rows:?}"
        );
    }

    #[test]
    fn vt100_unwrapped_url_like_clears_continuation_rows() {
        let width: u16 = 20;
        let height: u16 = 10;
        let backend = VT100Backend::new(width, height);
        let mut term = crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        let viewport = Rect::new(0, height - 1, width, 1);
        term.set_viewport_area(viewport);

        let filler_line: Line<'static> = Line::from(vec![
            "  │ ".into(),
            "XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX".into(),
        ]);
        insert_history_lines(&mut term, vec![filler_line]).expect("insert filler history");

        let url_like = "example.test/api/v1/short";
        let url_line: Line<'static> = Line::from(vec!["  │ ".into(), url_like.into()]);
        insert_history_lines(&mut term, vec![url_line]).expect("insert url-like history");

        let rows: Vec<String> = term.backend().vt100().screen().rows(0, width).collect();
        let first_row = rows
            .iter()
            .position(|row| row.contains("│ example.test/api"))
            .unwrap_or_else(|| panic!("expected url-like first row in screen rows: {rows:?}"));
        assert!(
            first_row + 1 < rows.len(),
            "expected a continuation row for wrapped URL-like line, rows: {rows:?}"
        );
        let continuation_row = rows[first_row + 1].trim_end();

        assert!(
            continuation_row.contains("/v1/short") || continuation_row.contains("short"),
            "expected continuation row to contain wrapped URL-like tail, got: {continuation_row:?}"
        );
        assert!(
            !continuation_row.contains('X'),
            "expected continuation row to be cleared before writing wrapped URL-like content, got: {continuation_row:?}"
        );
    }

    #[test]
    fn vt100_long_unwrapped_url_does_not_insert_extra_blank_gap_before_content() {
        let width: u16 = 56;
        let height: u16 = 24;
        let backend = VT100Backend::new(width, height);
        let mut term = crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        let viewport = Rect::new(0, height - 1, width, 1);
        term.set_viewport_area(viewport);

        let prompt = "Write a long URL as output for testing";
        insert_history_lines(&mut term, vec![Line::from(prompt)]).expect("insert prompt line");

        let long_url = format!(
            "https://example.test/api/v1/projects/alpha-team/releases/2026-02-17/builds/1234567890/{}",
            "very-long-segment-".repeat(16),
        );
        let url_line: Line<'static> = Line::from(vec!["• ".into(), long_url.into()]);
        insert_history_lines(&mut term, vec![url_line]).expect("insert long url line");

        let rows: Vec<String> = term.backend().vt100().screen().rows(0, width).collect();
        let prompt_row = rows
            .iter()
            .position(|row| row.contains("Write a long URL as output for testing"))
            .unwrap_or_else(|| panic!("expected prompt row in screen rows: {rows:?}"));
        let url_row = rows
            .iter()
            .position(|row| row.contains("• https://example.test/api"))
            .unwrap_or_else(|| panic!("expected URL first row in screen rows: {rows:?}"));

        assert!(
            url_row <= prompt_row + 2,
            "expected URL content to appear immediately after prompt (allowing at most one spacer row), got prompt_row={prompt_row}, url_row={url_row}, rows={rows:?}",
        );
    }

    #[test]
    fn sixel_history_insert_does_not_clear_image_rows_after_drawing() {
        let mut output = Vec::new();
        write_sixel_history_image(
            &mut output,
            SixelHistoryImageWrite {
                cell_size: SixelCellSize {
                    width: 1,
                    height: 1,
                },
                clear_background: (0, 0, 0),
                columns: 4,
                data: "\x1b7\x1bPqdata\x1b\\\x1b8",
                is_tmux: false,
                prefix_width: 2,
                rows: 3,
            },
            /*wrap_width*/ 20,
        )
        .expect("sixel history image should be written");

        let output = String::from_utf8(output).expect("output should be utf8");
        let image_end = output
            .rfind("\x1b8")
            .map(|index| index + "\x1b8".len())
            .expect("restore cursor should be present after sixel data");
        let after_image = &output[image_end..];

        assert!(
            !after_image.contains("\x1b[K"),
            "sixel rows must not be cleared after drawing, output={output:?}"
        );
        assert!(
            !after_image.contains("\r\n"),
            "sixel rows must not scroll after drawing, output={output:?}"
        );
    }

    #[test]
    fn sixel_history_insert_writes_loading_placeholder_before_sixel_data() {
        let mut output = Vec::new();
        write_sixel_history_image(
            &mut output,
            SixelHistoryImageWrite {
                cell_size: SixelCellSize {
                    width: 1,
                    height: 1,
                },
                clear_background: (0, 0, 0),
                columns: 20,
                data: "\x1b7\x1bPqdata\x1b\\\x1b8",
                is_tmux: false,
                prefix_width: 0,
                rows: 1,
            },
            /*wrap_width*/ 20,
        )
        .expect("sixel history image should be written");

        let output = String::from_utf8(output).expect("output should be utf8");
        let placeholder_index = output
            .find(IMAGE_LOADING_PLACEHOLDER)
            .expect("loading placeholder should be written");
        let sixel_index = output.find("\x1bP").expect("sixel data should be written");

        assert!(
            placeholder_index < sixel_index,
            "placeholder should be written before sixel data, output={output:?}"
        );
    }

    #[test]
    fn sixel_redraw_clear_is_limited_to_image_rect() {
        let mut output = Vec::new();
        let draw = DeferredSixelDraw {
            cell_size: SixelCellSize {
                width: 1,
                height: 1,
            },
            clear_background: (0, 0, 0),
            columns: 4,
            data: "\x1b7\x1bPqdata\x1b\\\x1b8".to_string(),
            is_tmux: false,
            rows: 3,
            source_row_offset: 0,
            visible_rows: 3,
            x: 5,
            y: 2,
        };

        clear_sixel_draw_regions(
            &mut output,
            Size::new(20, 10),
            &[draw],
            Some(SixelClearContext {
                cell_size: SixelCellSize {
                    width: 1,
                    height: 1,
                },
                clear_background: (0, 0, 0),
                is_tmux: false,
            }),
        )
        .expect("clear should write");

        let output = String::from_utf8(output).expect("output should be utf8");
        assert!(
            output.contains("\x1b[3;6H"),
            "clear should start at image x/y, output={output:?}"
        );
        assert!(
            !output.contains("\x1b[3;1H"),
            "clear must not start at column zero, output={output:?}"
        );
        assert!(
            output.contains("\"1;1;4;3"),
            "clear must only cover image columns/rows, output={output:?}"
        );
        assert!(
            !output.contains("\"1;1;20;3"),
            "clear must not cover the full terminal row, output={output:?}"
        );
    }

    #[test]
    fn deferred_sixel_draw_does_not_clear_full_row() {
        let mut output = Vec::new();
        let draw = DeferredSixelDraw {
            cell_size: SixelCellSize {
                width: 1,
                height: 1,
            },
            clear_background: (0, 0, 0),
            columns: 4,
            data: "\x1b7\x1bPqdata\x1b\\\x1b8".to_string(),
            is_tmux: false,
            rows: 3,
            source_row_offset: 0,
            visible_rows: 3,
            x: 5,
            y: 2,
        };

        draw_deferred_sixel_images(&mut output, &[draw]).expect("draw should write");

        let output = String::from_utf8(output).expect("output should be utf8");
        assert!(
            !output.contains("\x1b[K"),
            "deferred draw must not clear to end of row, output={output:?}"
        );
        assert!(
            output.contains("\x1b[3;6H"),
            "draw should target the image x/y, output={output:?}"
        );
        assert!(
            output.contains("\"1;1;4;3"),
            "draw clear pass must only cover image columns/rows, output={output:?}"
        );
    }

    #[test]
    fn vt100_zellij_mode_inserts_history_and_updates_viewport() {
        let width: u16 = 32;
        let height: u16 = 8;
        let backend = VT100Backend::new(width, height);
        let mut term = crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        let viewport = Rect::new(0, 4, width, 2);
        term.set_viewport_area(viewport);

        let line: Line<'static> = Line::from("zellij history");
        insert_history_lines_with_mode(&mut term, vec![line], InsertHistoryMode::Zellij)
            .expect("insert zellij history");

        let rows: Vec<String> = term.backend().vt100().screen().rows(0, width).collect();
        assert!(
            rows.iter().any(|row| row.contains("zellij history")),
            "expected zellij history row in screen output, rows: {rows:?}"
        );
        assert_eq!(term.viewport_area, Rect::new(0, 5, width, 2));
        assert_eq!(term.visible_history_rows(), 1);
    }
}
