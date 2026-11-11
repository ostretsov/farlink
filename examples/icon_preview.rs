//! Writes the app icon as PNGs: `cargo run --example icon_preview -- DIR`.

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    for size in [16u32, 22, 32, 48, 256] {
        let img = image::RgbaImage::from_raw(size, size, farlink::tray::icon::rgba(size)).unwrap();
        let path = format!("{dir}/farlink-{size}.png");
        img.save(&path).unwrap();
        println!("{path}");
    }
}
