//! Turning screen pictures into small tiles and back.
//!
//! Pipeline on the host for every monitor and every picture:
//! 1. scale to the chosen size,
//! 2. reduce colours to the chosen depth,
//! 3. compare with the previous picture in 64×64 tiles,
//! 4. send only changed tiles (neighbours merged into strips) as JPEG/PNG
//!    (Auto: lossless palette PNG for text/windows, JPEG for photos, and
//!    rough JPEG parts sharpened once they stop changing).

use crate::proto::{FLAG_LAST, TileFormat, TileHeader};
use crate::quality::{Codec, ColorDepth, Quality};
use anyhow::{Context, Result};

pub const TILE: u32 = 64;
/// Longest merged strip of changed tiles.
pub const MAX_STRIP: u32 = 512;

/// An RGBA picture (4 bytes per pixel, rows top to bottom).
#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Frame({}x{})", self.width, self.height)
    }
}

impl Frame {
    pub fn new(width: u32, height: u32) -> Frame {
        Frame { width, height, rgba: vec![0; (width * height * 4) as usize] }
    }

    pub fn filled(width: u32, height: u32, rgba: [u8; 4]) -> Frame {
        let mut f = Frame::new(width, height);
        for px in f.rgba.chunks_exact_mut(4) {
            px.copy_from_slice(&rgba);
        }
        f
    }

    /// From BGRA / BGRX bytes as delivered by X11 and Windows GDI.
    /// `stride` is bytes per source row.
    pub fn from_bgra(width: u32, height: u32, bgra: &[u8], stride: usize) -> Frame {
        let mut f = Frame::new(width, height);
        for y in 0..height as usize {
            let src = &bgra[y * stride..y * stride + width as usize * 4];
            let dst = &mut f.rgba[y * width as usize * 4..(y + 1) * width as usize * 4];
            for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = 255;
            }
        }
        f
    }

    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        [self.rgba[i], self.rgba[i + 1], self.rgba[i + 2], self.rgba[i + 3]]
    }

    pub fn set_pixel(&mut self, x: u32, y: u32, c: [u8; 4]) {
        let i = ((y * self.width + x) * 4) as usize;
        self.rgba[i..i + 4].copy_from_slice(&c);
    }

    pub fn fill_rect(&mut self, r: Rect, c: [u8; 4]) {
        for y in r.y..(r.y + r.h).min(self.height) {
            for x in r.x..(r.x + r.w).min(self.width) {
                self.set_pixel(x, y, c);
            }
        }
    }

    /// Copy `src` into this picture at (`x`, `y`) (clipped).
    pub fn blit(&mut self, src: &Frame, x: u32, y: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let w = src.width.min(self.width - x) as usize;
        for row in 0..src.height.min(self.height - y) as usize {
            let s = &src.rgba[row * src.width as usize * 4..][..w * 4];
            let d0 = ((y as usize + row) * self.width as usize + x as usize) * 4;
            self.rgba[d0..d0 + w * 4].copy_from_slice(s);
        }
    }

    pub fn crop(&self, r: Rect) -> Frame {
        let mut out = Frame::new(r.w, r.h);
        for row in 0..r.h as usize {
            let s0 = ((r.y as usize + row) * self.width as usize + r.x as usize) * 4;
            out.rgba[row * r.w as usize * 4..(row + 1) * r.w as usize * 4]
                .copy_from_slice(&self.rgba[s0..s0 + r.w as usize * 4]);
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Shrink a picture by averaging the source pixels behind each new pixel.
pub fn scale(src: &Frame, w: u32, h: u32) -> Frame {
    if w == src.width && h == src.height {
        return src.clone();
    }
    let w = w.max(1);
    let h = h.max(1);
    let mut out = Frame::new(w, h);
    let xs: Vec<(usize, usize)> = (0..w)
        .map(|x| {
            let a = (x as u64 * src.width as u64 / w as u64) as usize;
            let b = (((x + 1) as u64 * src.width as u64).div_ceil(w as u64) as usize).max(a + 1);
            (a, b.min(src.width as usize))
        })
        .collect();
    let sw = src.width as usize;
    for y in 0..h {
        let y0 = (y as u64 * src.height as u64 / h as u64) as usize;
        let y1 = ((((y + 1) as u64 * src.height as u64).div_ceil(h as u64)) as usize).max(y0 + 1).min(src.height as usize);
        for (x, &(x0, x1)) in xs.iter().enumerate() {
            let mut acc = [0u32; 3];
            let mut n = 0u32;
            for sy in y0..y1 {
                let row = &src.rgba[(sy * sw + x0) * 4..(sy * sw + x1) * 4];
                for px in row.chunks_exact(4) {
                    acc[0] += px[0] as u32;
                    acc[1] += px[1] as u32;
                    acc[2] += px[2] as u32;
                    n += 1;
                }
            }
            let i = (y as usize * w as usize + x) * 4;
            out.rgba[i] = (acc[0] / n) as u8;
            out.rgba[i + 1] = (acc[1] / n) as u8;
            out.rgba[i + 2] = (acc[2] / n) as u8;
            out.rgba[i + 3] = 255;
        }
    }
    out
}

/// Reduce colours in place.
pub fn quantize(f: &mut Frame, depth: ColorDepth) {
    match depth {
        ColorDepth::Full => {}
        ColorDepth::High => {
            for px in f.rgba.chunks_exact_mut(4) {
                px[0] = (px[0] & 0xF8) | (px[0] >> 5);
                px[1] = (px[1] & 0xFC) | (px[1] >> 6);
                px[2] = (px[2] & 0xF8) | (px[2] >> 5);
            }
        }
        ColorDepth::Low => {
            for px in f.rgba.chunks_exact_mut(4) {
                let idx = index_332(px[0], px[1], px[2]);
                let c = color_332(idx);
                px[0] = c[0];
                px[1] = c[1];
                px[2] = c[2];
            }
        }
        ColorDepth::Gray => {
            for px in f.rgba.chunks_exact_mut(4) {
                let y = luma(px[0], px[1], px[2]);
                px[0] = y;
                px[1] = y;
                px[2] = y;
            }
        }
    }
}

pub fn luma(r: u8, g: u8, b: u8) -> u8 {
    ((77 * r as u32 + 150 * g as u32 + 29 * b as u32) >> 8) as u8
}

/// 3 bits red, 3 bits green, 2 bits blue, rounded to the nearest level.
pub fn index_332(r: u8, g: u8, b: u8) -> u8 {
    let r3 = ((r as u32 * 7 + 127) / 255) as u8;
    let g3 = ((g as u32 * 7 + 127) / 255) as u8;
    let b2 = ((b as u32 * 3 + 127) / 255) as u8;
    (r3 << 5) | (g3 << 2) | b2
}

pub fn color_332(i: u8) -> [u8; 3] {
    let r = (i >> 5) & 7;
    let g = (i >> 2) & 7;
    let b = i & 3;
    [(r as u32 * 255 / 7) as u8, (g as u32 * 255 / 7) as u8, (b as u32 * 255 / 3) as u8]
}

/// Scale according to the quality settings. Colours are reduced later, per
/// part, while compressing (see [`encode_part`]): which parts are photos is
/// decided from the real colours.
pub fn prepare(raw: &Frame, q: &Quality) -> Frame {
    let (w, h) = q.scaled_size(raw.width, raw.height);
    scale(raw, w, h)
}

/// Tiles of `cur` that differ from `prev` (all tiles if `prev` is `None` or a
/// different size), merged into horizontal strips.
pub fn changed_rects(prev: Option<&Frame>, cur: &Frame) -> Vec<Rect> {
    let cols = cur.width.div_ceil(TILE);
    let rows = cur.height.div_ceil(TILE);
    let same_size = prev.is_some_and(|p| p.width == cur.width && p.height == cur.height);
    let mut out = Vec::new();
    for ty in 0..rows {
        let y = ty * TILE;
        let th = TILE.min(cur.height - y);
        let mut run: Option<Rect> = None;
        for tx in 0..cols {
            let x = tx * TILE;
            let tw = TILE.min(cur.width - x);
            let dirty = !same_size || {
                let p = prev.unwrap();
                (y..y + th).any(|row| {
                    let i = ((row * cur.width + x) * 4) as usize;
                    p.rgba[i..i + tw as usize * 4] != cur.rgba[i..i + tw as usize * 4]
                })
            };
            match (&mut run, dirty) {
                (Some(r), true) if r.w + tw <= MAX_STRIP => r.w += tw,
                (Some(r), true) => {
                    out.push(*r);
                    run = Some(Rect { x, y, w: tw, h: th });
                }
                (None, true) => run = Some(Rect { x, y, w: tw, h: th }),
                (Some(r), false) => {
                    out.push(*r);
                    run = None;
                }
                (None, false) => {}
            }
        }
        if let Some(r) = run {
            out.push(r);
        }
    }
    out
}

/// JPEG quality used to sharpen parts that Auto first sent as rough JPEG.
pub const REFINE_JPEG: u8 = 92;

/// One compressed part of a picture.
struct Encoded {
    format: TileFormat,
    data: Vec<u8>,
    /// Rough JPEG that Auto should sharpen once it stops changing.
    rough: bool,
}

/// Compress one part of a prepared picture.
pub fn encode_rect(f: &Frame, r: Rect, q: &Quality) -> Result<(TileFormat, Vec<u8>)> {
    let e = encode_part(f, r, q)?;
    Ok((e.format, e.data))
}

fn encode_part(f: &Frame, r: Rect, q: &Quality) -> Result<Encoded> {
    let q = q.clamped();
    let mut tile = f.crop(r);
    let jpeg_part = |tile: &Frame, rough: bool| -> Result<Encoded> {
        // 16-bit colour only adds banding to a JPEG (bigger, not smaller): JPEG
        // parts use the real colours, or one grey channel for greyscale.
        Ok(Encoded { format: TileFormat::Jpeg, data: jpeg(tile, q.jpeg_quality, q.color)?, rough })
    };
    let lossless = |mut tile: Frame| -> Result<Encoded> {
        quantize(&mut tile, q.color);
        // A palette of the colours really used compresses better than a fixed one.
        let data = match palette_of(&tile) {
            Some((palette, indexed)) => png_indexed(&tile, &palette, &indexed)?,
            None => png_lossless(&tile, q.color)?,
        };
        Ok(Encoded { format: TileFormat::Png, data, rough: false })
    };
    match q.codec {
        Codec::Jpeg if q.color == ColorDepth::Low => {
            quantize(&mut tile, ColorDepth::Low);
            jpeg_part(&tile, false)
        }
        Codec::Jpeg => jpeg_part(&tile, false),
        Codec::Png => lossless(tile),
        Codec::Auto => {
            // Text, windows and icons rarely use more than 256 colours in one
            // strip: send them lossless (palette PNG is small and fast). Decided
            // on the real colours: after greyscale everything would qualify,
            // photos too, and lossless photos are several times bigger.
            if let Some((palette, indexed)) = palette_of(&tile) {
                if q.color == ColorDepth::Full {
                    return Ok(Encoded { format: TileFormat::Png, data: png_indexed(&tile, &palette, &indexed)?, rough: false });
                }
                return lossless(tile);
            }
            match q.color {
                // 256 fixed colours: a lossless 8-bit picture is about as small as JPEG.
                ColorDepth::Low => lossless(tile),
                _ => jpeg_part(&tile, q.jpeg_quality < REFINE_JPEG),
            }
        }
    }
}

/// A small JPEG of a whole monitor: longest side at most `max` pixels.
pub fn thumbnail_jpeg(f: &Frame, max: u32, quality: u8) -> Result<(u32, u32, Vec<u8>)> {
    let k = (max as f32 / f.width.max(f.height).max(1) as f32).min(1.0);
    let (w, h) = (((f.width as f32 * k).round() as u32).max(1), ((f.height as f32 * k).round() as u32).max(1));
    let small = if (w, h) == (f.width, f.height) { f.clone() } else { scale(f, w, h) };
    Ok((w, h, jpeg(&small, quality, ColorDepth::Full)?))
}

fn jpeg(tile: &Frame, quality: u8, color: ColorDepth) -> Result<Vec<u8>> {
    use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
    let mut buf = Vec::with_capacity(tile.rgba.len() / 8);
    let mut enc = Encoder::new(&mut buf, quality);
    // Full colour resolution for high qualities keeps coloured text crisp.
    enc.set_sampling_factor(if quality >= 90 { SamplingFactor::R_4_4_4 } else { SamplingFactor::R_4_2_0 });
    let (w, h) = (tile.width as u16, tile.height as u16);
    if color == ColorDepth::Gray {
        let gray: Vec<u8> = tile.rgba.chunks_exact(4).map(|p| luma(p[0], p[1], p[2])).collect();
        enc.encode(&gray, w, h, ColorType::Luma)?;
    } else {
        enc.encode(&tile.rgba, w, h, ColorType::Rgba)?;
    }
    Ok(buf)
}

fn png_lossless(tile: &Frame, color: ColorDepth) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut enc = png::Encoder::new(&mut buf, tile.width, tile.height);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    let data: Vec<u8> = match color {
        ColorDepth::Gray => {
            enc.set_color(png::ColorType::Grayscale);
            tile.rgba.chunks_exact(4).map(|p| p[0]).collect()
        }
        ColorDepth::Low => {
            enc.set_color(png::ColorType::Indexed);
            let palette: Vec<u8> = (0..=255u8).flat_map(color_332).collect();
            enc.set_palette(palette);
            tile.rgba.chunks_exact(4).map(|p| index_332(p[0], p[1], p[2])).collect()
        }
        ColorDepth::Full | ColorDepth::High => {
            // Few colours: a palette PNG is much smaller and quicker.
            if let Some((palette, indexed)) = palette_of(tile) {
                return png_indexed(tile, &palette, &indexed);
            }
            enc.set_color(png::ColorType::Rgb);
            tile.rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect()
        }
    };
    let mut w = enc.write_header()?;
    w.write_image_data(&data)?;
    w.finish()?;
    Ok(buf)
}

fn png_indexed(tile: &Frame, palette: &[u8], indexed: &[u8]) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut enc = png::Encoder::new(&mut buf, tile.width, tile.height);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    enc.set_color(png::ColorType::Indexed);
    enc.set_palette(palette.to_vec());
    let mut w = enc.write_header()?;
    w.write_image_data(indexed)?;
    w.finish()?;
    Ok(buf)
}

/// The palette (RGB triples) and one index per pixel, if the picture has at
/// most 256 colours. Gives up as soon as the 257th colour appears.
pub fn palette_of(tile: &Frame) -> Option<(Vec<u8>, Vec<u8>)> {
    const SLOTS: usize = 1024; // open addressing, at most 1/4 full
    const EMPTY: u32 = u32::MAX; // alpha is always 255, so RGB keys never hit this
    let mut keys = [EMPTY; SLOTS];
    let mut vals = [0u8; SLOTS];
    let mut palette = Vec::with_capacity(64 * 3);
    let mut indexed = Vec::with_capacity(tile.rgba.len() / 4);
    let mut last_key = EMPTY;
    let mut last_val = 0u8;
    for p in tile.rgba.chunks_exact(4) {
        let key = u32::from_le_bytes([p[0], p[1], p[2], 0]);
        if key != last_key {
            let mut slot = (key.wrapping_mul(0x9E37_79B1) >> 22) as usize; // top 10 bits
            loop {
                if keys[slot] == key {
                    last_val = vals[slot];
                    break;
                }
                if keys[slot] == EMPTY {
                    let n = palette.len() / 3;
                    if n == 256 {
                        return None;
                    }
                    keys[slot] = key;
                    vals[slot] = n as u8;
                    palette.extend_from_slice(&p[..3]);
                    last_val = n as u8;
                    break;
                }
                slot = (slot + 1) % SLOTS;
            }
            last_key = key;
        }
        indexed.push(last_val);
    }
    Some((palette, indexed))
}

/// Threads used to compress one picture: half the processors (at most 4),
/// so the remote computer stays responsive for the person using it.
fn encode_threads() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| std::thread::available_parallelism().map(|n| n.get() / 2).unwrap_or(1).clamp(1, 4))
}

/// Compress many parts, in parallel when there is enough work.
fn encode_parts(f: &Frame, rects: &[Rect], q: &Quality) -> Vec<Result<Encoded>> {
    let pixels: u64 = rects.iter().map(|r| r.w as u64 * r.h as u64).sum();
    let threads = encode_threads().min(rects.len());
    if threads <= 1 || pixels < 200_000 {
        return rects.iter().map(|r| encode_part(f, *r, q)).collect();
    }
    let mut out: Vec<Option<Result<Encoded>>> = (0..rects.len()).map(|_| None).collect();
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                s.spawn(move || {
                    rects.iter().enumerate().skip(t).step_by(threads).map(|(i, r)| (i, encode_part(f, *r, q))).collect::<Vec<_>>()
                })
            })
            .collect();
        for h in handles {
            match h.join() {
                Ok(done) => done.into_iter().for_each(|(i, e)| out[i] = Some(e)),
                Err(_) => {} // left as None → reported below
            }
        }
    });
    out.into_iter().map(|e| e.unwrap_or_else(|| Err(anyhow::anyhow!("encoder thread failed")))).collect()
}

/// Decode a tile back to RGBA (desktop viewer side).
pub fn decode_tile(format: TileFormat, data: &[u8]) -> Result<Frame> {
    let fmt = match format {
        TileFormat::Jpeg => image::ImageFormat::Jpeg,
        TileFormat::Png => image::ImageFormat::Png,
    };
    let img = image::load_from_memory_with_format(data, fmt).context("decode tile")?.to_rgba8();
    Ok(Frame { width: img.width(), height: img.height(), rgba: img.into_raw() })
}

/// Remembers the last picture sent for one monitor and produces the binary
/// messages for the next one.
#[derive(Default)]
pub struct MonitorEncoder {
    prev: Option<Frame>,
    /// Per 64×64 tile (row by row): sent as rough JPEG, worth sharpening.
    rough: Vec<bool>,
}

impl MonitorEncoder {
    /// Forget the last picture so the next one is sent completely.
    pub fn reset(&mut self) {
        self.prev = None;
        self.rough.clear();
    }

    /// Messages for this picture (empty if nothing changed). The last message
    /// carries [`FLAG_LAST`].
    pub fn encode(&mut self, frame: Frame, q: &Quality, monitor: u8, seq: u32) -> Result<Vec<Vec<u8>>> {
        let rects = changed_rects(self.prev.as_ref(), &frame);
        let cols = frame.width.div_ceil(TILE) as usize;
        let tiles = cols * frame.height.div_ceil(TILE) as usize;
        if self.rough.len() != tiles || self.prev.as_ref().is_none_or(|p| p.width != frame.width || p.height != frame.height) {
            self.rough = vec![false; tiles];
        }
        let out = self.messages(&frame, &rects, q, monitor, seq)?;
        self.prev = Some(frame);
        Ok(out)
    }

    /// Parts still showing a rough JPEG (Auto only).
    pub fn needs_refine(&self) -> bool {
        self.rough.iter().any(|&r| r)
    }

    /// Send the rough JPEG parts again in high quality. Call when the screen
    /// has been still for a moment.
    pub fn refine(&mut self, q: &Quality, monitor: u8, seq: u32) -> Result<Vec<Vec<u8>>> {
        let Some(frame) = self.prev.take() else { return Ok(Vec::new()) };
        let cols = frame.width.div_ceil(TILE) as usize;
        let mut rects = Vec::new();
        for (ty, row) in self.rough.chunks(cols).enumerate() {
            let y = ty as u32 * TILE;
            let h = TILE.min(frame.height - y);
            let mut run: Option<Rect> = None;
            for (tx, &rough) in row.iter().enumerate() {
                let x = tx as u32 * TILE;
                let w = TILE.min(frame.width - x);
                match (&mut run, rough) {
                    (Some(r), true) if r.w + w <= MAX_STRIP => r.w += w,
                    (_, true) => {
                        rects.extend(run.take());
                        run = Some(Rect { x, y, w, h });
                    }
                    (_, false) => rects.extend(run.take()),
                }
            }
            rects.extend(run);
        }
        let sharp = Quality { codec: Codec::Jpeg, jpeg_quality: REFINE_JPEG, ..q.clamped() };
        let out = self.messages(&frame, &rects, &sharp, monitor, seq);
        self.prev = Some(frame);
        out
    }

    fn messages(&mut self, frame: &Frame, rects: &[Rect], q: &Quality, monitor: u8, seq: u32) -> Result<Vec<Vec<u8>>> {
        let cols = frame.width.div_ceil(TILE) as usize;
        let n = rects.len();
        let mut out = Vec::with_capacity(n);
        for (i, (r, e)) in rects.iter().zip(encode_parts(frame, rects, q)).enumerate() {
            let e = e?;
            for ty in r.y / TILE..(r.y + r.h).div_ceil(TILE) {
                for tx in r.x / TILE..(r.x + r.w).div_ceil(TILE) {
                    if let Some(t) = self.rough.get_mut(ty as usize * cols + tx as usize) {
                        *t = e.rough;
                    }
                }
            }
            let header = TileHeader {
                monitor,
                format: e.format,
                flags: if i + 1 == n { FLAG_LAST } else { 0 },
                frame_w: frame.width as u16,
                frame_h: frame.height as u16,
                x: r.x as u16,
                y: r.y as u16,
                w: r.w as u16,
                h: r.h as u16,
                seq,
            };
            out.push(header.encode(&e.data));
        }
        Ok(out)
    }
}

/// The viewer's copy of one remote monitor, updated tile by tile.
#[derive(Default)]
pub struct Canvas {
    pub frame: Option<Frame>,
    /// Bumped whenever pixels change, so the UI knows to re-upload the texture.
    pub generation: u64,
    /// Parts changed since the UI last took them (only those are re-uploaded).
    pub dirty: Vec<Rect>,
}

impl Canvas {
    pub fn apply(&mut self, h: &TileHeader, data: &[u8]) -> Result<()> {
        let tile = decode_tile(h.format, data)?;
        self.apply_tile(h, &tile);
        Ok(())
    }

    /// Put an already decoded tile in place.
    pub fn apply_tile(&mut self, h: &TileHeader, tile: &Frame) {
        let (fw, fh) = (h.frame_w as u32, h.frame_h as u32);
        if self.frame.as_ref().is_none_or(|f| f.width != fw || f.height != fh) {
            self.frame = Some(Frame::filled(fw, fh, [16, 18, 24, 255]));
            self.dirty.clear();
            self.dirty.push(Rect { x: 0, y: 0, w: fw, h: fh });
        }
        let f = self.frame.as_mut().unwrap();
        f.blit(tile, h.x as u32, h.y as u32);
        let (x, y) = (h.x as u32, h.y as u32);
        if x < fw && y < fh {
            self.dirty.push(Rect { x, y, w: tile.width.min(fw - x), h: tile.height.min(fh - y) });
        }
        self.generation += 1;
    }

    /// Changed parts since the last call (a single bounding box when there are many).
    pub fn take_dirty(&mut self) -> Vec<Rect> {
        let mut d = std::mem::take(&mut self.dirty);
        if d.len() > 48 {
            let x0 = d.iter().map(|r| r.x).min().unwrap_or(0);
            let y0 = d.iter().map(|r| r.y).min().unwrap_or(0);
            let x1 = d.iter().map(|r| r.x + r.w).max().unwrap_or(0);
            let y1 = d.iter().map(|r| r.y + r.h).max().unwrap_or(0);
            d = vec![Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }];
        }
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quality::Preset;

    fn gradient(w: u32, h: u32) -> Frame {
        let mut f = Frame::new(w, h);
        for y in 0..h {
            for x in 0..w {
                f.set_pixel(x, y, [(x * 255 / w) as u8, (y * 255 / h) as u8, 128, 255]);
            }
        }
        f
    }

    #[test]
    fn bgra_conversion_swaps_channels_and_honours_stride() {
        // 2x1 picture with 4 bytes of row padding.
        let bgra = [1, 2, 3, 0, 4, 5, 6, 0, 9, 9, 9, 9];
        let f = Frame::from_bgra(2, 1, &bgra, 12);
        assert_eq!(f.pixel(0, 0), [3, 2, 1, 255]);
        assert_eq!(f.pixel(1, 0), [6, 5, 4, 255]);
    }

    #[test]
    fn scaling_averages() {
        let mut f = Frame::filled(4, 2, [0, 0, 0, 255]);
        f.set_pixel(0, 0, [200, 100, 40, 255]);
        let s = scale(&f, 2, 1);
        assert_eq!((s.width, s.height), (2, 1));
        assert_eq!(s.pixel(0, 0), [50, 25, 10, 255]);
        assert_eq!(s.pixel(1, 0), [0, 0, 0, 255]);
        // Same size is a plain copy.
        assert_eq!(scale(&f, 4, 2), f);
    }

    #[test]
    fn scaling_odd_sizes_never_panics() {
        let f = gradient(1366, 768);
        for (w, h) in [(1, 1), (683, 384), (1025, 577), (1366, 768)] {
            let s = scale(&f, w, h);
            assert_eq!((s.width, s.height), (w, h));
        }
    }

    #[test]
    fn quantize_levels() {
        let mut f = Frame::filled(1, 1, [255, 255, 255, 255]);
        quantize(&mut f, ColorDepth::Low);
        assert_eq!(f.pixel(0, 0), [255, 255, 255, 255], "white stays white");
        let mut f = Frame::filled(1, 1, [10, 200, 30, 255]);
        quantize(&mut f, ColorDepth::Gray);
        let p = f.pixel(0, 0);
        assert!(p[0] == p[1] && p[1] == p[2]);
        let mut f = Frame::filled(1, 1, [0x87, 0x45, 0xFF, 255]);
        quantize(&mut f, ColorDepth::High);
        assert_eq!(f.pixel(0, 0)[0] & 0xF8, 0x80);
        assert_eq!(f.pixel(0, 0)[2], 0xFF);
        for i in 0..=255u8 {
            let c = color_332(i);
            assert_eq!(index_332(c[0], c[1], c[2]), i, "palette index {i} round-trips");
        }
    }

    #[test]
    fn first_frame_is_all_dirty_then_only_changes() {
        let f = gradient(200, 130);
        let all = changed_rects(None, &f);
        let area: u32 = all.iter().map(|r| r.w * r.h).sum();
        assert_eq!(area, 200 * 130, "covers the whole picture exactly");
        assert!(changed_rects(Some(&f), &f).is_empty());

        let mut g = f.clone();
        g.set_pixel(130, 70, [1, 2, 3, 255]);
        let rects = changed_rects(Some(&f), &g);
        assert_eq!(rects, vec![Rect { x: 128, y: 64, w: 64, h: 64 }]);
    }

    #[test]
    fn neighbouring_tiles_merge_into_strips() {
        let f = Frame::filled(1024, 64, [0, 0, 0, 255]);
        let mut g = f.clone();
        g.fill_rect(Rect { x: 0, y: 0, w: 1024, h: 1 }, [255, 0, 0, 255]);
        let rects = changed_rects(Some(&f), &g);
        assert_eq!(rects, vec![Rect { x: 0, y: 0, w: 512, h: 64 }, Rect { x: 512, y: 0, w: 512, h: 64 }]);
    }

    #[test]
    fn size_change_resends_everything() {
        let f = gradient(128, 128);
        let g = gradient(64, 64);
        assert_eq!(changed_rects(Some(&f), &g), vec![Rect { x: 0, y: 0, w: 64, h: 64 }]);
    }

    #[test]
    fn png_is_lossless_for_every_depth() {
        for depth in ColorDepth::ALL {
            let q = Quality { codec: Codec::Png, color: depth, ..Quality::default() };
            let f = prepare(&gradient(100, 70), &q);
            let r = Rect { x: 0, y: 0, w: f.width, h: f.height };
            let (fmt, data) = encode_rect(&f, r, &q).unwrap();
            assert_eq!(fmt, TileFormat::Png);
            let back = decode_tile(fmt, &data).unwrap();
            let mut want = f.clone();
            quantize(&mut want, depth);
            assert_eq!(back, want, "depth {depth:?}");
        }
    }

    #[test]
    fn auto_sends_photos_as_jpeg_whatever_the_colours() {
        // More than 256 real colours = photo, also in greyscale (where every
        // picture has at most 256 shades) and 16-bit colour.
        let photo = gradient(200, 120);
        let r = Rect { x: 0, y: 0, w: 200, h: 120 };
        for depth in [ColorDepth::Full, ColorDepth::High, ColorDepth::Gray] {
            let q = Quality { codec: Codec::Auto, color: depth, ..Quality::default() };
            assert_eq!(encode_rect(&photo, r, &q).unwrap().0, TileFormat::Jpeg, "{depth:?}");
        }
        // Text-like part in greyscale: lossless and really grey.
        let mut text = Frame::filled(64, 64, [250, 250, 250, 255]);
        text.fill_rect(Rect { x: 10, y: 10, w: 20, h: 4 }, [200, 30, 30, 255]);
        let q = Quality { codec: Codec::Auto, color: ColorDepth::Gray, ..Quality::default() };
        let (fmt, data) = encode_rect(&text, Rect { x: 0, y: 0, w: 64, h: 64 }, &q).unwrap();
        assert_eq!(fmt, TileFormat::Png);
        let p = decode_tile(fmt, &data).unwrap().pixel(15, 11);
        assert!(p[0] == p[1] && p[1] == p[2], "grey: {p:?}");
    }

    #[test]
    fn fewer_colours_means_smaller_png() {
        // Photo-like noisy picture (smooth gradients compress too well to compare).
        let mut raw = Frame::new(256, 256);
        let mut seed = 12345u32;
        for y in 0..256 {
            for x in 0..256 {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                let n = (seed >> 16) as u8;
                raw.set_pixel(x, y, [n, n.wrapping_add(x as u8), n.wrapping_mul(3), 255]);
            }
        }
        let size = |depth| {
            let q = Quality { codec: Codec::Png, color: depth, scale_pct: 100, ..Quality::default() };
            let f = prepare(&raw, &q);
            encode_rect(&f, Rect { x: 0, y: 0, w: 256, h: 256 }, &q).unwrap().1.len()
        };
        assert!(size(ColorDepth::Low) < size(ColorDepth::Full));
        assert!(size(ColorDepth::Gray) < size(ColorDepth::Full));
    }

    #[test]
    fn jpeg_is_close_and_quality_matters() {
        let raw = gradient(128, 128);
        let enc = |jq| {
            let q = Quality { codec: Codec::Jpeg, jpeg_quality: jq, color: ColorDepth::Full, scale_pct: 100, max_fps: 10 };
            encode_rect(&raw, Rect { x: 0, y: 0, w: 128, h: 128 }, &q).unwrap()
        };
        let (fmt, hi) = enc(95);
        let (_, lo) = enc(10);
        assert_eq!(fmt, TileFormat::Jpeg);
        assert!(lo.len() < hi.len());
        let back = decode_tile(fmt, &hi).unwrap();
        let p = back.pixel(64, 64);
        let o = raw.pixel(64, 64);
        for c in 0..3 {
            assert!((p[c] as i32 - o[c] as i32).abs() < 12, "channel {c}: {p:?} vs {o:?}");
        }
    }

    /// Screen-like picture: white window with dark "text", plus a noisy photo.
    fn desktop_like(w: u32, h: u32) -> Frame {
        let mut f = Frame::filled(w, h, [250, 250, 250, 255]);
        for y in 0..h {
            for x in 0..w {
                if x >= w / 2 {
                    let n = (x.wrapping_mul(7919) ^ y.wrapping_mul(104_729)).wrapping_mul(2_654_435_761) >> 24;
                    f.set_pixel(x, y, [n as u8, (x * 3) as u8, (y * 5) as u8, 255]);
                } else if (y / 16) % 2 == 0 && (x / 3 + y) % 4 == 0 {
                    f.set_pixel(x, y, [20, 20, 30, 255]);
                }
            }
        }
        f
    }

    #[test]
    fn palette_detection() {
        let f = Frame::filled(64, 64, [1, 2, 3, 255]);
        let (pal, idx) = palette_of(&f).unwrap();
        assert_eq!(pal, vec![1, 2, 3]);
        assert!(idx.iter().all(|&i| i == 0));
        let mut g = Frame::new(32, 32);
        for i in 0..1024u32 {
            g.set_pixel(i % 32, i / 32, [i as u8, (i >> 8) as u8, 0, 255]);
        }
        assert!(palette_of(&g).is_none(), "1024 colours");
        assert!(palette_of(&g.crop(Rect { x: 0, y: 0, w: 32, h: 8 })).is_some(), "exactly 256 colours");
    }

    #[test]
    fn auto_sends_text_lossless_and_photos_as_jpeg_then_sharpens_them() {
        let q = Quality::preset(Preset::Balanced);
        assert_eq!(q.codec, Codec::Auto);
        let f = desktop_like(1024, 128);
        let mut enc = MonitorEncoder::default();
        let mut canvas = Canvas::default();
        let mut formats = Vec::new();
        for m in enc.encode(f.clone(), &q, 0, 1).unwrap() {
            let (h, data) = TileHeader::decode(&m).unwrap();
            formats.push((h.x, h.format));
            canvas.apply(&h, data).unwrap();
        }
        assert!(formats.contains(&(0, TileFormat::Png)), "{formats:?}");
        assert!(formats.contains(&(512, TileFormat::Jpeg)), "{formats:?}");
        let text = Rect { x: 0, y: 0, w: 512, h: 128 };
        assert_eq!(canvas.frame.as_ref().unwrap().crop(text), f.crop(text), "text is exact");

        assert!(enc.needs_refine());
        let msgs = enc.refine(&q, 0, 2).unwrap();
        assert!(!msgs.is_empty());
        for m in &msgs {
            let (h, data) = TileHeader::decode(m).unwrap();
            assert!(h.x >= 512, "only the photo is sent again");
            assert_eq!(h.format, TileFormat::Jpeg);
            canvas.apply(&h, data).unwrap();
        }
        assert!(TileHeader::decode(msgs.last().unwrap()).unwrap().0.is_last());
        assert!(!enc.needs_refine());
        assert!(enc.refine(&q, 0, 3).unwrap().is_empty());
        assert!(enc.encode(f, &q, 0, 4).unwrap().is_empty(), "still nothing new");
    }

    #[test]
    fn parallel_encoding_keeps_order_and_last_flag() {
        let q = Quality::preset(Preset::Balanced);
        let f = desktop_like(1920, 1080);
        let mut enc = MonitorEncoder::default();
        let msgs = enc.encode(f.clone(), &q, 0, 1).unwrap();
        let mut canvas = Canvas::default();
        let mut last = (0u16, 0u16);
        for (i, m) in msgs.iter().enumerate() {
            let (h, data) = TileHeader::decode(m).unwrap();
            assert!((h.y, h.x) > last || i == 0, "row by row order");
            last = (h.y, h.x);
            assert_eq!(h.is_last(), i + 1 == msgs.len());
            canvas.apply(&h, data).unwrap();
        }
        let left = Rect { x: 0, y: 0, w: 512, h: 1080 }; // strips are 512 wide: 512..1024 holds photo too
        assert_eq!(canvas.frame.as_ref().unwrap().crop(left), f.crop(left));
    }

    #[test]
    fn canvas_reports_changed_parts() {
        let mut c = Canvas::default();
        let tile = Frame::filled(10, 10, [1, 1, 1, 255]);
        let h = TileHeader { monitor: 0, format: TileFormat::Png, flags: 0, frame_w: 100, frame_h: 50, x: 95, y: 5, w: 10, h: 10, seq: 1 };
        c.apply_tile(&h, &tile);
        let d = c.take_dirty();
        assert_eq!(d[0], Rect { x: 0, y: 0, w: 100, h: 50 }, "new picture: everything");
        assert_eq!(d[1], Rect { x: 95, y: 5, w: 5, h: 10 }, "clipped to the picture");
        c.apply_tile(&h, &tile);
        assert_eq!(c.take_dirty(), vec![Rect { x: 95, y: 5, w: 5, h: 10 }]);
        assert!(c.take_dirty().is_empty());
    }

    #[test]
    fn encoder_and_canvas_reproduce_the_screen() {
        let q = Quality { codec: Codec::Png, color: ColorDepth::Full, scale_pct: 100, ..Quality::preset(Preset::Sharp) };
        let mut enc = MonitorEncoder::default();
        let mut canvas = Canvas::default();
        let f1 = gradient(300, 200);
        let mut f2 = f1.clone();
        f2.fill_rect(Rect { x: 10, y: 150, w: 40, h: 20 }, [255, 255, 0, 255]);

        for (seq, f) in [(1u32, f1.clone()), (2, f2.clone())] {
            let msgs = enc.encode(f, &q, 1, seq).unwrap();
            assert!(!msgs.is_empty());
            for (i, m) in msgs.iter().enumerate() {
                let (h, data) = TileHeader::decode(m).unwrap();
                assert_eq!(h.monitor, 1);
                assert_eq!(h.seq, seq);
                assert_eq!(h.is_last(), i + 1 == msgs.len());
                canvas.apply(&h, data).unwrap();
            }
        }
        assert_eq!(canvas.frame.as_ref().unwrap(), &f2);
        assert!(enc.encode(f2, &q, 1, 3).unwrap().is_empty(), "no change, nothing sent");
        enc.reset();
        assert!(!enc.encode(f1, &q, 1, 4).unwrap().is_empty());
    }

    #[test]
    fn thumbnails_keep_aspect_and_never_grow() {
        let f = Frame::filled(1600, 900, [10, 120, 200, 255]);
        let (w, h, jpeg) = thumbnail_jpeg(&f, 480, 70).unwrap();
        assert_eq!((w, h), (480, 270));
        let back = decode_tile(TileFormat::Jpeg, &jpeg).unwrap();
        assert_eq!((back.width, back.height), (480, 270));
        let p = back.pixel(240, 135);
        assert!(p[0].abs_diff(10) < 8 && p[1].abs_diff(120) < 8 && p[2].abs_diff(200) < 8, "{p:?}");

        let (w, h, _) = thumbnail_jpeg(&Frame::filled(1280, 1024, [0, 0, 0, 255]), 480, 70).unwrap();
        assert_eq!((w, h), (480, 384), "portrait-ish screens fit by the longer side");
        let (w, h, _) = thumbnail_jpeg(&Frame::filled(300, 200, [0, 0, 0, 255]), 480, 70).unwrap();
        assert_eq!((w, h), (300, 200), "small screens are not enlarged");
    }
}

#[cfg(test)]
mod bench_colors {
    use super::*;
    use crate::quality::{ColorDepth, Quality};
    #[test]
    #[ignore]
    fn bench_color_depths() {
        let paths = std::env::var("BENCH_PNGS").unwrap();
        for p in paths.split(',') {
            let img = image::open(p).unwrap().to_rgba8();
            let raw = Frame { width: img.width(), height: img.height(), rgba: img.into_raw() };
            for depth in ColorDepth::ALL {
                let q = Quality { color: depth, ..Quality::default() };
                let t = std::time::Instant::now();
                let mut bytes = 0;
                for _ in 0..5 {
                    let f = raw.clone();
                    let mut enc = MonitorEncoder::default();
                    bytes = enc.encode(f, &q, 0, 1).unwrap().iter().map(|m| m.len()).sum::<usize>();
                }
                eprintln!("{p} {}x{} {depth:?}: {:.1} ms, {} KB", raw.width, raw.height, t.elapsed().as_secs_f64() * 200.0, bytes / 1024);
            }
        }
    }
}
