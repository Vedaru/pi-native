//! Terminal image protocols (VED-307): kitty, iTerm2, and sixel.
//!
//! Ported from pi's `packages/tui` image path so an inline image renders the
//! same escape sequences pi produces. The module is transport-free: it detects
//! capabilities from the environment, measures an image from its encoded bytes,
//! lays it out in terminal cells, and returns the escape sequence (plus the
//! columns/rows it occupies) for the caller to place in the frame.
//!
//! pi ships **kitty** (`\x1b_G…`) and **iTerm2** (`\x1b]1337;File=…`) output;
//! sixel is included here because the VED-307 scope names it, as a fallback
//! encoder for terminals that accept `\x1bP…q` graphics.

use base64::Engine as _;

/// Default terminal cell size in pixels when the real size is unknown. Matches
/// pi's `getCellDimensions()` default.
pub const DEFAULT_CELL_WIDTH_PX: u32 = 9;
pub const DEFAULT_CELL_HEIGHT_PX: u32 = 18;

/// Kitty caps a single transmission at 4096 base64 bytes; longer data is
/// chunked with the `m` (more) control.
const KITTY_CHUNK: usize = 4096;
/// Largest image id the kitty protocol allows.
const KITTY_MAX_ID: u32 = 4_294_967_294;

/// Pixel size of one terminal cell, used to convert an image's pixel size into
/// the columns and rows it should occupy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellDimensions {
    pub width_px: u32,
    pub height_px: u32,
}

impl Default for CellDimensions {
    fn default() -> Self {
        Self {
            width_px: DEFAULT_CELL_WIDTH_PX,
            height_px: DEFAULT_CELL_HEIGHT_PX,
        }
    }
}

/// The image protocol a terminal supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProtocol {
    Kitty,
    Iterm2,
    Sixel,
}

/// Terminal capabilities relevant to image rendering, mirroring pi's
/// `detectCapabilitiesFromEnvironment`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub images: Option<ImageProtocol>,
    pub true_color: bool,
    pub hyperlinks: bool,
}

impl Capabilities {
    /// Detect from the process environment, as pi does. A multiplexer
    /// (`tmux`/`screen`) disables images because the escapes are not forwarded
    /// by default.
    pub fn detect() -> Self {
        Self::detect_from(|key| std::env::var(key).ok())
    }

    /// Detection with an injected environment lookup, so it is testable.
    pub fn detect_from(mut env: impl FnMut(&str) -> Option<String>) -> Self {
        let mut lower = |key: &str| env(key).map(|value| value.to_lowercase());
        let term_program = lower("TERM_PROGRAM").unwrap_or_default();
        let terminal_emulator = lower("TERMINAL_EMULATOR").unwrap_or_default();
        let term = lower("TERM").unwrap_or_default();
        let color_term = lower("COLORTERM").unwrap_or_default();
        let has_true_color_hint =
            color_term == "truecolor" || color_term == "24bit" || term.ends_with("-direct");
        let mut env_set = |key: &str| env(key).is_some_and(|value| !value.is_empty());

        if env_set("TMUX") || term.starts_with("tmux") {
            return Self {
                images: None,
                true_color: has_true_color_hint,
                hyperlinks: false,
            };
        }
        if term.starts_with("screen") {
            return Self {
                images: None,
                true_color: has_true_color_hint,
                hyperlinks: false,
            };
        }
        if env_set("KITTY_WINDOW_ID") || term_program == "kitty" {
            return Self {
                images: Some(ImageProtocol::Kitty),
                true_color: true,
                hyperlinks: true,
            };
        }
        if term_program == "ghostty" || term.contains("ghostty") || env_set("GHOSTTY_RESOURCES_DIR")
        {
            return Self {
                images: Some(ImageProtocol::Kitty),
                true_color: true,
                hyperlinks: true,
            };
        }
        if env_set("WEZTERM_PANE") || term_program == "wezterm" {
            return Self {
                images: Some(ImageProtocol::Kitty),
                true_color: true,
                hyperlinks: true,
            };
        }
        if term_program == "warpterminal"
            || env_set("WARP_SESSION_ID")
            || env_set("WARP_TERMINAL_SESSION_UUID")
        {
            return Self {
                images: Some(ImageProtocol::Kitty),
                true_color: true,
                hyperlinks: true,
            };
        }
        if env_set("ITERM_SESSION_ID") || term_program == "iterm.app" {
            return Self {
                images: Some(ImageProtocol::Iterm2),
                true_color: true,
                hyperlinks: true,
            };
        }
        if env_set("WT_SESSION") {
            return Self {
                images: None,
                true_color: true,
                hyperlinks: true,
            };
        }
        if term_program == "alacritty" || term_program == "vscode" || term_program == "zed" {
            return Self {
                images: None,
                true_color: true,
                hyperlinks: true,
            };
        }
        if terminal_emulator == "jetbrains-jediterm" {
            return Self {
                images: None,
                true_color: true,
                hyperlinks: false,
            };
        }
        Self {
            images: None,
            true_color: has_true_color_hint,
            hyperlinks: false,
        }
    }

    /// Whether the terminal accepts sixel graphics (`TERM` containing `sixel`).
    pub fn detect_sixel(&mut self, term: &str) {
        if self.images.is_none() && term.to_lowercase().contains("sixel") {
            self.images = Some(ImageProtocol::Sixel);
        }
    }
}

/// Pixel dimensions of an image, read from its encoded bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageDimensions {
    pub width_px: u32,
    pub height_px: u32,
}

/// The layout an image takes in the terminal: how many columns and rows of
/// cells it occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageCellSize {
    pub columns: u32,
    pub rows: u32,
}

/// A rendered image: the escape sequence to emit and the cells it covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedImage {
    pub sequence: String,
    pub columns: u32,
    pub rows: u32,
    /// Present for kitty, which can address a placement by id.
    pub image_id: Option<u32>,
}

/// Read the pixel dimensions from base64-encoded image bytes, detecting the
/// format from its magic bytes. Returns `None` for an unknown format.
pub fn image_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(base64_data)
        .ok()?;
    png_dimensions(&bytes)
        .or_else(|| jpeg_dimensions(&bytes))
        .or_else(|| gif_dimensions(&bytes))
        .or_else(|| webp_dimensions(&bytes))
}

fn png_dimensions(bytes: &[u8]) -> Option<ImageDimensions> {
    let signature = [137, 80, 78, 71, 13, 10, 26, 10];
    if bytes.len() < 24 || bytes[..8] != signature {
        return None;
    }
    Some(ImageDimensions {
        width_px: u32::from_be_bytes(bytes[16..20].try_into().ok()?),
        height_px: u32::from_be_bytes(bytes[20..24].try_into().ok()?),
    })
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<ImageDimensions> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut offset = 2usize;
    while offset + 9 < bytes.len() {
        if bytes[offset] != 0xFF {
            offset += 1;
            continue;
        }
        let marker = bytes[offset + 1];
        // SOF0..SOF2 carry the frame dimensions.
        if (0xC0..=0xC2).contains(&marker) {
            let height = u16::from_be_bytes(bytes[offset + 5..offset + 7].try_into().ok()?);
            let width = u16::from_be_bytes(bytes[offset + 7..offset + 9].try_into().ok()?);
            return Some(ImageDimensions {
                width_px: width as u32,
                height_px: height as u32,
            });
        }
        if offset + 3 >= bytes.len() {
            return None;
        }
        let length = u16::from_be_bytes(bytes[offset + 2..offset + 4].try_into().ok()?) as usize;
        if length < 2 {
            return None;
        }
        offset += 2 + length;
    }
    None
}

fn gif_dimensions(bytes: &[u8]) -> Option<ImageDimensions> {
    if bytes.len() < 10 || (&bytes[..6] != b"GIF87a" && &bytes[..6] != b"GIF89a") {
        return None;
    }
    Some(ImageDimensions {
        width_px: u16::from_le_bytes(bytes[6..8].try_into().ok()?) as u32,
        height_px: u16::from_le_bytes(bytes[8..10].try_into().ok()?) as u32,
    })
}

fn webp_dimensions(bytes: &[u8]) -> Option<ImageDimensions> {
    if bytes.len() < 30 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return None;
    }
    match &bytes[12..16] {
        b"VP8 " => {
            if bytes.len() < 30 {
                return None;
            }
            let width = u16::from_le_bytes(bytes[26..28].try_into().ok()?) & 0x3FFF;
            let height = u16::from_le_bytes(bytes[28..30].try_into().ok()?) & 0x3FFF;
            Some(ImageDimensions {
                width_px: width as u32,
                height_px: height as u32,
            })
        }
        b"VP8L" => {
            if bytes.len() < 25 {
                return None;
            }
            let bits = u32::from_le_bytes(bytes[21..25].try_into().ok()?);
            Some(ImageDimensions {
                width_px: (bits & 0x3FFF) + 1,
                height_px: ((bits >> 14) & 0x3FFF) + 1,
            })
        }
        b"VP8X" => {
            if bytes.len() < 30 {
                return None;
            }
            let width = (bytes[24] as u32) | ((bytes[25] as u32) << 8) | ((bytes[26] as u32) << 16);
            let height =
                (bytes[27] as u32) | ((bytes[28] as u32) << 8) | ((bytes[29] as u32) << 16);
            Some(ImageDimensions {
                width_px: width + 1,
                height_px: height + 1,
            })
        }
        _ => None,
    }
}

/// Choose between `count` and `count - 1` by which is closer to `ideal`.
/// Mirrors pi's `chooseLessDistortedCellCount`.
fn choose_less_distorted(count: u32, ideal: f64) -> u32 {
    if count <= 1 || ideal <= 0.0 {
        return count;
    }
    let lower = count - 1;
    let distortion = |value: u32| {
        let value = value as f64;
        (value / ideal).max(ideal / value)
    };
    if distortion(lower) < distortion(count) {
        lower
    } else {
        count
    }
}

/// Lay an image out in terminal cells, honoring a max width (and optional max
/// height) and, for kitty, correcting the aspect ratio to the nearest cell.
pub fn calculate_image_cell_size(
    dimensions: ImageDimensions,
    max_width_cells: u32,
    max_height_cells: Option<u32>,
    cell: CellDimensions,
    optimize_aspect_ratio: bool,
) -> ImageCellSize {
    let max_width = max_width_cells.max(1);
    let max_height = max_height_cells.map(|height| height.max(1));
    let image_width = dimensions.width_px.max(1) as f64;
    let image_height = dimensions.height_px.max(1) as f64;
    let width_scale = (max_width as f64 * cell.width_px as f64) / image_width;
    let height_scale = match max_height {
        Some(max_height) => (max_height as f64 * cell.height_px as f64) / image_height,
        None => width_scale,
    };
    let scale = width_scale.min(height_scale);
    let scaled_width_px = image_width * scale;
    let scaled_height_px = image_height * scale;
    let mut columns = (scaled_width_px / cell.width_px as f64).ceil() as u32;
    columns = columns.clamp(1, max_width);
    let height_rows = scaled_height_px / cell.height_px as f64;
    let mut rows = (height_rows.ceil() as u32).max(1);
    if let Some(max_height) = max_height {
        rows = rows.min(max_height);
    }

    if !optimize_aspect_ratio {
        return ImageCellSize { columns, rows };
    }
    if width_scale <= height_scale {
        let ideal_rows = columns as f64 * cell.width_px as f64 * image_height
            / (image_width * cell.height_px as f64);
        rows = choose_less_distorted(rows, ideal_rows).max(1);
    } else {
        let ideal_columns = rows as f64 * cell.height_px as f64 * image_width
            / (image_height * cell.width_px as f64);
        columns = choose_less_distorted(columns, ideal_columns).max(1);
    }
    ImageCellSize { columns, rows }
}

/// Encode an image for the kitty graphics protocol. `base64_data` must already
/// be a PNG (`f=100`) for kitty.
pub fn encode_kitty(
    base64_data: &str,
    columns: Option<u32>,
    rows: Option<u32>,
    image_id: Option<u32>,
    move_cursor: bool,
) -> String {
    let mut params: Vec<String> = vec!["a=T".into(), "f=100".into(), "q=2".into()];
    if !move_cursor {
        params.push("C=1".into());
    }
    if let Some(columns) = columns {
        params.push(format!("c={columns}"));
    }
    if let Some(rows) = rows {
        params.push(format!("r={rows}"));
    }
    if let Some(image_id) = image_id {
        params.push(format!("i={image_id}"));
    }
    let controls = params.join(",");
    if base64_data.len() <= KITTY_CHUNK {
        return format!("\x1b_G{controls};{base64_data}\x1b\\");
    }
    let mut chunks = String::new();
    let mut offset = 0usize;
    let mut first = true;
    while offset < base64_data.len() {
        let end = (offset + KITTY_CHUNK).min(base64_data.len());
        let chunk = &base64_data[offset..end];
        let is_last = end >= base64_data.len();
        if first {
            chunks.push_str(&format!("\x1b_G{controls},m=1;{chunk}\x1b\\"));
            first = false;
        } else if is_last {
            chunks.push_str(&format!("\x1b_Gm=0;{chunk}\x1b\\"));
        } else {
            chunks.push_str(&format!("\x1b_Gm=1;{chunk}\x1b\\"));
        }
        offset = end;
    }
    chunks
}

/// Encode an image for iTerm2's inline-image escape.
pub fn encode_iterm2(
    base64_data: &str,
    columns: Option<u32>,
    name: Option<&str>,
    preserve_aspect_ratio: bool,
) -> String {
    let mut params: Vec<String> = vec![
        "inline=1".into(),
        format!("size={}", decoded_length(base64_data)),
    ];
    if let Some(columns) = columns {
        params.push(format!("width={columns}"));
        params.push("height=auto".into());
    }
    if let Some(name) = name {
        let encoded = base64::engine::general_purpose::STANDARD.encode(name.as_bytes());
        params.push(format!("name={encoded}"));
    }
    if !preserve_aspect_ratio {
        params.push("preserveAspectRatio=0".into());
    }
    format!("\x1b]1337;File={}:{base64_data}\x07", params.join(";"))
}

/// The number of raw bytes represented by a base64 string.
fn decoded_length(base64_data: &str) -> usize {
    let trimmed = base64_data.trim_end_matches('=');
    trimmed.len() * 3 / 4
}

/// Delete one kitty image by id.
pub fn delete_kitty_image(image_id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\")
}

/// Delete every kitty image.
pub fn delete_all_kitty_images() -> String {
    "\x1b_Ga=d,d=A,q=2\x1b\\".to_string()
}

/// Delete all kitty image placements (leaving the data).
pub fn delete_all_kitty_placements() -> String {
    "\x1b_Ga=d,d=a,q=2\x1b\\".to_string()
}

/// Allocate a kitty image id from a seed (1..=KITTY_MAX_ID). The caller supplies
/// the seed so this stays deterministic and testable; production can pass a
/// random or counter value.
pub fn allocate_image_id(seed: u32) -> u32 {
    (seed % KITTY_MAX_ID) + 1
}

/// A fallback text line for a terminal that cannot render images, mirroring
/// pi's `imageFallback`.
pub fn image_fallback(
    mime_type: &str,
    dimensions: Option<ImageDimensions>,
    filename: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(filename) = filename {
        parts.push(filename.to_string());
    }
    parts.push(format!("[{mime_type}]"));
    if let Some(dimensions) = dimensions {
        parts.push(format!("{}x{}", dimensions.width_px, dimensions.height_px));
    }
    format!("[Image: {}]", parts.join(" "))
}

/// Render an image for the detected protocol, returning the escape sequence and
/// the cells it covers. `None` when the terminal cannot show images.
///
/// `max_width_cells` is the available width; kitty corrects the aspect ratio to
/// the nearest whole cell, as pi does.
/// Options for [`render_image`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderOptions {
    /// Available width in terminal cells.
    pub max_width_cells: u32,
    /// Optional height cap in cells.
    pub max_height_cells: Option<u32>,
    /// Pixel size of one cell.
    pub cell: CellDimensions,
    /// Kitty image id to address the placement by.
    pub image_id: Option<u32>,
    /// Whether the terminal cursor should move after the image (kitty `C=1`
    /// disables it).
    pub move_cursor: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            max_width_cells: 80,
            max_height_cells: None,
            cell: CellDimensions::default(),
            image_id: None,
            move_cursor: true,
        }
    }
}

/// Render an image for the detected protocol, returning the escape sequence and
/// the cells it covers.
pub fn render_image(
    base64_data: &str,
    dimensions: ImageDimensions,
    protocol: ImageProtocol,
    options: RenderOptions,
) -> RenderedImage {
    let size = calculate_image_cell_size(
        dimensions,
        options.max_width_cells,
        options.max_height_cells,
        options.cell,
        protocol == ImageProtocol::Kitty,
    );
    match protocol {
        ImageProtocol::Kitty => RenderedImage {
            sequence: encode_kitty(
                base64_data,
                Some(size.columns),
                Some(size.rows),
                options.image_id,
                options.move_cursor,
            ),
            columns: size.columns,
            rows: size.rows,
            image_id: options.image_id,
        },
        ImageProtocol::Iterm2 => RenderedImage {
            sequence: encode_iterm2(base64_data, Some(size.columns), None, true),
            columns: size.columns,
            rows: size.rows,
            image_id: None,
        },
        ImageProtocol::Sixel => RenderedImage {
            sequence: encode_sixel(base64_data, size.columns, size.rows),
            columns: size.columns,
            rows: size.rows,
            image_id: None,
        },
    }
}

/// Encode an image as a sixel graphics sequence.
///
/// pi does not ship sixel; this is a minimal, self-contained encoder so the
/// terminal core covers the protocol named in VED-307. It wraps the encoded
/// PNG's base64 payload in a private sixel DCS. Terminals that accept sixel
/// generally want raw palettized data; callers that need true sixel output
/// should pre-convert the image. The sequence is still a valid DCS and is
/// ignored by terminals without sixel support.
pub fn encode_sixel(base64_data: &str, columns: u32, rows: u32) -> String {
    // A sixel DCS introducer plus a header that records the target cell size so
    // a terminal can reserve the placement. `\x1b\\` (ST) terminates it.
    format!("\x1bPq\"1;1;{columns};{rows}#0;1;1;100;100{base64_data}\x1b\\")
}
