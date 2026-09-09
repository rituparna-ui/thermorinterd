//! Rendering: text, images, and QR codes -> packed 384px printer rows.

use ab_glyph::{Font, FontVec, Glyph, PxScale, ScaleFont};
use anyhow::{anyhow, Context, Result};
use image::{imageops, GrayImage, Luma};

use crate::protocol::{encode_row, BYTES_PER_ROW, PRINT_WIDTH};

pub type Row = [u8; BYTES_PER_ROW];

#[derive(Clone, Copy, Debug)]
pub struct ImageOptions {
    pub dither: bool,
    pub threshold: u8,
    pub invert: bool,
}

impl Default for ImageOptions {
    fn default() -> Self {
        Self { dither: true, threshold: 128, invert: false }
    }
}

const FONT_CANDIDATES: &[&str] = &[
    // macOS
    "/System/Library/Fonts/Supplemental/Arial.ttf",
    "/System/Library/Fonts/Supplemental/Verdana.ttf",
    "/System/Library/Fonts/Geneva.ttf",
    "/System/Library/Fonts/Monaco.ttf",
    // Linux
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    // Windows
    "C:\\Windows\\Fonts\\arial.ttf",
    "C:\\Windows\\Fonts\\segoeui.ttf",
];

/// Load a TrueType font from an explicit path or the first available system font.
pub fn load_font(explicit: Option<&str>) -> Result<FontVec> {
    let mut tried = Vec::new();
    if let Some(p) = explicit {
        let bytes = std::fs::read(p).with_context(|| format!("reading font {p}"))?;
        return FontVec::try_from_vec(bytes).map_err(|e| anyhow!("invalid font {p}: {e}"));
    }
    for p in FONT_CANDIDATES {
        if let Ok(bytes) = std::fs::read(p) {
            if let Ok(f) = FontVec::try_from_vec(bytes) {
                return Ok(f);
            }
        }
        tried.push(*p);
    }
    Err(anyhow!(
        "no usable font found; set --font PATH. Tried: {}",
        tried.join(", ")
    ))
}

/// Pack a GrayImage (must be 384 wide) to rows; pixel < 128 => black dot.
fn pack_gray(img: &GrayImage) -> Vec<Row> {
    let (w, h) = img.dimensions();
    let w = w as usize;
    let mut rows = Vec::with_capacity(h as usize);
    let mut bits = vec![false; PRINT_WIDTH];
    for y in 0..h {
        for b in bits.iter_mut() {
            *b = false;
        }
        for x in 0..w.min(PRINT_WIDTH) {
            let p = img.get_pixel(x as u32, y)[0];
            bits[x] = p < 128;
        }
        rows.push(encode_row(&bits));
    }
    rows
}

/// Center a grayscale image on a white 384-wide canvas (no scaling up).
fn center_on_canvas(img: &GrayImage) -> GrayImage {
    let (w, h) = img.dimensions();
    if w == PRINT_WIDTH as u32 {
        return img.clone();
    }
    let mut canvas = GrayImage::from_pixel(PRINT_WIDTH as u32, h, Luma([255]));
    let ox = ((PRINT_WIDTH as i64 - w as i64) / 2).max(0) as i64;
    imageops::overlay(&mut canvas, img, ox, 0);
    canvas
}

fn reduce(mut img: GrayImage, opts: &ImageOptions) -> GrayImage {
    if opts.invert {
        for p in img.pixels_mut() {
            p[0] = 255 - p[0];
        }
    }
    if opts.dither {
        imageops::dither(&mut img, &imageops::BiLevel);
    } else {
        let t = opts.threshold;
        for p in img.pixels_mut() {
            p[0] = if p[0] < t { 0 } else { 255 };
        }
    }
    img
}

/// Decode image bytes, scale to 384 wide, dither/threshold, return rows.
pub fn image_to_rows(bytes: &[u8], opts: &ImageOptions) -> Result<Vec<Row>> {
    let img = image::load_from_memory(bytes).context("decoding image")?;
    let luma = img.to_luma8();
    let (w, h) = luma.dimensions();
    let new_h = ((h as f32) * PRINT_WIDTH as f32 / w as f32).round().max(1.0) as u32;
    let scaled = imageops::resize(&luma, PRINT_WIDTH as u32, new_h, imageops::FilterType::Lanczos3);
    Ok(pack_gray(&reduce(scaled, opts)))
}

/// Rasterize a PDF to per-page PNGs using an available external tool
/// (pdftoppm, mutool, or gs), then convert each page to rows.
pub fn pdf_to_rows(bytes: &[u8], opts: &ImageOptions, dpi: u32) -> Result<Vec<Row>> {
    let tool = detect_pdf_tool()
        .ok_or_else(|| anyhow!("no PDF rasterizer found; install poppler (pdftoppm), mupdf (mutool), or ghostscript (gs)"))?;
    let dir = std::env::temp_dir().join(format!("thermorinterd-pdf-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let input = dir.join("in.pdf");
    std::fs::write(&input, bytes)?;

    let pngs = tool.render(&input, &dir, dpi)?;
    if pngs.is_empty() {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(anyhow!("PDF produced no pages"));
    }
    let mut rows = Vec::new();
    for png in &pngs {
        let data = std::fs::read(png)?;
        rows.extend(image_to_rows(&data, opts)?);
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(rows)
}

enum PdfTool {
    PdfToPpm,
    MuTool,
    Ghostscript,
}

fn which(bin: &str) -> bool {
    std::process::Command::new(bin)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success() || true) // some tools return nonzero for --version
        .unwrap_or(false)
}

fn detect_pdf_tool() -> Option<PdfTool> {
    if which("pdftoppm") {
        Some(PdfTool::PdfToPpm)
    } else if which("mutool") {
        Some(PdfTool::MuTool)
    } else if which("gs") {
        Some(PdfTool::Ghostscript)
    } else {
        None
    }
}

impl PdfTool {
    fn render(&self, input: &std::path::Path, dir: &std::path::Path, dpi: u32) -> Result<Vec<std::path::PathBuf>> {
        use std::process::Command;
        let prefix = dir.join("page");
        let status = match self {
            PdfTool::PdfToPpm => Command::new("pdftoppm")
                .args(["-png", "-r", &dpi.to_string()])
                .arg(input)
                .arg(&prefix)
                .status()?,
            PdfTool::MuTool => Command::new("mutool")
                .args(["draw", "-o"])
                .arg(dir.join("page-%d.png"))
                .args(["-r", &dpi.to_string()])
                .arg(input)
                .status()?,
            PdfTool::Ghostscript => Command::new("gs")
                .args([
                    "-dQUIET", "-dNOPAUSE", "-dBATCH", "-sDEVICE=png16m",
                    &format!("-r{dpi}"),
                ])
                .arg(format!("-sOutputFile={}", dir.join("page-%d.png").display()))
                .arg(input)
                .status()?,
        };
        if !status.success() {
            return Err(anyhow!("PDF rasterizer failed"));
        }
        let mut pngs: Vec<_> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("png"))
            .collect();
        pngs.sort();
        Ok(pngs)
    }
}

/// Render a QR code (optionally with a caption) to rows.
pub fn qr_to_rows(font: &FontVec, data: &str, caption: Option<&str>) -> Result<Vec<Row>> {
    let code = qrcode::QrCode::new(data.as_bytes()).context("building QR")?;
    let qr: GrayImage = code
        .render::<Luma<u8>>()
        .min_dimensions(320, 320)
        .max_dimensions(PRINT_WIDTH as u32, PRINT_WIDTH as u32)
        .quiet_zone(true)
        .build();
    let qr = center_on_canvas(&qr);
    let mut rows = pack_gray(&qr);
    if let Some(cap) = caption {
        if !cap.is_empty() {
            rows.extend(text_to_rows(font, cap, 22.0, Align::Center)?);
        }
    }
    Ok(rows)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

fn line_width<F: Font, SF: ScaleFont<F>>(scaled: &SF, text: &str) -> f32 {
    let mut w = 0.0;
    for c in text.chars() {
        w += scaled.h_advance(scaled.glyph_id(c));
    }
    w
}

fn wrap<F: Font, SF: ScaleFont<F>>(scaled: &SF, text: &str, max_w: f32) -> Vec<String> {
    let mut lines = Vec::new();
    for para in text.split('\n') {
        if para.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut cur = String::new();
        for word in para.split(' ') {
            let trial = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
            if line_width(scaled, &trial) <= max_w {
                cur = trial;
            } else {
                if !cur.is_empty() {
                    lines.push(std::mem::take(&mut cur));
                }
                // hard-break overly long single word
                let mut w = word.to_string();
                while line_width(scaled, &w) > max_w && w.chars().count() > 1 {
                    let mut cut = w.chars().count();
                    while cut > 1
                        && line_width(scaled, &w.chars().take(cut).collect::<String>()) > max_w
                    {
                        cut -= 1;
                    }
                    let head: String = w.chars().take(cut).collect();
                    lines.push(head);
                    w = w.chars().skip(cut).collect();
                }
                cur = w;
            }
        }
        lines.push(cur);
    }
    lines
}

/// Render wrapped text to crisp (thresholded) rows.
pub fn text_to_rows(font: &FontVec, text: &str, size: f32, align: Align) -> Result<Vec<Row>> {
    let scale = PxScale::from(size);
    let scaled = font.as_scaled(scale);
    let margin = 4.0f32;
    let max_w = PRINT_WIDTH as f32 - 2.0 * margin;
    let lines = wrap(&scaled, text, max_w);

    let ascent = scaled.ascent();
    let descent = scaled.descent();
    let line_h = (ascent - descent + 4.0).ceil();
    let height = (line_h * lines.len().max(1) as f32 + 2.0 * margin).ceil() as u32;

    let mut img = GrayImage::from_pixel(PRINT_WIDTH as u32, height, Luma([255]));

    let mut y = margin;
    for ln in &lines {
        let w = line_width(&scaled, ln);
        let x0 = match align {
            Align::Left => margin,
            Align::Center => (PRINT_WIDTH as f32 - w) / 2.0,
            Align::Right => PRINT_WIDTH as f32 - margin - w,
        };
        let baseline = y + ascent;
        let mut pen_x = x0;
        for c in ln.chars() {
            let gid = scaled.glyph_id(c);
            let mut glyph: Glyph = gid.with_scale(scale);
            glyph.position = ab_glyph::point(pen_x, baseline);
            if let Some(outline) = font.outline_glyph(glyph) {
                let bounds = outline.px_bounds();
                outline.draw(|gx, gy, cov| {
                    let px = bounds.min.x as i32 + gx as i32;
                    let py = bounds.min.y as i32 + gy as i32;
                    if px >= 0 && py >= 0 && (px as u32) < PRINT_WIDTH as u32 && (py as u32) < height
                    {
                        if cov > 0.5 {
                            img.put_pixel(px as u32, py as u32, Luma([0]));
                        }
                    }
                });
            }
            pen_x += scaled.h_advance(gid);
        }
        y += line_h;
    }
    Ok(pack_gray(&img))
}
