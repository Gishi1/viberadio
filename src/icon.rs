//! The VibeRadio mark: a dark rounded square with teal sound waves.
//!
//! The same drawing backs the window icon and the tray icon, so the title
//! bar, taskbar and tray all show the branding from `packaging/viberadio.svg`.

const BACKGROUND: (f32, f32, f32) = (0.086, 0.094, 0.110);
const INNER: (f32, f32, f32) = (0.247, 0.714, 0.659);
const OUTER: (f32, f32, f32) = (0.165, 0.435, 0.408);

/// Premultiplied RGBA pixels for an icon of `size` pixels square.
pub fn rgba(size: u32) -> Vec<u8> {
    const SAMPLES: u32 = 4;
    let size = size.max(16);
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let mut red = 0.0;
            let mut green = 0.0;
            let mut blue = 0.0;
            let mut alpha = 0.0;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let nx = (x as f32 + (sx as f32 + 0.5) / SAMPLES as f32) / size as f32;
                    let ny = (y as f32 + (sy as f32 + 0.5) / SAMPLES as f32) / size as f32;
                    let (r, g, b, a) = sample(nx, ny);
                    red += r * a;
                    green += g * a;
                    blue += b * a;
                    alpha += a;
                }
            }
            if alpha <= 0.0 {
                out.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                let scale = 255.0 / alpha;
                out.extend_from_slice(&[
                    (red * scale).round() as u8,
                    (green * scale).round() as u8,
                    (blue * scale).round() as u8,
                    (alpha / (SAMPLES * SAMPLES) as f32 * 255.0).round() as u8,
                ]);
            }
        }
    }
    out
}

fn sample(x: f32, y: f32) -> (f32, f32, f32, f32) {
    if !inside_rounded(x, y, 0.1875) {
        return (0.0, 0.0, 0.0, 0.0);
    }
    let dx = x - 0.5;
    let dy = y - 0.5;
    if dx * dx + dy * dy <= 0.0625f32.powi(2) {
        return (INNER.0, INNER.1, INNER.2, 1.0);
    }
    if arc(x, y, 0.359375, 0.19921875, 0.025390625, true)
        || arc(x, y, 0.640625, 0.19921875, 0.025390625, false)
    {
        return (INNER.0, INNER.1, INNER.2, 1.0);
    }
    if arc(x, y, 0.25, 0.3515625, 0.021484375, true)
        || arc(x, y, 0.75, 0.3515625, 0.021484375, false)
    {
        return (OUTER.0, OUTER.1, OUTER.2, 1.0);
    }
    (BACKGROUND.0, BACKGROUND.1, BACKGROUND.2, 1.0)
}

/// A half-ring centered at `(cx, 0.5)` with `radius` and half the stroke
/// width `half_width`, kept on one side of its center.
fn arc(x: f32, y: f32, cx: f32, radius: f32, half_width: f32, left: bool) -> bool {
    if if left { x > cx } else { x < cx } {
        return false;
    }
    let dx = x - cx;
    let dy = y - 0.5;
    ((dx * dx + dy * dy).sqrt() - radius).abs() <= half_width
}

fn inside_rounded(x: f32, y: f32, radius: f32) -> bool {
    if !(0.0..=1.0).contains(&x) || !(0.0..=1.0).contains(&y) {
        return false;
    }
    let cx = x.clamp(radius, 1.0 - radius);
    let cy = y.clamp(radius, 1.0 - radius);
    let dx = x - cx;
    let dy = y - cy;
    dx * dx + dy * dy <= radius * radius
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mark_is_teal_with_a_transparent_corner() {
        let size = 32u32;
        let data = rgba(size);
        assert_eq!(data.len(), (size * size * 4) as usize);
        let pixel = |x: u32, y: u32| &data[((y * size + x) * 4) as usize..][..4];
        let center = pixel(size / 2, size / 2);
        assert_eq!(center[3], 255);
        assert!(center[0] > 40 && center[1] > 150 && center[2] > 130);
        assert_eq!(pixel(0, 0), [0, 0, 0, 0]);
        assert!(data
            .chunks_exact(4)
            .any(|px| px[3] == 255 && px[0] > 40 && px[1] > 150 && px[2] > 130));
    }
}
