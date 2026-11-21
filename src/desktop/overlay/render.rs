use anyhow::{Result, ensure};
use cosmic_text::{
    Align, Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, SwashCache, Weight, Wrap,
};

use super::SCALE;

const BACKGROUND: Color = Color::rgba(21, 24, 32, 195);
const ACTIVE_DOT: Color = Color::rgb(255, 255, 255);
const INACTIVE_DOT: Color = Color::rgb(154, 160, 170);

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
        rounded_rect(
            &mut pixels,
            width,
            (cx, cy),
            (half_w, half_h),
            radius,
            BACKGROUND,
        );
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

pub(super) fn workspace_dots(row: usize, total: usize, width: usize, height: usize) -> Vec<u8> {
    let scale = SCALE as f32;
    let (cx, cy) = (width as f32 / 2.0, height as f32 / 2.0);
    let half_w = cx - 0.5;
    let pad = (10.0 * scale).min(cy / 3.0);
    // Compress long columns to fit while keeping the active dot in its true row.
    let spacing = (24.0 * scale).min(2.0 * (cy - pad - 0.5) / total as f32);
    let active_radius = (4.5 * scale).min(spacing * 0.35).min(half_w / 2.0);
    let span = (total - 1) as f32 * spacing;
    let half_h = span / 2.0 + active_radius + pad;
    let mut pixels = vec![0; width * height];
    rounded_rect(
        &mut pixels,
        width,
        (cx, cy),
        (half_w, half_h),
        half_w.min(half_h),
        BACKGROUND,
    );
    for i in 0..total {
        let y = cy - span / 2.0 + i as f32 * spacing;
        let (radius, color) = if row == i + 1 {
            (active_radius, ACTIVE_DOT)
        } else {
            (active_radius * (2.0 / 3.0), INACTIVE_DOT)
        };
        rounded_rect(&mut pixels, width, (cx, y), (radius, radius), radius, color);
    }
    pixels.into_iter().flat_map(u32::to_ne_bytes).collect()
}

fn rounded_rect(
    pixels: &mut [u32],
    width: usize,
    (cx, cy): (f32, f32),
    (half_w, half_h): (f32, f32),
    radius: f32,
    color: Color,
) {
    let [r, g, b, alpha] = color.as_rgba();
    for y in
        (cy - half_h).floor() as usize..((cy + half_h).ceil() as usize).min(pixels.len() / width)
    {
        for x in (cx - half_w).floor() as usize..((cx + half_w).ceil() as usize).min(width) {
            // Rounded rectangle signed distance with a one-pixel antialiasing ramp.
            let dx = (x as f32 + 0.5 - cx).abs() - (half_w - radius);
            let dy = (y as f32 + 0.5 - cy).abs() - (half_h - radius);
            let distance = dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0) - radius;
            let a = (alpha as f32 * (0.5 - distance).clamp(0.0, 1.0)).round() as u8;
            blend(&mut pixels[y * width + x], Color::rgba(r, g, b, a));
        }
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

    #[test]
    fn dots_show_only_the_current_project_rows_and_highlight_the_active_one() {
        let (width, height) = (56, 768);
        for (row, total) in [(1, 1), (1, 3), (2, 3), (3, 3), (37, 50)] {
            let pixels = workspace_dots(row, total, width, height);
            let mut marks = Vec::new();
            for (y, line) in pixels.chunks_exact(width * 4).enumerate() {
                let x = width / 2 * 4;
                let color = u32::from_ne_bytes(line[x..x + 4].try_into().unwrap());
                if color == ACTIVE_DOT.0 || color == INACTIVE_DOT.0 {
                    if marks.last().is_none_or(|(_, end, _)| *end + 1 != y) {
                        marks.push((y, y, color));
                    } else if let Some((_, end, _)) = marks.last_mut() {
                        *end = y;
                    }
                }
            }
            let expected: Vec<_> = (1..=total)
                .map(|i| {
                    if i == row {
                        ACTIVE_DOT.0
                    } else {
                        INACTIVE_DOT.0
                    }
                })
                .collect();
            assert_eq!(
                marks.iter().map(|&(_, _, color)| color).collect::<Vec<_>>(),
                expected
            );
            let (start, end, _) = marks[row - 1];
            assert!(
                marks
                    .iter()
                    .filter(|&&(_, _, color)| color == INACTIVE_DOT.0)
                    .all(|&(top, bottom, _)| bottom - top < end - start)
            );
            if total == 3 {
                for pair in marks.windows(2) {
                    let (top, bottom, _) = pair[0];
                    let (next_top, next_bottom, _) = pair[1];
                    assert_eq!((next_top + next_bottom - top - bottom) / 2, 48);
                }
            }
            assert_eq!(pixels[..4], [0; 4]);
        }
    }
}
