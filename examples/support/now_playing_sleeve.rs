use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

/// Make small, colorful record sleeves in memory. No file, font, or service lookup.
pub fn sleeve(index: u32) -> Image {
    let palettes = [
        ([40, 65, 133], [250, 179, 88]),
        ([27, 98, 82], [214, 153, 183]),
        ([102, 38, 75], [235, 119, 100]),
    ];
    let (base, ink) = palettes[index as usize % palettes.len()];
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(128, 128);
    for (i, p) in buffer.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = ((i % 128) as i32, (i / 128) as i32);
        let (dx, dy) = (x - 57, y - 61);
        let r2 = dx * dx + dy * dy;
        let [r, g, b] = if r2 < 1_500 && (r2 / 94) % 3 != 1 || (x + 2 * y) % 43 < 5 {
            ink
        } else {
            base
        };
        *p = Rgba8Pixel { r, g, b, a: 255 };
    }
    Image::from_rgba8(buffer)
}
