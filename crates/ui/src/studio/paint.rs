//! Non-destructive brush strokes for Studio image edit.
//!
//! Strokes are stored in source-image pixels. Overlap is unioned into an A8
//! mask so a figure-8 has a silhouette and no crossing X.

use std::io::Cursor;
use std::sync::Arc;

use gpui::{Image, ImageFormat};
use image::{GrayImage, Luma, Rgba, RgbaImage};

const FILL_ALPHA: u8 = 51; // 20% white

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Stroke {
    pub points: Vec<(f32, f32)>,
    pub radius: f32,
}

#[derive(Clone, Debug)]
pub(super) struct PaintSession {
    pub width: u32,
    pub height: u32,
    pub strokes: Vec<Stroke>,
    pub redo: Vec<Stroke>,
    live: Option<Stroke>,
    mask: GrayImage,
}

impl PaintSession {
    pub(super) fn new(width: u32, height: u32) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        Self {
            width,
            height,
            strokes: Vec::new(),
            redo: Vec::new(),
            live: None,
            mask: GrayImage::new(width, height),
        }
    }

    pub(super) fn is_drawing(&self) -> bool {
        self.live.is_some()
    }

    pub(super) fn has_paint(&self) -> bool {
        self.strokes.iter().any(|stroke| !stroke.points.is_empty())
            || self
                .live
                .as_ref()
                .is_some_and(|stroke| !stroke.points.is_empty())
    }

    pub(super) fn brush_radius(width: u32, height: u32, t: f32) -> f32 {
        let short = width.min(height) as f32;
        let min = 2.0;
        let max = (short * 0.18).max(min + 1.0);
        min + t.clamp(0.0, 1.0) * (max - min)
    }

    pub(super) fn begin_stroke(&mut self, point: (f32, f32), radius: f32) {
        self.redo.clear();
        self.live = Some(Stroke {
            points: vec![point],
            radius: radius.max(0.5),
        });
        stamp_disk(&mut self.mask, point.0, point.1, radius.max(0.5));
    }

    pub(super) fn extend_stroke(&mut self, point: (f32, f32)) {
        let Some(live) = self.live.as_mut() else {
            return;
        };
        let Some(&(prev_x, prev_y)) = live.points.last() else {
            live.points.push(point);
            stamp_disk(&mut self.mask, point.0, point.1, live.radius);
            return;
        };
        let dx = point.0 - prev_x;
        let dy = point.1 - prev_y;
        if dx * dx + dy * dy < 0.25 {
            return;
        }
        live.points.push(point);
        stamp_capsule(
            &mut self.mask,
            prev_x,
            prev_y,
            point.0,
            point.1,
            live.radius,
        );
    }

    pub(super) fn end_stroke(&mut self) {
        if let Some(live) = self.live.take()
            && !live.points.is_empty()
        {
            self.strokes.push(live);
        }
    }

    pub(super) fn undo(&mut self) -> bool {
        let Some(stroke) = self.strokes.pop() else {
            return false;
        };
        self.redo.push(stroke);
        self.rebuild_mask();
        true
    }

    pub(super) fn redo(&mut self) -> bool {
        let Some(stroke) = self.redo.pop() else {
            return false;
        };
        stamp_stroke(&mut self.mask, &stroke);
        self.strokes.push(stroke);
        true
    }

    pub(super) fn overlay_image(&self) -> Option<Arc<Image>> {
        if !self.has_paint() {
            return None;
        }
        let rgba = overlay_rgba(&self.mask);
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(rgba)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .ok()?;
        Some(Arc::new(Image::from_bytes(
            ImageFormat::Png,
            encoded.into_inner(),
        )))
    }

    pub(super) fn mask_png(&self) -> Option<Vec<u8>> {
        if !self.has_paint() {
            return None;
        }
        let mut rgb = image::RgbImage::new(self.width, self.height);
        for (x, y, pixel) in self.mask.enumerate_pixels() {
            let value = if pixel.0[0] > 0 { 255 } else { 0 };
            rgb.put_pixel(x, y, image::Rgb([value, value, value]));
        }
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(rgb)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .ok()?;
        Some(encoded.into_inner())
    }

    fn rebuild_mask(&mut self) {
        self.mask = GrayImage::new(self.width, self.height);
        for stroke in &self.strokes {
            stamp_stroke(&mut self.mask, stroke);
        }
        if let Some(live) = &self.live {
            stamp_stroke(&mut self.mask, live);
        }
    }
}

fn stamp_stroke(mask: &mut GrayImage, stroke: &Stroke) {
    if stroke.points.is_empty() {
        return;
    }
    if stroke.points.len() == 1 {
        stamp_disk(mask, stroke.points[0].0, stroke.points[0].1, stroke.radius);
        return;
    }
    for pair in stroke.points.windows(2) {
        stamp_capsule(
            mask,
            pair[0].0,
            pair[0].1,
            pair[1].0,
            pair[1].1,
            stroke.radius,
        );
    }
}

fn stamp_disk(mask: &mut GrayImage, cx: f32, cy: f32, radius: f32) {
    let width = mask.width() as i32;
    let height = mask.height() as i32;
    let radius = radius.max(0.5);
    let min_x = ((cx - radius).floor() as i32).max(0);
    let max_x = ((cx + radius).ceil() as i32).min(width - 1);
    let min_y = ((cy - radius).floor() as i32).max(0);
    let max_y = ((cy + radius).ceil() as i32).min(height - 1);
    let r2 = radius * radius;
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            if dx * dx + dy * dy <= r2 {
                mask.put_pixel(x as u32, y as u32, Luma([255]));
            }
        }
    }
}

fn stamp_capsule(mask: &mut GrayImage, x0: f32, y0: f32, x1: f32, y1: f32, radius: f32) {
    stamp_disk(mask, x0, y0, radius);
    stamp_disk(mask, x1, y1, radius);
    let vx = x1 - x0;
    let vy = y1 - y0;
    let len2 = vx * vx + vy * vy;
    if len2 < f32::EPSILON {
        return;
    }
    let width = mask.width() as i32;
    let height = mask.height() as i32;
    let radius = radius.max(0.5);
    let min_x = ((x0.min(x1) - radius).floor() as i32).max(0);
    let max_x = ((x0.max(x1) + radius).ceil() as i32).min(width - 1);
    let min_y = ((y0.min(y1) - radius).floor() as i32).max(0);
    let max_y = ((y0.max(y1) + radius).ceil() as i32).min(height - 1);
    let r2 = radius * radius;
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let t = ((px - x0) * vx + (py - y0) * vy) / len2;
            if (0.0..=1.0).contains(&t) {
                let dx = px - (x0 + t * vx);
                let dy = py - (y0 + t * vy);
                if dx * dx + dy * dy <= r2 {
                    mask.put_pixel(x as u32, y as u32, Luma([255]));
                }
            }
        }
    }
}

fn overlay_rgba(mask: &GrayImage) -> RgbaImage {
    let width = mask.width();
    let height = mask.height();
    let mut rgba = RgbaImage::new(width, height);
    for y in 0..height {
        for x in 0..width {
            if mask.get_pixel(x, y).0[0] == 0 {
                continue;
            }
            let edge = neighbor_empty(mask, x, y);
            let alpha = if edge { 255 } else { FILL_ALPHA };
            rgba.put_pixel(x, y, Rgba([255, 255, 255, alpha]));
        }
    }
    rgba
}

fn neighbor_empty(mask: &GrayImage, x: u32, y: u32) -> bool {
    let width = mask.width();
    let height = mask.height();
    let neighbors = [
        (x.wrapping_sub(1), y),
        (x + 1, y),
        (x, y.wrapping_sub(1)),
        (x, y + 1),
    ];
    neighbors
        .into_iter()
        .any(|(nx, ny)| nx >= width || ny >= height || mask.get_pixel(nx, ny).0[0] == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn figure_eight_unions_and_has_no_interior_cross() {
        let mut session = PaintSession::new(80, 80);
        let radius = 6.0;
        session.begin_stroke((25.0, 25.0), radius);
        for t in 1..=40 {
            let a = t as f32 / 40.0 * std::f32::consts::TAU;
            session.extend_stroke((25.0 + 12.0 * a.cos(), 25.0 + 12.0 * a.sin()));
        }
        session.extend_stroke((40.0, 40.0));
        for t in 1..=40 {
            let a = t as f32 / 40.0 * std::f32::consts::TAU;
            session.extend_stroke((55.0 + 12.0 * a.cos(), 55.0 + 12.0 * a.sin()));
        }
        session.end_stroke();

        let crossing = session.mask.get_pixel(40, 40).0[0];
        assert_eq!(crossing, 255, "union must fill the figure-8 crossing");

        let overlay = overlay_rgba(&session.mask);
        let mut interior_white = 0u32;
        let mut edge_white = 0u32;
        for y in 1..79 {
            for x in 1..79 {
                let pixel = overlay.get_pixel(x, y);
                if pixel.0[3] == 0 {
                    continue;
                }
                if pixel.0[3] == 255 {
                    edge_white += 1;
                } else {
                    interior_white += 1;
                }
            }
        }
        assert!(interior_white > 50, "fill should cover the unioned loops");
        assert!(edge_white > 20, "silhouette should outline the union");

        let center = overlay.get_pixel(40, 40);
        assert_eq!(
            center.0[3], FILL_ALPHA,
            "crossing must be fill, not an interior stroke X"
        );
    }

    #[test]
    fn mask_png_is_white_on_black() {
        let mut session = PaintSession::new(32, 32);
        session.begin_stroke((16.0, 16.0), 4.0);
        session.end_stroke();
        let png = session.mask_png().expect("painted mask");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        let decoded = image::load_from_memory(&png).unwrap().to_rgb8();
        assert_eq!(*decoded.get_pixel(16, 16), image::Rgb([255, 255, 255]));
        assert_eq!(*decoded.get_pixel(0, 0), image::Rgb([0, 0, 0]));
    }

    #[test]
    fn undo_clears_the_last_stroke() {
        let mut session = PaintSession::new(40, 40);
        session.begin_stroke((10.0, 10.0), 3.0);
        session.end_stroke();
        session.begin_stroke((30.0, 30.0), 3.0);
        session.end_stroke();
        assert!(session.mask.get_pixel(30, 30).0[0] > 0);
        assert!(session.undo());
        assert_eq!(session.mask.get_pixel(30, 30).0[0], 0);
        assert!(session.mask.get_pixel(10, 10).0[0] > 0);
        assert!(session.redo());
        assert!(session.mask.get_pixel(30, 30).0[0] > 0);
    }
}
