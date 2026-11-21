use anyhow::{Result, ensure};
use cosmic_text::{
    Align, Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, SwashCache, Weight, Wrap,
};

use super::SCALE;

pub(super) struct BannerRenderer {
    fonts: FontSystem,
    glyphs: SwashCache,
}

impl BannerRenderer {
    pub(super) fn new() -> Result<Self> {
        let fonts = FontSystem::new();
        ensure!(
            fonts.db().faces().next().is_some(),
            "no fonts available for the overlay"
        );
        Ok(Self {
            fonts,
            glyphs: SwashCache::new(),
        })
    }

    pub(super) fn render(&mut self, title: &str, width: usize, height: usize) -> Vec<u8> {
        let scale = SCALE as f32;
        let pad_x = (26.0 * scale).min(width as f32 / 8.0);
        let pad_y = (12.0 * scale).min(height as f32 / 8.0);
        let wrap_width = (width as f32 - 2.0 * pad_x - 4.0).max(1.0);
        let mut text = Buffer::new_empty(Metrics::new(36.0 * scale, 44.0 * scale));
        text.set_wrap(Wrap::WordOrGlyph);
        text.set_size(Some(wrap_width), None);
        text.set_text(
            title,
            &Attrs::new()
                .family(Family::SansSerif)
                .weight(Weight::SEMIBOLD),
            Shaping::Advanced,
            Some(Align::Center),
        );
        let mut text_size = (0.0_f32, 0.0_f32);
        for size in [36.0, 32.0, 28.0, 24.0, 20.0, 16.0] {
            text.set_metrics(Metrics::new(size * scale, size * 1.22 * scale));
            text.shape_until_scroll(&mut self.fonts, false);
            text_size = text.layout_runs().fold((0.0_f32, 0.0_f32), |(w, h), run| {
                (w.max(run.line_w), h.max(run.line_top + run.line_height))
            });
            if text_size.1 + 2.0 * pad_y + 4.0 <= height as f32 {
                break;
            }
        }
        let (cx, cy) = (width as f32 / 2.0, height as f32 / 2.0);
        let half_w = (text_size.0 / 2.0 + pad_x).min((cx - 1.0).max(0.0));
        let half_h = (text_size.1 / 2.0 + pad_y).min((cy - 1.0).max(0.0));
        let radius = (18.0 * scale).min(half_w).min(half_h);
        let mut pixels = vec![0_u32; width * height];
        for y in ((cy - half_h).floor() as usize)..((cy + half_h).ceil() as usize) {
            for x in ((cx - half_w).floor() as usize)..((cx + half_w).ceil() as usize) {
                // Rounded rectangle signed distance, with a one-pixel
                // antialiasing ramp at the edge.
                let dx = (x as f32 + 0.5 - cx).abs() - (half_w - radius);
                let dy = (y as f32 + 0.5 - cy).abs() - (half_h - radius);
                let distance = dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0) - radius;
                let a = (195.0 * (0.5 - distance).clamp(0.0, 1.0)).round() as u8;
                blend(&mut pixels[y * width + x], Color::rgba(21, 24, 32, a));
            }
        }
        let (text_x, text_y) = (
            (cx - wrap_width / 2.0).round() as i32,
            (cy - text_size.1 / 2.0).round() as i32,
        );
        text.draw(
            &mut self.fonts,
            &mut self.glyphs,
            Color::rgb(255, 255, 255),
            |x, y, w, h, color| {
                let (x, y) = (x + text_x, y + text_y);
                for y in y.max(0)..(y + h as i32).min(height as i32) {
                    for x in x.max(0)..(x + w as i32).min(width as i32) {
                        blend(&mut pixels[y as usize * width + x as usize], color);
                    }
                }
            },
        );
        pixels.into_iter().flat_map(u32::to_ne_bytes).collect()
    }
}

// wl_shm ARGB8888 expects premultiplied color in native byte order.
fn blend(pixel: &mut u32, color: Color) {
    let [dst_a, dst_r, dst_g, dst_b] = pixel.to_be_bytes();
    let [r, g, b, a] = color.as_rgba();
    let over = |src: u8, dst: u8| {
        let a = u32::from(a);
        ((u32::from(src) * a + u32::from(dst) * (255 - a) + 127) / 255) as u8
    };
    *pixel = u32::from_be_bytes([
        over(255, dst_a),
        over(r, dst_r),
        over(g, dst_g),
        over(b, dst_b),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_pixels_are_premultiplied_argb() {
        let mut pixel = 0;
        blend(&mut pixel, Color::rgba(240, 100, 20, 128));
        assert_eq!(pixel.to_be_bytes(), [128, 120, 50, 10]);
        blend(&mut pixel, Color::rgb(255, 255, 255));
        assert_eq!(pixel, 0xffff_ffff);
    }
}
