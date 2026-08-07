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
#[allow(dead_code)] // old faithful's 1-bit path — dormant post colour-deprecation, kept for reversibility
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
#[allow(dead_code)] // dormant 1-bit path (see above)
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
#[allow(dead_code)] // dormant 1-bit path (see above)
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
#[allow(dead_code)] // dormant 1-bit path (see above)
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

// ================= 6-colour (Spectra 6 / GDEP073E01) render path =================
// 800×480, one byte per pixel = the panel's source code. Ported one-for-one from
// the spectra-pipeline: measured-appearance palette, S-curve tone-map, sRGB→Lab,
// serpentine Floyd–Steinberg with a luminance-weighted Lab match and an error
// clamp. Old faithful's 1-bit path above is untouched; this is the parallel path
// rev2 (the colour Beacon) fetches.

pub const CW: usize = 800;
pub const CH: usize = 480;
pub const COLOR_FRAME_BYTES: usize = CW * CH; // 384,000, 1 byte/px

// Measured panel appearance (sRGB) paired with the source code fed to PIC_display().
const PAL: [([u8; 3], u8); 6] = [
    ([2, 2, 2], 0x00),       // black
    ([190, 200, 200], 0xFF), // white
    ([205, 202, 0], 0xFC),   // yellow
    ([135, 19, 0], 0xE0),    // red
    ([5, 64, 158], 0x03),    // blue
    ([39, 102, 60], 0x1C),   // green
];

// Luminance-weighted Lab distance: dd = wL·dL² + wC·(da²+db²). wL up penalises
// luminance mismatch, so darks pick dark inks (kills the yellow-in-black sparkle).
// wL = 4 is the validated sweet spot; per-image S-CIELAB auto-tune is the follow-up.
const WL: f32 = 4.0;
const WC: f32 = 1.0;

fn srgb_to_lab(r: u8, g: u8, b: u8) -> [f32; 3] {
    let lin = |c: u8| -> f32 {
        let s = c as f32 / 255.0;
        if s <= 0.04045 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
    };
    let (rl, gl, bl) = (lin(r), lin(g), lin(b));
    let x = 0.4124564 * rl + 0.3575761 * gl + 0.1804375 * bl;
    let y = 0.2126729 * rl + 0.7151522 * gl + 0.0721750 * bl;
    let z = 0.0193339 * rl + 0.1191920 * gl + 0.9503041 * bl;
    let (xn, yn, zn) = (0.95047f32, 1.0f32, 1.08883f32);
    let lab_f = |t: f32| -> f32 {
        let d = 6.0f32 / 29.0;
        if t > d * d * d { t.cbrt() } else { t / (3.0 * d * d) + 4.0 / 29.0 }
    };
    let (fx, fy, fz) = (lab_f(x / xn), lab_f(y / yn), lab_f(z / zn));
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

// S-curve tone LUT (shadow-lift + highlight-compress), applied per RGB channel
// before Lab. strength .9, shadowBoost .1, highlightCompress -1.5, midpoint .5.
fn scurve_lut() -> [u8; 256] {
    let (strength, shadow_boost, hi_compress, midpoint) = (0.9f32, 0.1f32, -1.5f32, 0.5f32);
    let mid = midpoint.clamp(0.01, 0.99);
    let s_exp = (1.0 - strength * shadow_boost * 1.5).clamp(0.15, 3.0);
    let h_exp = (1.0 - strength * hi_compress).clamp(0.15, 3.0);
    let mut lut = [0u8; 256];
    for (i, e) in lut.iter_mut().enumerate() {
        let v = i as f32 / 255.0;
        let out = if v <= mid {
            (v / mid).powf(s_exp) * mid
        } else {
            mid + ((v - mid) / (1.0 - mid)).powf(h_exp) * (1.0 - mid)
        };
        *e = (out * 255.0).clamp(0.0, 255.0) as u8;
    }
    lut
}

#[inline]
fn nearest(pal_lab: &[[f32; 3]; 6], c: [f32; 3]) -> usize {
    let mut bi = 0usize;
    let mut bd = f32::INFINITY;
    for (i, p) in pal_lab.iter().enumerate() {
        let (dl, da, db) = (c[0] - p[0], c[1] - p[1], c[2] - p[2]);
        let dd = WL * dl * dl + WC * (da * da + db * db);
        if dd < bd {
            bd = dd;
            bi = i;
        }
    }
    bi
}

#[inline]
fn diffuse_lab(lab: &mut [[f32; 3]], x: i32, y: i32, e: [f32; 3], num: f32) {
    if x < 0 || x >= CW as i32 || y < 0 || y >= CH as i32 {
        return;
    }
    let j = y as usize * CW + x as usize;
    lab[j][0] += e[0] * num;
    lab[j][1] += e[1] * num;
    lab[j][2] += e[2] * num;
}

/// Serpentine Floyd–Steinberg over the working Lab buffer straight into the
/// panel's 1-byte-per-pixel source-code frame. Alternating scan direction cancels
/// directional streaking; each match is made on a value clamped to a plausible Lab
/// range so accumulated error can't run away out of saturated regions.
fn serpentine_fs(lab: &mut [[f32; 3]], pal_lab: &[[f32; 3]; 6]) -> Vec<u8> {
    let mut out = vec![0xFFu8; COLOR_FRAME_BYTES]; // default = white code
    for y in 0..CH {
        let rev = y & 1 == 1;
        let dir: i32 = if rev { -1 } else { 1 };
        for xi in 0..CW {
            let x = if rev { CW - 1 - xi } else { xi };
            let i0 = y * CW + x;
            let c0 = lab[i0][0].clamp(0.0, 100.0);
            let c1 = lab[i0][1].clamp(-128.0, 127.0);
            let c2 = lab[i0][2].clamp(-128.0, 127.0);
            let bi = nearest(pal_lab, [c0, c1, c2]);
            out[i0] = PAL[bi].1;
            let p = pal_lab[bi];
            let e = [c0 - p[0], c1 - p[1], c2 - p[2]];
            let (xg, yg) = (x as i32, y as i32);
            diffuse_lab(lab, xg + dir, yg, e, 7.0 / 16.0);
            diffuse_lab(lab, xg - dir, yg + 1, e, 3.0 / 16.0);
            diffuse_lab(lab, xg, yg + 1, e, 5.0 / 16.0);
            diffuse_lab(lab, xg + dir, yg + 1, e, 1.0 / 16.0);
        }
    }
    out
}

/// Decode an image, stretch to 800×480 (generated art is authored at panel
/// aspect), S-curve tone-map, convert to Lab, and serpentine-dither to the six
/// measured inks. Returns the 384,000-byte source-code frame rev2 blits verbatim.
/// The colour analogue of `image_to_frame`.
pub fn image_to_color_frame(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory(bytes).map_err(|e| format!("decode: {e}"))?;
    let rgb = img
        .resize_exact(CW as u32, CH as u32, image::imageops::FilterType::Lanczos3)
        .to_rgb8();
    let src = rgb.as_raw();

    let lut = scurve_lut();
    let mut pal_lab = [[0.0f32; 3]; 6];
    for (i, (c, _)) in PAL.iter().enumerate() {
        pal_lab[i] = srgb_to_lab(c[0], c[1], c[2]);
    }

    let mut lab = vec![[0.0f32; 3]; CW * CH];
    for (i, px) in lab.iter_mut().enumerate() {
        let r = lut[src[i * 3] as usize];
        let g = lut[src[i * 3 + 1] as usize];
        let b = lut[src[i * 3 + 2] as usize];
        *px = srgb_to_lab(r, g, b);
    }

    Ok(serpentine_fs(&mut lab, &pal_lab))
}

/// Unpack the 6-colour source-code frame into a viewable PNG in the panel's
/// *measured* colours — the digital twin, so `recall_beacon` shows the model what
/// the colour glass actually looks like. Colour analogue of `frame_to_png`.
pub fn color_frame_to_png(frame: &[u8]) -> Result<Vec<u8>, String> {
    if frame.len() != COLOR_FRAME_BYTES {
        return Err(format!(
            "colour frame is {} bytes, expected {COLOR_FRAME_BYTES}",
            frame.len()
        ));
    }
    let mut img = image::RgbImage::new(CW as u32, CH as u32);
    for (i, code) in frame.iter().enumerate() {
        let rgb = PAL
            .iter()
            .find(|entry| entry.1 == *code)
            .map(|entry| entry.0)
            .unwrap_or([0, 0, 0]);
        img.put_pixel((i % CW) as u32, (i / CW) as u32, image::Rgb(rgb));
    }
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .map_err(|e| format!("png encode: {e}"))?;
    Ok(out.into_inner())
}
