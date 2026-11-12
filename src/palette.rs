//! "Colour optimised": the host learns which colours the remote screen really
//! uses and sends parts of it with a palette of at most 256 of them.
//!
//! * [`PaletteLearner`] samples the pictures the host captures, counts their
//!   colours and builds a [`Palette`]: the most common colours exactly, the
//!   rest grouped by likeness (median cut). It builds again every few seconds
//!   on a background thread and only switches when the new palette fits the
//!   screen clearly better, so a new colour on screen changes nothing at once.
//! * [`Palette::map`] turns one part of a picture into palette indices and
//!   tells how many pixels are far from every palette colour. The encoder
//!   uses the palette only where that share is small ("if effective"):
//!   photos and video keep their normal path.
//!
//! Viewers need nothing new: such a part is an ordinary palette PNG.

use crate::codec::Frame;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Most colours in a palette (one byte per pixel).
pub const MAX_COLORS: usize = 256;
/// A pixel further than this from its palette colour counts as "far"
/// (squared distance over red, green, blue: 12 levels on each).
pub const FAR: u32 = 3 * 12 * 12;
/// A part with more far pixels than this share is not sent with the palette.
pub const MAX_FAR_SHARE: f32 = 0.03;

fn key(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

fn rgb(k: u32) -> [u8; 3] {
    [(k >> 16) as u8, (k >> 8) as u8, k as u8]
}

fn dist(a: [u8; 3], b: [u8; 3]) -> u32 {
    let d = |x: u8, y: u8| (x as i32 - y as i32).pow(2) as u32;
    d(a[0], b[0]) + d(a[1], b[1]) + d(a[2], b[2])
}

/// Up to 256 colours and a quick way from any colour to the nearest of them.
pub struct Palette {
    colors: Vec<[u8; 3]>,
    /// Palette colours map to themselves, whatever shares their cell below.
    exact: HashMap<u32, u8>,
    /// 64×64×64 cells of the colour cube -> nearest palette index + 1
    /// (0: not worked out yet). Filled as colours turn up.
    cells: Box<[AtomicU16]>,
}

impl std::fmt::Debug for Palette {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Palette({} colours)", self.colors.len())
    }
}

/// One part of a picture in palette form.
pub struct Mapped {
    /// RGB triples of the colours this part uses.
    pub palette: Vec<u8>,
    /// One index into `palette` per pixel.
    pub indexed: Vec<u8>,
    /// Share of pixels far from their palette colour (0..=1).
    pub far_share: f32,
}

impl Palette {
    pub fn new(mut colors: Vec<[u8; 3]>) -> Palette {
        colors.truncate(MAX_COLORS);
        if colors.is_empty() {
            colors.push([0, 0, 0]);
        }
        let exact = colors.iter().enumerate().map(|(i, c)| (key(c[0], c[1], c[2]), i as u8)).collect();
        let cells = (0..64 * 64 * 64).map(|_| AtomicU16::new(0)).collect();
        Palette { colors, exact, cells }
    }

    pub fn colors(&self) -> &[[u8; 3]] {
        &self.colors
    }

    fn scan(&self, c: [u8; 3]) -> u8 {
        let mut best = (u32::MAX, 0);
        for (i, p) in self.colors.iter().enumerate() {
            let d = dist(c, *p);
            if d < best.0 {
                best = (d, i);
            }
        }
        best.1 as u8
    }

    /// Index of the palette colour nearest to this colour.
    pub fn nearest(&self, r: u8, g: u8, b: u8) -> u8 {
        if let Some(&i) = self.exact.get(&key(r, g, b)) {
            return i;
        }
        let cell = ((r as usize >> 2) << 12) | ((g as usize >> 2) << 6) | (b as usize >> 2);
        match self.cells[cell].load(Ordering::Relaxed) {
            0 => {
                // Nearest to the middle of the cell: the same for all its colours.
                let i = self.scan([(r & 0xFC) | 2, (g & 0xFC) | 2, (b & 0xFC) | 2]);
                self.cells[cell].store(i as u16 + 1, Ordering::Relaxed);
                i
            }
            i => (i - 1) as u8,
        }
    }

    /// Mean squared distance of these counted colours to the palette.
    pub fn error(&self, hist: &[(u32, u32)]) -> f64 {
        let (mut sum, mut n) = (0f64, 0f64);
        for &(k, count) in hist {
            let c = rgb(k);
            let p = self.colors[self.nearest(c[0], c[1], c[2]) as usize];
            sum += dist(c, p) as f64 * count as f64;
            n += count as f64;
        }
        if n == 0.0 { 0.0 } else { sum / n }
    }

    /// Put one part of a picture into palette form.
    pub fn map(&self, tile: &Frame) -> Mapped {
        let mut indexed = Vec::with_capacity(tile.rgba.len() / 4);
        // Global index -> index within this part (0xFFFF: not used yet).
        let mut local = [u16::MAX; MAX_COLORS];
        let mut palette = Vec::new();
        let mut far = 0usize;
        // Screens are full of runs of one colour: remember the last answer.
        let mut last: Option<([u8; 3], u8, bool)> = None;
        for px in tile.rgba.chunks_exact(4) {
            let c = [px[0], px[1], px[2]];
            let (idx, is_far) = match last {
                Some((lc, idx, is_far)) if lc == c => (idx, is_far),
                _ => {
                    let g = self.nearest(c[0], c[1], c[2]) as usize;
                    if local[g] == u16::MAX {
                        local[g] = (palette.len() / 3) as u16;
                        palette.extend_from_slice(&self.colors[g]);
                    }
                    let found = (local[g] as u8, dist(c, self.colors[g]) > FAR);
                    last = Some((c, found.0, found.1));
                    found
                }
            };
            far += is_far as usize;
            indexed.push(idx);
        }
        let far_share = if indexed.is_empty() { 0.0 } else { far as f32 / indexed.len() as f32 };
        Mapped { palette, indexed, far_share }
    }
}

/// Choose at most `max` colours for these counted colours: every colour if
/// they fit; else the common ones exactly (big flat areas, text colours stay
/// true) and the rest grouped by likeness.
pub fn build(hist: &[(u32, u32)], max: usize) -> Vec<[u8; 3]> {
    if hist.len() <= max {
        return hist.iter().map(|&(k, _)| rgb(k)).collect();
    }
    let total: u64 = hist.iter().map(|h| h.1 as u64).sum();
    let mut sorted: Vec<(u32, u32)> = hist.to_vec();
    sorted.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    // Exact: colours covering at least 0.1 % of the screen each, at most half the palette.
    let popular = sorted.iter().take(max / 2).take_while(|h| h.1 as u64 * 1000 >= total).count();
    let mut out: Vec<[u8; 3]> = sorted[..popular].iter().map(|&(k, _)| rgb(k)).collect();
    let rest: Vec<([u8; 3], u32)> = sorted[popular..].iter().map(|&(k, n)| (rgb(k), n)).collect();
    out.extend(median_cut(rest, max - popular));
    out
}

/// Split the colours into `n` groups of like colours; one average per group.
fn median_cut(colors: Vec<([u8; 3], u32)>, n: usize) -> Vec<[u8; 3]> {
    if colors.is_empty() || n == 0 {
        return Vec::new();
    }
    // (colours, widest channel, its extent)
    fn measure(b: &[([u8; 3], u32)]) -> (usize, u8) {
        let mut best = (0, 0);
        for ch in 0..3 {
            let (lo, hi) = b.iter().fold((255u8, 0u8), |(lo, hi), c| (lo.min(c.0[ch]), hi.max(c.0[ch])));
            if hi - lo >= best.1 {
                best = (ch, hi - lo);
            }
        }
        best
    }
    let mut boxes = vec![colors];
    while boxes.len() < n {
        // The box that is widest, a little in favour of the ones with many pixels.
        let pick = boxes
            .iter()
            .enumerate()
            .filter(|(_, b)| b.len() > 1)
            .map(|(i, b)| {
                let weight: u64 = b.iter().map(|c| c.1 as u64).sum();
                (i, measure(b).1 as f64 * (weight as f64).powf(0.3))
            })
            .filter(|(_, score)| *score > 0.0)
            .max_by(|a, b| a.1.total_cmp(&b.1));
        let Some((i, _)) = pick else { break };
        let mut b = boxes.swap_remove(i);
        let (ch, _) = measure(&b);
        b.sort_unstable_by_key(|c| c.0[ch]);
        // Half the pixels on each side, never an empty side.
        let half = b.iter().map(|c| c.1 as u64).sum::<u64>() / 2;
        let mut acc = 0u64;
        let mut at = b.iter().position(|c| {
            acc += c.1 as u64;
            acc > half
        });
        at = at.map(|a| a.clamp(1, b.len() - 1));
        let tail = b.split_off(at.unwrap_or(b.len() / 2).max(1));
        boxes.push(b);
        boxes.push(tail);
    }
    boxes
        .iter()
        .map(|b| {
            let n: u64 = b.iter().map(|c| c.1 as u64).sum::<u64>().max(1);
            let avg = |ch: usize| ((b.iter().map(|c| c.0[ch] as u64 * c.1 as u64).sum::<u64>() + n / 2) / n) as u8;
            [avg(0), avg(1), avg(2)]
        })
        .collect()
}

/// Watches the screen's colours and keeps a palette for them.
#[derive(Default)]
pub struct PaletteLearner {
    /// Colour -> how often it was seen lately (older sightings fade).
    counts: HashMap<u32, u32>,
    observed: Option<Instant>,
    built: Option<Instant>,
    current: Option<Arc<Palette>>,
    /// A palette being worked out in the background.
    pending: Option<mpsc::Receiver<Option<Vec<[u8; 3]>>>>,
}

impl PaletteLearner {
    /// The screen's colours are counted at most this often.
    pub const OBSERVE_EVERY: Duration = Duration::from_millis(400);
    /// A better palette is looked for this often.
    pub const REBUILD_EVERY: Duration = Duration::from_secs(4);
    /// Every this many pixels across and down is counted.
    const STRIDE: usize = 4;
    /// No more different colours than this are counted (photos, video).
    const MAX_COUNTED: usize = 150_000;
    /// A new palette replaces the old one only if its error is below this
    /// share of the old one's (and the old one is not already near perfect).
    const BETTER: f64 = 0.7;
    const GOOD_ENOUGH: f64 = 4.0;

    pub fn palette(&self) -> Option<Arc<Palette>> {
        self.current.clone()
    }

    /// Count the colours of a captured picture (cheap: a sample, not often).
    pub fn observe(&mut self, f: &Frame, now: Instant) {
        // Not often; pictures taken at the same moment (several monitors) all count.
        if self.observed.is_some_and(|t| t != now && now.duration_since(t) < Self::OBSERVE_EVERY) {
            return;
        }
        self.observed = Some(now);
        let (w, h) = (f.width as usize, f.height as usize);
        let mut last: Option<(u32, u32)> = None; // colour and how many in a row
        let add = |counts: &mut HashMap<u32, u32>, k: u32, n: u32| {
            if let Some(c) = counts.get_mut(&k) {
                *c = c.saturating_add(n);
            } else if counts.len() < Self::MAX_COUNTED {
                counts.insert(k, n);
            }
        };
        for y in (0..h).step_by(Self::STRIDE) {
            let row = &f.rgba[y * w * 4..(y + 1) * w * 4];
            for px in row.chunks_exact(4).step_by(Self::STRIDE) {
                let k = key(px[0], px[1], px[2]);
                match &mut last {
                    Some((lk, n)) if *lk == k => *n += 1,
                    _ => {
                        if let Some((lk, n)) = last.take() {
                            add(&mut self.counts, lk, n);
                        }
                        last = Some((k, 1));
                    }
                }
            }
        }
        if let Some((lk, n)) = last {
            add(&mut self.counts, lk, n);
        }
    }

    fn histogram(&self) -> Vec<(u32, u32)> {
        let mut h: Vec<(u32, u32)> = self.counts.iter().map(|(&k, &n)| (k, n)).collect();
        h.sort_unstable(); // the same palette for the same screen, whatever the hash order
        h
    }

    /// Call often. Returns true when the palette was replaced by a better
    /// one (not for the very first palette): what the viewer has was drawn
    /// with the old one, so the host sends the screen again when it is still.
    pub fn step(&mut self, now: Instant) -> bool {
        if self.counts.is_empty() {
            return false;
        }
        if self.current.is_none() {
            // The first palette at once: the first picture is already optimised.
            self.current = Some(Arc::new(Palette::new(build(&self.histogram(), MAX_COLORS))));
            self.built = Some(now);
            return false;
        }
        if let Some(rx) = &self.pending {
            return match rx.try_recv() {
                Ok(found) => {
                    self.pending = None;
                    match found {
                        Some(colors) => {
                            self.current = Some(Arc::new(Palette::new(colors)));
                            true
                        }
                        None => false,
                    }
                }
                Err(mpsc::TryRecvError::Empty) => false,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending = None;
                    false
                }
            };
        }
        if self.built.is_some_and(|t| now.duration_since(t) < Self::REBUILD_EVERY) {
            return false;
        }
        self.built = Some(now);
        let hist = self.histogram();
        // Older sightings fade, so the palette follows what is on screen now.
        self.counts.retain(|_, n| {
            *n /= 2;
            *n > 0
        });
        let old = self.current.clone().expect("checked above");
        let (tx, rx) = mpsc::channel();
        self.pending = Some(rx);
        let work = move || {
            let _ = tx.send(better_palette(&old, &hist));
        };
        if std::thread::Builder::new().name("palette".into()).spawn(work).is_err() {
            self.pending = None;
        }
        false
    }
}

/// A palette for `hist` that is clearly better than `old`, if there is one.
fn better_palette(old: &Palette, hist: &[(u32, u32)]) -> Option<Vec<[u8; 3]>> {
    let old_err = old.error(hist);
    if old_err <= PaletteLearner::GOOD_ENOUGH {
        return None;
    }
    let colors = build(hist, MAX_COLORS);
    let new = Palette::new(colors.clone());
    (new.error(hist) < old_err * PaletteLearner::BETTER).then_some(colors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_of(colors: &[[u8; 3]], w: u32, h: u32) -> Frame {
        let mut f = Frame::new(w, h);
        for (i, px) in f.rgba.chunks_exact_mut(4).enumerate() {
            let c = colors[(i / 5) % colors.len()]; // runs of 5, out of step with the sampling
            px.copy_from_slice(&[c[0], c[1], c[2], 255]);
        }
        f
    }

    #[test]
    fn few_colours_are_kept_exactly() {
        let colors = [[255, 255, 255], [0, 0, 0], [0, 120, 215], [240, 240, 240]];
        let f = frame_of(&colors, 64, 64);
        let mut l = PaletteLearner::default();
        let t = Instant::now();
        l.observe(&f, t);
        assert!(!l.step(t), "the first palette is no replacement");
        let p = l.palette().unwrap();
        assert_eq!(p.colors().len(), 4);
        let m = p.map(&f);
        assert_eq!(m.far_share, 0.0);
        assert_eq!(m.palette.len(), 4 * 3);
        // Every pixel comes back as it was.
        for (px, &i) in f.rgba.chunks_exact(4).zip(&m.indexed) {
            assert_eq!(&px[..3], &m.palette[i as usize * 3..i as usize * 3 + 3]);
        }
    }

    #[test]
    fn common_colours_stay_true_and_the_rest_is_close() {
        // A white page with blue text shades and a smooth grey ramp of 1000 colours.
        let mut hist: Vec<(u32, u32)> = vec![(key(255, 255, 255), 50_000), (key(0, 102, 204), 5_000)];
        for i in 0..1000u32 {
            let v = (i * 255 / 999) as u8;
            hist.push((key(v, v, (i % 7) as u8), 3));
        }
        let colors = build(&hist, MAX_COLORS);
        assert!(colors.len() <= MAX_COLORS && colors.len() > 200);
        assert!(colors.contains(&[255, 255, 255]) && colors.contains(&[0, 102, 204]), "common colours are exact");
        let p = Palette::new(colors);
        assert!(p.error(&hist) < 16.0, "error {}", p.error(&hist));
        let i = p.nearest(255, 255, 255) as usize;
        assert_eq!(p.colors()[i], [255, 255, 255]);
    }

    #[test]
    fn a_part_far_from_the_palette_says_so() {
        let p = Palette::new(vec![[0, 0, 0], [255, 255, 255]]);
        let text = frame_of(&[[0, 0, 0], [255, 255, 255], [250, 250, 250]], 30, 30);
        assert!(p.map(&text).far_share <= MAX_FAR_SHARE, "nearly white is close enough");
        let photo = frame_of(&[[200, 30, 30], [30, 200, 30], [255, 255, 255]], 30, 30);
        let m = p.map(&photo);
        assert!(m.far_share > 0.6, "red and green are far from black and white: {}", m.far_share);
        assert_eq!(m.palette.len() % 3, 0);
        assert!(m.indexed.iter().all(|&i| (i as usize) < m.palette.len() / 3));
    }

    #[test]
    fn the_palette_follows_the_screen_in_the_background_not_at_once() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut l = PaletteLearner::default();
        let greys: Vec<[u8; 3]> = (0..=255u8).map(|v| [v, v, v]).collect();
        l.observe(&frame_of(&greys, 256, 64), at(0));
        l.step(at(0));
        let first = l.palette().unwrap();
        assert!(first.colors().iter().all(|c| c[0] == c[1] && c[1] == c[2]));

        // The screen turns colourful. Nothing changes at once...
        let colours: Vec<[u8; 3]> = (0..=255u8).map(|v| [v, 255 - v, v / 2]).collect();
        let colourful = frame_of(&colours, 256, 64);
        l.observe(&colourful, at(500));
        assert!(!l.step(at(500)));
        assert!(Arc::ptr_eq(&first, &l.palette().unwrap()), "same palette until the next look for a better one");

        // ...but a few seconds later a better palette is worked out and taken.
        let mut replaced = false;
        let mut now = 4500;
        for _ in 0..200 {
            l.observe(&colourful, at(now));
            if l.step(at(now)) {
                replaced = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
            now += 450;
        }
        assert!(replaced, "the palette was replaced");
        let p = l.palette().unwrap();
        assert!(p.map(&colourful).far_share <= MAX_FAR_SHARE);

        // The same screen again: good enough, no more replacing.
        for _ in 0..30 {
            now += 4500;
            l.observe(&colourful, at(now));
            assert!(!l.step(at(now)), "a fitting palette stays");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn median_cut_never_loses_or_invents_groups() {
        let colors: Vec<([u8; 3], u32)> = (0..50u8).map(|i| ([i * 5, 255 - i * 5, i], 1 + i as u32)).collect();
        assert_eq!(median_cut(colors.clone(), 8).len(), 8);
        assert_eq!(median_cut(colors.clone(), 500).len(), 50, "no more groups than colours");
        assert_eq!(median_cut(vec![([1, 2, 3], 9)], 4), vec![[1, 2, 3]]);
        assert!(median_cut(Vec::new(), 4).is_empty());
    }
}
