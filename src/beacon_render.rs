//! Server-side 1-bit frame rendering for the e-paper Beacon. The Worker bakes
//! the exact 66,240-byte buffer the panel expects and the device blits it
//! verbatim — no on-device rendering. Buffer format (confirmed against the
//! vendor driver's `EPD_5in0_Display` + `Paint_SetPixel`):
//!   960x552, 120 bytes/row, row-major; 8 px/byte, MSB = leftmost pixel;
//!   bit 1 = white, 0 = black; an all-`0xFF` buffer is a white screen.
//!
//! Font24 is the vendor 17x24 monospace bitmap, ported byte-faithfully (the
//! glyph table is `include_bytes!`'d) so text matches what already rendered
//! crisply on the glass. Glyphs run from ASCII ' ' (0x20), 72 bytes each.

pub const W: usize = 960;
pub const H: usize = 552;
const ROW: usize = W / 8; // 120 bytes per row
pub const FRAME_BYTES: usize = ROW * H; // 66,240

const FONT_W: usize = 17;
const FONT_H: usize = 24;
const GLYPH_ROW_BYTES: usize = FONT_W.div_ceil(8); // 3
const GLYPH_BYTES: usize = FONT_H * GLYPH_ROW_BYTES; // 72
static FONT24: &[u8] = include_bytes!("beacon_font24.bin");

#[inline]
fn set_black(buf: &mut [u8], x: usize, y: usize) {
    if x >= W || y >= H {
        return;
    }
    buf[x / 8 + y * ROW] &= !(0x80u8 >> (x % 8));
}

/// Blit one ASCII glyph with its top-left at (x,y), black on the white frame.
/// Bytes outside printable ASCII are skipped (text is ASCII-folded upstream).
fn draw_char(buf: &mut [u8], x: usize, y: usize, ch: u8) {
    if !(0x20..=0x7e).contains(&ch) {
        return;
    }
    let base = (ch - 0x20) as usize * GLYPH_BYTES;
    for page in 0..FONT_H {
        let row = base + page * GLYPH_ROW_BYTES;
        for col in 0..FONT_W {
            if FONT24[row + col / 8] & (0x80 >> (col % 8)) != 0 {
                set_black(buf, x + col, y + page);
            }
        }
    }
}

const MARGIN_X: usize = 28;
const TOP_Y: usize = 40;
const LINE_PITCH: usize = FONT_H + 8; // 32 px

/// Render ASCII text into a fresh white frame: greedy word-wrap to the panel
/// width, clipped to its height. The caller folds to ASCII first, so byte
/// indices are char indices (slicing a long word never splits a UTF-8 char).
pub fn render_text_frame(text: &str) -> Vec<u8> {
    let mut buf = vec![0xFFu8; FRAME_BYTES];
    let max_cols = (W - 2 * MARGIN_X) / FONT_W; // ~53
    let max_lines = (H - TOP_Y) / LINE_PITCH;

    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if line.is_empty() {
            line.push_str(word);
        } else if line.len() + 1 + word.len() <= max_cols {
            line.push(' ');
            line.push_str(word);
        } else {
            lines.push(std::mem::take(&mut line));
            line.push_str(word);
        }
        // Hard-break a single word longer than a full line.
        while line.len() > max_cols {
            lines.push(line[..max_cols].to_string());
            line = line[max_cols..].to_string();
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }

    for (i, l) in lines.iter().take(max_lines).enumerate() {
        let y = TOP_Y + i * LINE_PITCH;
        let mut x = MARGIN_X;
        for &b in l.as_bytes() {
            draw_char(&mut buf, x, y, b);
            x += FONT_W;
        }
    }
    buf
}

/// Diagnostic frame: 3px border, an 80x80 top-left origin marker (reveals any
/// mirror/flip), and a corner-to-corner diagonal (reveals a row-stride error).
/// Kept for re-proving the packing/transport/blit path after render changes.
pub fn test_frame() -> Vec<u8> {
    let mut buf = vec![0xFFu8; FRAME_BYTES];

    for x in 0..W {
        for t in 0..3 {
            set_black(&mut buf, x, t);
            set_black(&mut buf, x, H - 1 - t);
        }
    }
    for y in 0..H {
        for t in 0..3 {
            set_black(&mut buf, t, y);
            set_black(&mut buf, W - 1 - t, y);
        }
    }
    for y in 0..80 {
        for x in 0..80 {
            set_black(&mut buf, x, y);
        }
    }
    for x in 0..W {
        set_black(&mut buf, x, x * (H - 1) / (W - 1));
    }

    buf
}

/// Push one neighbour's share of a Floyd–Steinberg quantisation error.
fn diffuse(gray: &mut [u8], x: i32, y: i32, err: i32, num: i32) {
    if x < 0 || x >= W as i32 || y < 0 || y >= H as i32 {
        return;
    }
    let j = y as usize * W + x as usize;
    gray[j] = (gray[j] as i32 + err * num / 16).clamp(0, 255) as u8;
}

/// Floyd–Steinberg dither a W×H grayscale buffer (0 = black … 255 = white)
/// straight into the panel's packed 1-bit frame. Each pixel snaps to black or
/// white, then the rounding error is pushed onto the not-yet-visited neighbours
/// (7/16 right, 3/16 down-left, 5/16 down, 1/16 down-right) — so clusters of
/// black/white dots average back out to the original grey. Mutates `gray` as
/// the error spreads.
pub fn dither_pack(gray: &mut [u8]) -> Vec<u8> {
    let mut buf = vec![0xFFu8; FRAME_BYTES]; // all white
    for y in 0..H {
        for x in 0..W {
            let old = gray[y * W + x] as i32;
            let new = if old < 128 { 0 } else { 255 };
            if new == 0 {
                set_black(&mut buf, x, y);
            }
            let err = old - new;
            let (xi, yi) = (x as i32, y as i32);
            diffuse(gray, xi + 1, yi, err, 7);
            diffuse(gray, xi - 1, yi + 1, err, 3);
            diffuse(gray, xi, yi + 1, err, 5);
            diffuse(gray, xi + 1, yi + 1, err, 1);
        }
    }
    buf
}

/// Decode an image (PNG/JPEG/WebP), fit it to the panel letterboxed on white
/// with aspect preserved, grayscale it, lift the near-white background to clean
/// white (so line art dithers crisp instead of speckling), then Floyd–Steinberg
/// into the packed 1-bit frame. This is the whole image→frame path the bake
/// will run on Ideogram output; for now it runs on the sample PNGs at fetch
/// time. (The offline Python prep, ported one-for-one into the Worker.)
pub fn image_to_frame(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory(bytes).map_err(|e| format!("decode: {e}"))?;
    // Resize fits *within* W×H preserving aspect, so the result is ≤ W×H.
    let fitted = img
        .resize(W as u32, H as u32, image::imageops::FilterType::Lanczos3)
        .to_luma8();
    let (fw, fh) = (fitted.width() as usize, fitted.height() as usize);
    let src = fitted.as_raw();

    // Centre the fitted image on a white W×H canvas (letterbox).
    let mut gray = vec![255u8; W * H];
    let ox = (W - fw) / 2;
    let oy = (H - fh) / 2;
    for y in 0..fh {
        for x in 0..fw {
            let v = src[y * fw + x];
            gray[(oy + y) * W + (ox + x)] = if v > 230 { 255 } else { v };
        }
    }
    Ok(dither_pack(&mut gray))
}

/// Unpack the panel's packed 1-bit frame (the exact `FRAME_BYTES` buffer the
/// device blits) back into a viewable PNG — so `recall_beacon` can hand the model
/// the literal image on the glass, dither grain and all. Inverse of the packing
/// in `set_black`/`dither_pack`: MSB = leftmost pixel, bit 1 = white, 0 = black.
pub fn frame_to_png(frame: &[u8]) -> Result<Vec<u8>, String> {
    if frame.len() != FRAME_BYTES {
        return Err(format!("frame is {} bytes, expected {FRAME_BYTES}", frame.len()));
    }
    let mut img = image::GrayImage::new(W as u32, H as u32);
    for y in 0..H {
        for x in 0..W {
            let bit = (frame[x / 8 + y * ROW] >> (7 - (x % 8))) & 1;
            let v = if bit == 1 { 255u8 } else { 0u8 };
            img.put_pixel(x as u32, y as u32, image::Luma([v]));
        }
    }
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageLuma8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .map_err(|e| format!("png encode: {e}"))?;
    Ok(out.into_inner())
}
