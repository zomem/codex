use crate::sixel_history::SixelCellSize;
use crate::sixel_history::escape_sixel_for_tmux;
use crate::sixel_history::insert_sixel_raster_attributes;
use crate::sixel_history::sixel_debug_log;
use crate::sixel_history::sixel_to_history_lines;
use crate::terminal_palette::default_bg;
use base64::Engine;
use icy_sixel::BackgroundMode;
use icy_sixel::EncodeOptions;
use icy_sixel::SixelImage;
use image::DynamicImage;
use image::ImageReader;
use image::RgbaImage;
use image::imageops::FilterType;
use ratatui::layout::Rect;
use ratatui::text::Line;
use std::io::Cursor;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use url::Url;

const TERMINAL_FONT_SIZE: (u16, u16) = (20, 40);
const DEFAULT_PREVIEW_ROWS: u16 = 40;
const DEFAULT_PREVIEW_COLUMNS: u16 = 80;
const MIN_PREVIEW_ROWS: u16 = 8;
const MAX_PREVIEW_COLUMNS: u16 = 120;
const MAX_PREVIEW_ROWS: u16 = 40;
const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_IMAGE_PIXELS: u64 = 16 * 1024 * 1024;
const DEFAULT_CLEAR_BACKGROUND: (u8, u8, u8) = (8, 13, 20);
const IMAGE_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ImagePreviewCacheKey {
    pub(crate) columns: u16,
    pub(crate) rows: u16,
    pub(crate) cell_width: u16,
    pub(crate) cell_height: u16,
}

#[derive(Clone, Copy, Debug)]
struct TerminalMetrics {
    columns: u16,
    font_size: (u16, u16),
    rows: u16,
}

struct LoadedImage {
    bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ImagePreviewRenderOptions {
    pub(crate) clear_background: Option<(u8, u8, u8)>,
    pub(crate) min_scale: Option<f32>,
    pub(crate) min_visible_alpha: Option<u8>,
    pub(crate) redraw_on_scroll: bool,
    pub(crate) resize_filter: FilterType,
    pub(crate) max_columns: u16,
    pub(crate) max_rows: u16,
}

impl Default for ImagePreviewRenderOptions {
    fn default() -> Self {
        Self {
            clear_background: None,
            min_scale: None,
            min_visible_alpha: None,
            redraw_on_scroll: false,
            resize_filter: FilterType::Triangle,
            max_columns: MAX_PREVIEW_COLUMNS,
            max_rows: MAX_PREVIEW_ROWS,
        }
    }
}

pub(crate) fn local_image_preview_cache_key(max_width: Option<usize>) -> ImagePreviewCacheKey {
    let metrics = terminal_metrics();
    let area = preview_area(max_width, metrics, ImagePreviewRenderOptions::default());
    ImagePreviewCacheKey {
        columns: area.width,
        rows: area.height,
        cell_width: metrics.font_size.0,
        cell_height: metrics.font_size.1,
    }
}

pub(crate) fn render_local_image_preview_to_lines(
    path: &Path,
    cache_key: ImagePreviewCacheKey,
) -> Option<Vec<Line<'static>>> {
    let image = load_preview_image(path)?;
    render_sixel_image_preview_to_lines(
        image,
        Rect::new(0, 0, cache_key.columns, cache_key.rows),
        (cache_key.cell_width, cache_key.cell_height),
        ImagePreviewRenderOptions::default(),
    )
}

pub(crate) fn render_markdown_image_to_lines(
    destination: &str,
    cwd: Option<&Path>,
    max_width: Option<usize>,
) -> Option<Vec<Line<'static>>> {
    let loaded = load_image_bytes(destination, cwd)?;
    let image = decode_image_bytes(&loaded.bytes)?;
    render_dynamic_image_preview_to_lines_with_options(
        image,
        max_width,
        ImagePreviewRenderOptions::default(),
    )
}

fn render_dynamic_image_preview_to_lines_with_options(
    image: DynamicImage,
    max_width: Option<usize>,
    options: ImagePreviewRenderOptions,
) -> Option<Vec<Line<'static>>> {
    let metrics = terminal_metrics();
    let options = ImagePreviewRenderOptions {
        max_columns: options.max_columns.min(metrics.columns),
        ..options
    };
    render_sixel_image_preview_to_lines(
        image,
        preview_area(max_width, metrics, options),
        metrics.font_size,
        options,
    )
}

fn load_preview_image(path: &Path) -> Option<DynamicImage> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > MAX_IMAGE_BYTES {
        return None;
    }

    let image = ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    if u64::from(image.width()) * u64::from(image.height()) > MAX_IMAGE_PIXELS {
        return None;
    }

    Some(image)
}

fn decode_image_bytes(bytes: &[u8]) -> Option<DynamicImage> {
    let image = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    if u64::from(image.width()) * u64::from(image.height()) > MAX_IMAGE_PIXELS {
        return None;
    }
    Some(image)
}

fn render_sixel_image_preview_to_lines(
    image: DynamicImage,
    mut max_area: Rect,
    font_size: (u16, u16),
    options: ImagePreviewRenderOptions,
) -> Option<Vec<Line<'static>>> {
    if max_area.width == 0 || max_area.height == 0 {
        return Some(Vec::new());
    }

    if let Some(min_scale) = options
        .min_scale
        .filter(|scale| scale.is_finite() && *scale > 0.0)
    {
        let min_columns =
            scaled_cells_for_pixels(image.width(), font_size.0, min_scale).min(options.max_columns);
        let min_rows =
            scaled_cells_for_pixels(image.height(), font_size.1, min_scale).min(options.max_rows);
        max_area.width = max_area.width.max(min_columns);
        max_area.height = max_area.height.max(min_rows);
    }

    let (pixel_width, pixel_height, columns, rows) =
        fitted_image_size(&image, max_area, font_size)?;
    sixel_debug_log(format_args!(
        "preview-fit source={}x{} max_area={}x{} font={}x{} pixels={}x{} cells={}x{} options=min_scale:{:?},min_alpha:{:?},max:{}x{}",
        image.width(),
        image.height(),
        max_area.width,
        max_area.height,
        font_size.0,
        font_size.1,
        pixel_width,
        pixel_height,
        columns,
        rows,
        options.min_scale,
        options.min_visible_alpha,
        options.max_columns,
        options.max_rows
    ));
    let mut resized = image
        .resize_exact(pixel_width, pixel_height, options.resize_filter)
        .to_rgba8();
    if let Some(min_visible_alpha) = options.min_visible_alpha {
        keep_visible_alpha_for_sixel(&mut resized, min_visible_alpha);
    }
    let reserved_area = Rect::new(0, 0, columns, rows.max(1));
    let data = encode_sixel_image(resized)?;
    let clear_background = options
        .clear_background
        .or_else(default_bg)
        .unwrap_or(DEFAULT_CLEAR_BACKGROUND);
    sixel_debug_log(format_args!(
        "preview-encoded reserved={}x{} clear_bg=#{:02x}{:02x}{:02x} data_bytes={}",
        reserved_area.width,
        reserved_area.height,
        clear_background.0,
        clear_background.1,
        clear_background.2,
        data.len()
    ));
    Some(sixel_to_history_lines(
        data,
        reserved_area,
        SixelCellSize {
            width: font_size.0,
            height: font_size.1,
        },
        clear_background,
        is_tmux_session(),
        options.redraw_on_scroll,
    ))
}

fn scaled_cells_for_pixels(pixels: u32, cell_size: u16, scale: f32) -> u16 {
    let scaled_pixels = (pixels as f32 * scale).ceil().max(1.0) as u32;
    u16::try_from(scaled_pixels.div_ceil(u32::from(cell_size))).unwrap_or(u16::MAX)
}

fn keep_visible_alpha_for_sixel(image: &mut RgbaImage, min_visible_alpha: u8) {
    for pixel in image.pixels_mut() {
        let alpha = pixel.0[3];
        pixel.0[3] = if alpha >= min_visible_alpha { 255 } else { 0 };
    }
}

fn encode_sixel_image(image: RgbaImage) -> Option<String> {
    let width = usize::try_from(image.width()).ok()?;
    let height = usize::try_from(image.height()).ok()?;
    let mut data = SixelImage::from_rgba(image.into_raw(), width, height)
        .with_background_mode(BackgroundMode::Transparent)
        .encode_with(&EncodeOptions::default())
        .ok()?;
    insert_sixel_raster_attributes(&mut data, width, height)?;

    if is_tmux_session() {
        escape_sixel_for_tmux(&mut data)?;
    }
    Some(data)
}

fn fitted_image_size(
    image: &DynamicImage,
    max_area: Rect,
    font_size: (u16, u16),
) -> Option<(u32, u32, u16, u16)> {
    if image.width() == 0 || image.height() == 0 || max_area.width == 0 || max_area.height == 0 {
        return None;
    }

    let max_pixel_width = u32::from(max_area.width) * u32::from(font_size.0);
    let max_pixel_height = u32::from(max_area.height) * u32::from(font_size.1);
    let width_scale = max_pixel_width as f32 / image.width() as f32;
    let height_scale = max_pixel_height as f32 / image.height() as f32;
    let scale = width_scale.min(height_scale).max(f32::EPSILON);

    let pixel_width = (image.width() as f32 * scale).ceil().max(1.0) as u32;
    let pixel_height = (image.height() as f32 * scale).ceil().max(1.0) as u32;
    let columns = u16::try_from(pixel_width.div_ceil(u32::from(font_size.0))).ok()?;
    let rows = u16::try_from(pixel_height.div_ceil(u32::from(font_size.1))).ok()?;

    Some((
        pixel_width,
        pixel_height,
        columns.min(max_area.width).max(1),
        rows.min(max_area.height).max(1),
    ))
}

fn terminal_metrics() -> TerminalMetrics {
    #[cfg(not(test))]
    {
        if let Ok(size) = crossterm::terminal::window_size()
            && size.columns > 0
            && size.rows > 0
            && size.width > 0
            && size.height > 0
        {
            return TerminalMetrics {
                columns: size.columns,
                font_size: (
                    rounded_cell_size(size.width, size.columns),
                    rounded_cell_size(size.height, size.rows),
                ),
                rows: size.rows,
            };
        }
    }

    TerminalMetrics {
        columns: DEFAULT_PREVIEW_COLUMNS,
        font_size: TERMINAL_FONT_SIZE,
        rows: DEFAULT_PREVIEW_ROWS,
    }
}

#[cfg(not(test))]
fn rounded_cell_size(pixels: u16, cells: u16) -> u16 {
    let pixels = u32::from(pixels);
    let cells = u32::from(cells);
    u16::try_from((pixels + cells / 2) / cells)
        .unwrap_or(u16::MAX)
        .max(1)
}

fn is_tmux_session() -> bool {
    std::env::var("TERM").is_ok_and(|term| term.starts_with("tmux"))
        || std::env::var("TERM_PROGRAM").is_ok_and(|term_program| term_program == "tmux")
}

fn preview_area(
    max_width: Option<usize>,
    metrics: TerminalMetrics,
    options: ImagePreviewRenderOptions,
) -> Rect {
    let width = max_width
        .and_then(|value| u16::try_from(value).ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_PREVIEW_COLUMNS)
        .min(options.max_columns)
        .min(metrics.columns);
    let height = preview_rows(metrics.rows, options.max_rows);
    Rect::new(0, 0, width, height)
}

fn preview_rows(terminal_rows: u16, max_rows: u16) -> u16 {
    let rows = if terminal_rows == 0 {
        DEFAULT_PREVIEW_ROWS
    } else {
        terminal_rows.saturating_mul(2) / 3
    };
    rows.clamp(MIN_PREVIEW_ROWS, max_rows.max(MIN_PREVIEW_ROWS))
}

fn load_image_bytes(destination: &str, cwd: Option<&Path>) -> Option<LoadedImage> {
    if let Ok(url) = Url::parse(destination) {
        return match url.scheme() {
            "data" => load_data_image_bytes(destination),
            "http" | "https" => load_remote_image_bytes(&url),
            "file" => {
                let path = url.to_file_path().ok()?;
                load_local_image_bytes(&path)
            }
            "files" => {
                let path = files_url_to_path(&url)?;
                load_local_image_bytes(&path)
            }
            _ => None,
        };
    }

    let path = resolve_local_image_path(destination, cwd);
    load_local_image_bytes(&path)
}

fn load_data_image_bytes(destination: &str) -> Option<LoadedImage> {
    let data_url = destination.strip_prefix("data:")?;
    let (metadata, payload) = data_url.split_once(',')?;
    if !metadata.split(';').next().is_some_and(|mime_type| {
        mime_type.eq_ignore_ascii_case("image/png")
            || mime_type.eq_ignore_ascii_case("image/jpeg")
            || mime_type.eq_ignore_ascii_case("image/jpg")
            || mime_type.eq_ignore_ascii_case("image/gif")
            || mime_type.eq_ignore_ascii_case("image/webp")
    }) {
        return None;
    }
    if !metadata
        .split(';')
        .any(|part| part.eq_ignore_ascii_case("base64"))
    {
        return None;
    }
    let payload = urlencoding::decode(payload).ok()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload.as_bytes())
        .ok()?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return None;
    }
    Some(LoadedImage { bytes })
}

fn resolve_local_image_path(destination: &str, cwd: Option<&Path>) -> PathBuf {
    let path = destination
        .split(['#', '?'])
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or(destination);
    let path = Path::new(path);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    cwd.map(|cwd| cwd.join(path))
        .unwrap_or_else(|| path.to_path_buf())
}

fn files_url_to_path(url: &Url) -> Option<PathBuf> {
    let path = url.path();
    if path.is_empty() {
        return None;
    }
    Some(PathBuf::from(path))
}

fn load_local_image_bytes(path: &Path) -> Option<LoadedImage> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return None;
    }
    Some(LoadedImage { bytes })
}

fn load_remote_image_bytes(url: &Url) -> Option<LoadedImage> {
    let url = url.as_str().to_string();
    std::thread::spawn(move || {
        let client = reqwest::blocking::Client::builder()
            .timeout(IMAGE_FETCH_TIMEOUT)
            .build()
            .ok()?;
        let response = client.get(&url).send().ok()?.error_for_status().ok()?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_IMAGE_BYTES)
        {
            return None;
        }

        let mut bytes = Vec::new();
        {
            use std::io::Read as _;

            response
                .take(MAX_IMAGE_BYTES + 1)
                .read_to_end(&mut bytes)
                .ok()?;
        }
        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            return None;
        }

        Some(LoadedImage { bytes })
    })
    .join()
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sixel_history::sixel_history_image_from_line;
    use icy_sixel::SixelImage;
    use image::Rgba;
    use image::RgbaImage;

    #[test]
    fn image_preview_renders_sixel_lines() {
        let image = RgbaImage::from_fn(40, 40, |x, y| {
            if x < 20 && y < 20 {
                Rgba([255, 0, 0, 255])
            } else if x >= 20 && y < 20 {
                Rgba([0, 255, 0, 255])
            } else if x < 20 {
                Rgba([0, 0, 255, 255])
            } else {
                Rgba([255, 255, 0, 255])
            }
        });

        let lines = render_sixel_image_preview_to_lines(
            DynamicImage::ImageRgba8(image),
            Rect::new(0, 0, 4, 2),
            TERMINAL_FONT_SIZE,
            ImagePreviewRenderOptions::default(),
        )
        .expect("preview should render");

        assert_eq!(lines.len(), 2);
        let image = sixel_history_image_from_line(&lines[0]).expect("preview should be sixel");
        assert_eq!(image.rows, 2);
        assert!(image.data.starts_with("\x1b7\x1b[?8452h\x1b[4X\x1b[1B"));
        assert!(image.data.contains("\x1bP9;1;0q"));
        assert!(image.data.contains("\"1;1;80;80"));
    }

    #[test]
    fn sixel_preview_uses_fitted_height_for_wide_images() {
        let image = RgbaImage::from_pixel(800, 80, Rgba([0, 255, 0, 255]));

        let lines = render_sixel_image_preview_to_lines(
            DynamicImage::ImageRgba8(image),
            Rect::new(0, 0, 80, 40),
            TERMINAL_FONT_SIZE,
            ImagePreviewRenderOptions::default(),
        )
        .expect("preview should render");

        let image = sixel_history_image_from_line(&lines[0]).expect("preview should be sixel");
        assert_eq!(image.rows, 4);
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn sixel_preview_preserves_transparency() {
        let image = RgbaImage::from_fn(12, 12, |x, y| {
            if x == y {
                Rgba([255, 0, 0, 48])
            } else {
                Rgba([0, 0, 0, 0])
            }
        });

        let lines = render_sixel_image_preview_to_lines(
            DynamicImage::ImageRgba8(image),
            Rect::new(0, 0, 2, 1),
            TERMINAL_FONT_SIZE,
            ImagePreviewRenderOptions {
                min_visible_alpha: Some(8),
                ..ImagePreviewRenderOptions::default()
            },
        )
        .expect("preview should render");
        let image = sixel_history_image_from_line(&lines[0]).expect("preview should be sixel");
        let sixel = extract_sixel_sequence(&image.data);
        let decoded = SixelImage::decode(sixel.as_bytes()).expect("sixel should decode");

        assert_eq!(decoded.background_mode, BackgroundMode::Transparent);
        assert!(decoded.has_transparency());
    }

    #[test]
    fn sixel_preview_min_scale_expands_reserved_area() {
        let image = RgbaImage::from_pixel(160, 80, Rgba([0, 255, 0, 255]));

        let lines = render_sixel_image_preview_to_lines(
            DynamicImage::ImageRgba8(image),
            Rect::new(0, 0, 2, 1),
            TERMINAL_FONT_SIZE,
            ImagePreviewRenderOptions {
                min_scale: Some(1.0),
                max_columns: 20,
                max_rows: 20,
                ..ImagePreviewRenderOptions::default()
            },
        )
        .expect("preview should render");

        let image = sixel_history_image_from_line(&lines[0]).expect("preview should be sixel");
        assert_eq!((image.columns, image.rows), (8, 2));
    }

    #[test]
    fn fitted_preview_size_preserves_source_ratio_across_window_widths() {
        let image =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(1600, 900, Rgba([0, 255, 0, 255])));

        for max_area in [
            Rect::new(0, 0, 120, 40),
            Rect::new(0, 0, 80, 40),
            Rect::new(0, 0, 32, 40),
        ] {
            let (pixel_width, pixel_height, _, _) =
                fitted_image_size(&image, max_area, TERMINAL_FONT_SIZE)
                    .expect("preview should fit");
            assert_ratio_close(f64::from(pixel_width) / f64::from(pixel_height), 16.0 / 9.0);
        }
    }

    #[test]
    fn fitted_preview_size_preserves_source_ratio_for_tall_images() {
        let image =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(900, 1600, Rgba([0, 255, 0, 255])));

        for max_area in [
            Rect::new(0, 0, 120, 40),
            Rect::new(0, 0, 80, 24),
            Rect::new(0, 0, 32, 16),
        ] {
            let (pixel_width, pixel_height, _, _) =
                fitted_image_size(&image, max_area, TERMINAL_FONT_SIZE)
                    .expect("preview should fit");
            assert_ratio_close(f64::from(pixel_width) / f64::from(pixel_height), 9.0 / 16.0);
        }
    }

    #[test]
    fn fitted_preview_size_upscales_small_images_to_available_area() {
        let image =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(300, 500, Rgba([0, 255, 0, 255])));

        let (pixel_width, pixel_height, columns, rows) =
            fitted_image_size(&image, Rect::new(0, 0, 80, 40), TERMINAL_FONT_SIZE)
                .expect("preview should fit");

        assert_eq!((pixel_width, pixel_height), (960, 1600));
        assert_eq!((columns, rows), (48, 40));
        assert_ratio_close(
            f64::from(pixel_width) / f64::from(pixel_height),
            300.0 / 500.0,
        );
    }

    #[test]
    fn markdown_image_preview_renders_sixel_lines() {
        let image = RgbaImage::from_fn(20, 20, |x, y| {
            if x / 10 == y / 10 {
                Rgba([255, 255, 255, 255])
            } else {
                Rgba([0, 0, 0, 0])
            }
        });

        let lines = render_sixel_image_preview_to_lines(
            DynamicImage::ImageRgba8(image),
            Rect::new(0, 0, 2, 1),
            TERMINAL_FONT_SIZE,
            ImagePreviewRenderOptions::default(),
        )
        .expect("preview should render");

        assert_eq!(lines.len(), 1);
        let image = sixel_history_image_from_line(&lines[0]).expect("preview should be sixel");
        assert_eq!(image.columns, 2);
        assert_eq!(image.rows, 1);
    }

    fn assert_ratio_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 0.01,
            "expected ratio {expected}, got {actual}"
        );
    }

    fn extract_sixel_sequence(data: &str) -> &str {
        let start = data.find("\x1bP").expect("sixel data should start");
        let terminator = data[start..]
            .find("\x1b\\")
            .expect("sixel data should terminate");
        &data[start..start + terminator + 2]
    }
}
