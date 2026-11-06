//! Turning screen pictures into small tiles and back.
//!
//! Pipeline on the host for every monitor and every picture:
//! 1. scale to the chosen size,
//! 2. reduce colours to the chosen depth,
//! 3. compare with the previous picture in 64×64 tiles,
//! 4. send only changed tiles (neighbours merged into strips) as JPEG/PNG.

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

/// Scale + reduce colours according to the quality settings.
pub fn prepare(raw: &Frame, q: &Quality) -> Frame {
    let (w, h) = q.scaled_size(raw.width, raw.height);
    let mut f = scale(raw, w, h);
    quantize(&mut f, q.color);
    f
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

/// Compress one part of a prepared picture.
pub fn encode_rect(f: &Frame, r: Rect, q: &Quality) -> Result<(TileFormat, Vec<u8>)> {
    let tile = f.crop(r);
    let mut buf = Vec::new();
    match q.codec {
        Codec::Jpeg => {
            use image::ImageEncoder;
            let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, q.clamped().jpeg_quality);
            if q.color == ColorDepth::Gray {
                let gray: Vec<u8> = tile.rgba.chunks_exact(4).map(|p| p[0]).collect();
                enc.write_image(&gray, r.w, r.h, image::ExtendedColorType::L8)?;
            } else {
                let rgb: Vec<u8> = tile.rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
                enc.write_image(&rgb, r.w, r.h, image::ExtendedColorType::Rgb8)?;
            }
            Ok((TileFormat::Jpeg, buf))
        }
        Codec::Png => {
            let mut enc = png::Encoder::new(&mut buf, r.w, r.h);
            enc.set_depth(png::BitDepth::Eight);
            enc.set_compression(png::Compression::Fast);
            let data: Vec<u8> = match q.color {
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
                    enc.set_color(png::ColorType::Rgb);
                    tile.rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect()
                }
            };
            let mut w = enc.write_header()?;
            w.write_image_data(&data)?;
            w.finish()?;
            Ok((TileFormat::Png, buf))
        }
    }
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
}

impl MonitorEncoder {
    /// Forget the last picture so the next one is sent completely.
    pub fn reset(&mut self) {
        self.prev = None;
    }

    /// Messages for this picture (empty if nothing changed). The last message
    /// carries [`FLAG_LAST`].
    pub fn encode(&mut self, frame: Frame, q: &Quality, monitor: u8, seq: u32) -> Result<Vec<Vec<u8>>> {
        let rects = changed_rects(self.prev.as_ref(), &frame);
        let mut out = Vec::with_capacity(rects.len());
        let n = rects.len();
        for (i, r) in rects.into_iter().enumerate() {
            let (format, data) = encode_rect(&frame, r, q)?;
            let header = TileHeader {
                monitor,
                format,
                flags: if i + 1 == n { FLAG_LAST } else { 0 },
                frame_w: frame.width as u16,
                frame_h: frame.height as u16,
                x: r.x as u16,
                y: r.y as u16,
                w: r.w as u16,
                h: r.h as u16,
                seq,
            };
            out.push(header.encode(&data));
        }
        self.prev = Some(frame);
        Ok(out)
    }
}

/// The viewer's copy of one remote monitor, updated tile by tile.
#[derive(Default)]
pub struct Canvas {
    pub frame: Option<Frame>,
    /// Bumped whenever pixels change, so the UI knows to re-upload the texture.
    pub generation: u64,
}

impl Canvas {
    pub fn apply(&mut self, h: &TileHeader, data: &[u8]) -> Result<()> {
        let tile = decode_tile(h.format, data)?;
        let (fw, fh) = (h.frame_w as u32, h.frame_h as u32);
        if self.frame.as_ref().is_none_or(|f| f.width != fw || f.height != fh) {
            self.frame = Some(Frame::filled(fw, fh, [16, 18, 24, 255]));
        }
        let f = self.frame.as_mut().unwrap();
        f.blit(&tile, h.x as u32, h.y as u32);
        self.generation += 1;
        Ok(())
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
            assert_eq!(back, f, "depth {depth:?}");
        }
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
}
