//! The app icon (tray, window, taskbar), drawn in code so every size is sharp.
//!
//! It fills the whole square like other tray icons: a rounded tile in retro
//! purple with a big white monitor on it. The monitor's dark screen shows a
//! white mouse pointer: a computer you see and control from far away.

type Rgb = (u8, u8, u8);

/// The tile: retro purple, like the menus of old computer sci-fi games.
pub const PURPLE: Rgb = (0x6e, 0x2f, 0xc0);
/// The monitor frame and stand.
pub const FRAME: Rgb = (0xff, 0xff, 0xff);
/// The screen: a very dark purple, so the white pointer stands out at 16 px.
pub const SCREEN: Rgb = (0x22, 0x0e, 0x40);
/// The mouse pointer on the screen.
pub const POINTER: Rgb = (0xff, 0xff, 0xff);

/// Supersampling per axis (antialiasing).
const SS: u32 = 4;

/// The icon as straight (not premultiplied) RGBA, row by row.
pub fn rgba(size: u32) -> Vec<u8> {
    let n = f64::from(size);
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (mut a, mut r, mut g, mut b) = (0.0, 0.0, 0.0, 0.0);
            for sy in 0..SS {
                for sx in 0..SS {
                    let u = (f64::from(x) + (f64::from(sx) + 0.5) / f64::from(SS)) / n;
                    let v = (f64::from(y) + (f64::from(sy) + 0.5) / f64::from(SS)) / n;
                    if let Some((cr, cg, cb)) = sample(u, v) {
                        a += 1.0;
                        r += f64::from(cr);
                        g += f64::from(cg);
                        b += f64::from(cb);
                    }
                }
            }
            if a > 0.0 {
                let samples = f64::from(SS * SS);
                out.extend_from_slice(&[
                    (r / a).round() as u8,
                    (g / a).round() as u8,
                    (b / a).round() as u8,
                    (a / samples * 255.0).round() as u8,
                ]);
            } else {
                out.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    out
}

/// ARGB32 in network byte order (A, R, G, B), what StatusNotifierItem wants.
pub fn argb(size: u32) -> Vec<u8> {
    let mut px = rgba(size);
    for p in px.chunks_mut(4) {
        p.rotate_right(1);
    }
    px
}

/// Colour at a point (0..1 on both axes); `None` outside the tile.
fn sample(u: f64, v: f64) -> Option<Rgb> {
    if !in_round_rect(u, v, (0.0, 0.0, 1.0, 1.0), 0.22) {
        return None;
    }
    // Screen inside the monitor frame; the pointer on the screen.
    if in_round_rect(u, v, SCREEN_RECT, 0.035) {
        return Some(if in_pointer(u, v) { POINTER } else { SCREEN });
    }
    let frame = in_round_rect(u, v, MONITOR_RECT, 0.08);
    let neck = (0.41..=0.59).contains(&u) && (0.66..=0.79).contains(&v);
    let base = in_round_rect(u, v, (0.25, 0.77, 0.75, 0.87), 0.05);
    Some(if frame || neck || base { FRAME } else { PURPLE })
}

/// (left, top, right, bottom) of the monitor and its screen.
const MONITOR_RECT: (f64, f64, f64, f64) = (0.10, 0.14, 0.90, 0.68);
const SCREEN_RECT: (f64, f64, f64, f64) = (0.19, 0.23, 0.81, 0.59);

fn in_round_rect(u: f64, v: f64, (l, t, r, b): (f64, f64, f64, f64), radius: f64) -> bool {
    let (cx, cy) = ((l + r) / 2.0, (t + b) / 2.0);
    let qx = (u - cx).abs() - ((r - l) / 2.0 - radius);
    let qy = (v - cy).abs() - ((b - t) / 2.0 - radius);
    qx.max(0.0).hypot(qy.max(0.0)) <= radius && qx <= radius && qy <= radius
}

/// The classic arrow pointer, tip at the top left, in screen-relative units.
const ARROW: [(f64, f64); 7] = [(0.0, 0.0), (0.0, 0.80), (0.19, 0.62), (0.31, 0.90), (0.44, 0.84), (0.32, 0.57), (0.57, 0.57)];
/// Where the tip is and how tall the arrow is.
const ARROW_TIP: (f64, f64) = (0.40, 0.265);
const ARROW_SIZE: f64 = 0.35;

fn in_pointer(u: f64, v: f64) -> bool {
    let (x, y) = ((u - ARROW_TIP.0) / ARROW_SIZE, (v - ARROW_TIP.1) / ARROW_SIZE);
    // Even-odd ray casting.
    let mut inside = false;
    let mut j = ARROW.len() - 1;
    for i in 0..ARROW.len() {
        let ((xi, yi), (xj, yj)) = (ARROW[i], ARROW[j]);
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(img: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * size + x) * 4) as usize;
        img[i..i + 4].try_into().unwrap()
    }

    fn opaque(c: Rgb) -> [u8; 4] {
        [c.0, c.1, c.2, 255]
    }

    #[test]
    fn has_the_right_size() {
        for size in [16, 22, 32, 48, 256] {
            assert_eq!(rgba(size).len(), (size * size * 4) as usize);
            assert_eq!(argb(size).len(), (size * size * 4) as usize);
        }
    }

    #[test]
    fn fills_the_square_except_rounded_corners() {
        let img = rgba(32);
        assert_eq!(pixel(&img, 32, 0, 0)[3], 0, "corner cut off");
        for (x, y) in [(16, 0), (0, 16), (31, 16), (16, 31)] {
            assert_eq!(pixel(&img, 32, x, y), opaque(PURPLE), "edge ({x},{y}) is purple");
        }
        assert!(img.chunks(4).filter(|p| p[3] == 255).count() > 32 * 32 * 9 / 10);
    }

    #[test]
    fn white_monitor_dark_screen_white_pointer() {
        let img = rgba(48);
        assert_eq!(pixel(&img, 48, 7, 20), opaque(FRAME), "left side of the frame");
        assert_eq!(pixel(&img, 48, 24, 35), opaque(FRAME), "stand");
        assert_eq!(pixel(&img, 48, 34, 26), opaque(SCREEN), "screen right of the pointer");
        assert_eq!(pixel(&img, 48, 20, 18), opaque(POINTER), "pointer");
    }

    #[test]
    fn argb_is_rgba_with_alpha_first() {
        let (a, b) = (rgba(22), argb(22));
        for (p, q) in a.chunks(4).zip(b.chunks(4)) {
            assert_eq!([p[3], p[0], p[1], p[2]], q);
        }
    }

    #[test]
    fn edges_are_antialiased() {
        assert!(rgba(32).chunks(4).any(|p| p[3] > 0 && p[3] < 255));
    }
}
