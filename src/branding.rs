use eframe::egui::{Color32, ColorImage, IconData};

pub fn logo_image(size: usize) -> ColorImage {
    let mut pixels = vec![Color32::TRANSPARENT; size * size];
    let radius = size as f32 * 0.48;
    let border = size as f32 * 0.035;
    let center = (size as f32 - 1.0) * 0.5;

    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let distance = (dx * dx + dy * dy).sqrt();
            let index = y * size + x;

            if distance > radius {
                pixels[index] = Color32::TRANSPARENT;
                continue;
            }

            if distance >= radius - border {
                pixels[index] = Color32::WHITE;
                continue;
            }

            let y_ratio = y as f32 / size as f32;
            pixels[index] = if y_ratio < 0.305 {
                Color32::from_rgb(30, 30, 34)
            } else if y_ratio < 0.695 {
                Color32::from_rgb(245, 245, 245)
            } else {
                Color32::from_rgb(247, 173, 26)
            };
        }
    }

    ColorImage {
        size: [size, size],
        pixels,
    }
}

pub fn icon_data(size: usize) -> IconData {
    let image = logo_image(size);
    let mut rgba = Vec::with_capacity(size * size * 4);
    for pixel in image.pixels {
        rgba.extend_from_slice(&pixel.to_array());
    }

    IconData {
        rgba,
        width: size as u32,
        height: size as u32,
    }
}
