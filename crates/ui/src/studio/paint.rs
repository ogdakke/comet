//! Unioned brush mask for Studio image edit.
//!
//! A silhouette is a raster: overlapping strokes must flatten to one fill
//! with a 1px edge, not a stack of ribbons. GPUI's scene cannot hold a
//! primitive per stamp (that blew the Metal instance buffer), so this
//! module owns an A8 coverage mask, incrementally paints a BGRA overlay,
//! and publishes one [`RenderImage`] for the viewer to blit.
//!
//! Coverage is a 1px anti-aliased distance field so diagonals do not stair-step.

use std::cell::RefCell;
use std::io::Cursor;
use std::sync::Arc;

use gpui::{App, RenderImage, Window};
use image::{Frame, GrayImage, Luma, Rgba, RgbaImage};

const FILL_ALPHA: u8 = 51; // 20% white

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BrushMode {
    Add,
    Subtract,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Stroke {
    pub points: Vec<(f32, f32)>,
    pub radius: f32,
    pub mode: BrushMode,
}

#[derive(Clone, Debug, PartialEq)]
enum PaintOp {
    Stroke(Stroke),
    Invert,
}

#[derive(Clone)]
pub(super) struct OverlayGpu {
    pub image: Arc<RenderImage>,
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

impl std::fmt::Debug for OverlayGpu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OverlayGpu")
            .field("x0", &self.x0)
            .field("y0", &self.y0)
            .field("x1", &self.x1)
            .field("y1", &self.y1)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub(super) struct PaintSession {
    pub width: u32,
    pub height: u32,
    ops: Vec<PaintOp>,
    redo: Vec<PaintOp>,
    live: Option<Stroke>,
    mask: GrayImage,
    overlay: RgbaImage,
    painted_bounds: Option<(u32, u32, u32, u32)>,
    gpu: Option<OverlayGpu>,
    stale_gpu: RefCell<Vec<Arc<RenderImage>>>,
}

impl PaintSession {
    pub(super) fn new(width: u32, height: u32) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        Self {
            width,
            height,
            ops: Vec::new(),
            redo: Vec::new(),
            live: None,
            mask: GrayImage::new(width, height),
            overlay: RgbaImage::new(width, height),
            painted_bounds: None,
            gpu: None,
            stale_gpu: RefCell::new(Vec::new()),
        }
    }

    pub(super) fn is_drawing(&self) -> bool {
        self.live.is_some()
    }

    pub(super) fn has_paint(&self) -> bool {
        self.painted_bounds.is_some()
            || self
                .live
                .as_ref()
                .is_some_and(|stroke| !stroke.points.is_empty())
    }

    pub(super) fn brush_radius(width: u32, height: u32, t: f32) -> f32 {
        let short = width.min(height) as f32;
        let min = 4.0;
        let max = (short * 0.18).max(min + 1.0);
        min + t.clamp(0.0, 1.0) * (max - min)
    }

    pub(super) fn overlay_gpu(&self) -> Option<OverlayGpu> {
        self.gpu.clone()
    }

    pub(super) fn flush_stale_gpu(&self, window: &mut Window, cx: &mut App) {
        for image in self.stale_gpu.borrow_mut().drain(..) {
            cx.drop_image(image, Some(window));
        }
    }

    pub(super) fn begin_stroke(&mut self, point: (f32, f32), radius: f32, mode: BrushMode) {
        self.redo.clear();
        let radius = radius.max(0.5);
        self.live = Some(Stroke {
            points: vec![point],
            radius,
            mode,
        });
        if self.stamp_disk(point.0, point.1, radius, mode) {
            self.publish_gpu();
        }
    }

    #[cfg(test)]
    pub(super) fn extend_stroke(&mut self, point: (f32, f32)) {
        self.extend_stroke_min(point, 0.75);
    }

    /// `min_distance` is in source-image pixels. Use a screen-space conversion
    /// so a 4K photo does not record a point every sub-pixel.
    pub(super) fn extend_stroke_min(&mut self, point: (f32, f32), min_distance: f32) {
        let Some(live) = self.live.as_ref() else {
            return;
        };
        let radius = live.radius;
        let mode = live.mode;
        let prev = live.points.last().copied();
        let Some((prev_x, prev_y)) = prev else {
            if let Some(live) = self.live.as_mut() {
                live.points.push(point);
            }
            if self.stamp_disk(point.0, point.1, radius, mode) {
                self.publish_gpu();
            }
            return;
        };
        let dx = point.0 - prev_x;
        let dy = point.1 - prev_y;
        let min = min_distance.max(0.35);
        if dx * dx + dy * dy < min * min {
            return;
        }
        if let Some(live) = self.live.as_mut() {
            live.points.push(point);
        }
        if self.stamp_capsule(prev_x, prev_y, point.0, point.1, radius, mode) {
            self.publish_gpu();
        }
    }

    pub(super) fn end_stroke(&mut self) {
        if let Some(live) = self.live.take()
            && !live.points.is_empty()
        {
            if live.mode == BrushMode::Subtract {
                self.rescan_bounds();
            }
            self.ops.push(PaintOp::Stroke(live));
            self.publish_gpu();
        }
    }

    pub(super) fn undo(&mut self) -> bool {
        let Some(op) = self.ops.pop() else {
            return false;
        };
        self.redo.push(op);
        self.rebuild_from_ops();
        true
    }

    pub(super) fn redo(&mut self) -> bool {
        let Some(op) = self.redo.pop() else {
            return false;
        };
        apply_op(&mut self.mask, &op);
        self.ops.push(op);
        self.rebuild_overlay_full();
        self.publish_gpu();
        true
    }

    pub(super) fn invert(&mut self) {
        self.commit_live();
        self.redo.clear();
        invert_mask(&mut self.mask);
        self.ops.push(PaintOp::Invert);
        self.rebuild_overlay_full();
        self.publish_gpu();
    }

    pub(super) fn reset(&mut self) {
        self.ops.clear();
        self.redo.clear();
        self.live = None;
        self.mask = GrayImage::new(self.width, self.height);
        self.overlay = RgbaImage::new(self.width, self.height);
        self.painted_bounds = None;
        self.replace_gpu(None);
    }

    #[cfg(test)]
    pub(super) fn mask_png(&self) -> Option<Vec<u8>> {
        self.mask_png_sized(self.width, self.height)
    }

    /// Binary white-on-black mask at `width`×`height`. Venice multi-edit
    /// treats extra images as layers; a mask that does not match the source
    /// pixels is ignored and the model rewrites the whole frame.
    pub(super) fn mask_png_sized(&self, width: u32, height: u32) -> Option<Vec<u8>> {
        if !self.has_paint() {
            return None;
        }
        let width = width.max(1);
        let height = height.max(1);
        let mut rgb = image::RgbImage::new(self.width, self.height);
        for (x, y, pixel) in self.mask.enumerate_pixels() {
            let value = if pixel.0[0] >= 128 { 255 } else { 0 };
            rgb.put_pixel(x, y, image::Rgb([value, value, value]));
        }
        let rgb = if width != self.width || height != self.height {
            image::imageops::resize(&rgb, width, height, image::imageops::FilterType::Nearest)
        } else {
            rgb
        };
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(rgb)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .ok()?;
        Some(encoded.into_inner())
    }

    fn commit_live(&mut self) {
        if let Some(live) = self.live.take()
            && !live.points.is_empty()
        {
            self.ops.push(PaintOp::Stroke(live));
        }
    }

    fn stamp_disk(&mut self, cx: f32, cy: f32, radius: f32, mode: BrushMode) -> bool {
        let rect = disk_rect(self.width, self.height, cx, cy, radius);
        if stamp_disk_mask(&mut self.mask, cx, cy, radius, mode) {
            refresh_overlay(
                &self.mask,
                &mut self.overlay,
                padded_rect(rect, self.width, self.height),
            );
            self.note_bounds(rect, mode);
            true
        } else {
            false
        }
    }

    fn stamp_capsule(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        radius: f32,
        mode: BrushMode,
    ) -> bool {
        let rect = capsule_rect(self.width, self.height, x0, y0, x1, y1, radius);
        if stamp_capsule_mask(&mut self.mask, x0, y0, x1, y1, radius, mode) {
            refresh_overlay(
                &self.mask,
                &mut self.overlay,
                padded_rect(rect, self.width, self.height),
            );
            self.note_bounds(rect, mode);
            true
        } else {
            false
        }
    }

    fn note_bounds(&mut self, rect: (u32, u32, u32, u32), mode: BrushMode) {
        if mode == BrushMode::Add {
            self.painted_bounds = Some(union_bounds(self.painted_bounds, rect));
        }
    }

    fn rescan_bounds(&mut self) {
        self.painted_bounds = scan_bounds(&self.mask);
    }

    fn rebuild_from_ops(&mut self) {
        self.mask = GrayImage::new(self.width, self.height);
        for op in &self.ops {
            apply_op(&mut self.mask, op);
        }
        if let Some(live) = &self.live {
            stamp_stroke_mask(&mut self.mask, live);
        }
        self.rebuild_overlay_full();
        self.publish_gpu();
    }

    fn rebuild_overlay_full(&mut self) {
        for y in 0..self.height {
            for x in 0..self.width {
                self.overlay
                    .put_pixel(x, y, overlay_pixel(&self.mask, x, y));
            }
        }
        self.rescan_bounds();
    }

    fn publish_gpu(&mut self) {
        let Some((x0, y0, x1, y1)) = self.painted_bounds else {
            self.replace_gpu(None);
            return;
        };
        let width = (x1 - x0 + 1).max(1);
        let height = (y1 - y0 + 1).max(1);
        let crop = image::imageops::crop_imm(&self.overlay, x0, y0, width, height).to_image();
        let gpu = OverlayGpu {
            image: Arc::new(RenderImage::new(vec![Frame::new(crop)])),
            x0,
            y0,
            x1,
            y1,
        };
        self.replace_gpu(Some(gpu));
    }

    fn replace_gpu(&mut self, next: Option<OverlayGpu>) {
        if let Some(old) = self.gpu.take() {
            self.stale_gpu.borrow_mut().push(old.image);
        }
        self.gpu = next;
    }
}

fn apply_op(mask: &mut GrayImage, op: &PaintOp) {
    match op {
        PaintOp::Stroke(stroke) => stamp_stroke_mask(mask, stroke),
        PaintOp::Invert => invert_mask(mask),
    }
}

fn stamp_stroke_mask(mask: &mut GrayImage, stroke: &Stroke) {
    if stroke.points.is_empty() {
        return;
    }
    if stroke.points.len() == 1 {
        stamp_disk_mask(
            mask,
            stroke.points[0].0,
            stroke.points[0].1,
            stroke.radius,
            stroke.mode,
        );
        return;
    }
    for pair in stroke.points.windows(2) {
        stamp_capsule_mask(
            mask,
            pair[0].0,
            pair[0].1,
            pair[1].0,
            pair[1].1,
            stroke.radius,
            stroke.mode,
        );
    }
}

/// Extra bbox padding so the 1px coverage ramp is stamped and refreshed.
const AA_PAD: f32 = 1.0;

fn stamp_disk_mask(mask: &mut GrayImage, cx: f32, cy: f32, radius: f32, mode: BrushMode) -> bool {
    stamp_coverage(mask, cx, cy, cx, cy, radius, mode)
}

fn stamp_capsule_mask(
    mask: &mut GrayImage,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    radius: f32,
    mode: BrushMode,
) -> bool {
    stamp_coverage(mask, x0, y0, x1, y1, radius, mode)
}

fn stamp_coverage(
    mask: &mut GrayImage,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    radius: f32,
    mode: BrushMode,
) -> bool {
    let width = mask.width() as i32;
    let height = mask.height() as i32;
    let radius = radius.max(0.5);
    let pad = radius + AA_PAD;
    let min_x = ((x0.min(x1) - pad).floor() as i32).max(0);
    let max_x = ((x0.max(x1) + pad).ceil() as i32).min(width - 1);
    let min_y = ((y0.min(y1) - pad).floor() as i32).max(0);
    let max_y = ((y0.max(y1) + pad).ceil() as i32).min(height - 1);
    let mut changed = false;
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let coverage = coverage_capsule(x as f32 + 0.5, y as f32 + 0.5, x0, y0, x1, y1, radius);
            if apply_coverage(mask.get_pixel_mut(x as u32, y as u32), coverage, mode) {
                changed = true;
            }
        }
    }
    changed
}

/// 1 inside, 0 outside, linear 1px ramp so the 50% contour sits on `radius`.
fn coverage_capsule(px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32, radius: f32) -> f32 {
    let vx = x1 - x0;
    let vy = y1 - y0;
    let len2 = vx * vx + vy * vy;
    let t = if len2 < f32::EPSILON {
        0.0
    } else {
        ((px - x0) * vx + (py - y0) * vy) / len2
    }
    .clamp(0.0, 1.0);
    let dx = px - (x0 + t * vx);
    let dy = py - (y0 + t * vy);
    let distance = (dx * dx + dy * dy).sqrt();
    (radius + 0.5 - distance).clamp(0.0, 1.0)
}

fn apply_coverage(pixel: &mut Luma<u8>, coverage: f32, mode: BrushMode) -> bool {
    if coverage <= 0.0 {
        return false;
    }
    let cov = (coverage * 255.0).round() as u8;
    let next = match mode {
        BrushMode::Add => pixel.0[0].max(cov),
        BrushMode::Subtract => {
            let remain = 255u16.saturating_sub(cov as u16);
            ((pixel.0[0] as u16 * remain + 127) / 255) as u8
        }
    };
    if next == pixel.0[0] {
        return false;
    }
    pixel.0[0] = next;
    true
}

fn invert_mask(mask: &mut GrayImage) {
    for pixel in mask.pixels_mut() {
        pixel.0[0] = 255 - pixel.0[0];
    }
}

const INSIDE: u8 = 128;

fn overlay_pixel(mask: &GrayImage, x: u32, y: u32) -> Rgba<u8> {
    let coverage = mask.get_pixel(x, y).0[0];
    if coverage == 0 {
        return Rgba([0, 0, 0, 0]);
    }
    // 8-neighbor ring on the inside of the 50% contour. 4-neighbor-only
    // marks a diagonal as isolated stairs (a dashed outline).
    if coverage >= INSIDE {
        let alpha = if neighbor_below(mask, x, y, INSIDE) {
            255
        } else {
            FILL_ALPHA
        };
        return Rgba([255, 255, 255, alpha]);
    }
    let alpha = (255.0 * (coverage as f32 / INSIDE as f32)).round() as u8;
    Rgba([255, 255, 255, alpha])
}

fn neighbor_below(mask: &GrayImage, x: u32, y: u32, threshold: u8) -> bool {
    let width = mask.width();
    let height = mask.height();
    let neighbors = [
        (x.wrapping_sub(1), y.wrapping_sub(1)),
        (x, y.wrapping_sub(1)),
        (x + 1, y.wrapping_sub(1)),
        (x.wrapping_sub(1), y),
        (x + 1, y),
        (x.wrapping_sub(1), y + 1),
        (x, y + 1),
        (x + 1, y + 1),
    ];
    neighbors
        .into_iter()
        .any(|(nx, ny)| nx >= width || ny >= height || mask.get_pixel(nx, ny).0[0] < threshold)
}

fn refresh_overlay(mask: &GrayImage, overlay: &mut RgbaImage, rect: (u32, u32, u32, u32)) {
    let (x0, y0, x1, y1) = rect;
    for y in y0..=y1 {
        for x in x0..=x1 {
            overlay.put_pixel(x, y, overlay_pixel(mask, x, y));
        }
    }
}

#[cfg(test)]
fn overlay_rgba(mask: &GrayImage) -> RgbaImage {
    let mut rgba = RgbaImage::new(mask.width(), mask.height());
    refresh_overlay(mask, &mut rgba, (0, 0, mask.width() - 1, mask.height() - 1));
    rgba
}

fn disk_rect(width: u32, height: u32, cx: f32, cy: f32, radius: f32) -> (u32, u32, u32, u32) {
    let radius = radius.max(0.5) + AA_PAD;
    let x0 = ((cx - radius).floor() as i32).max(0) as u32;
    let y0 = ((cy - radius).floor() as i32).max(0) as u32;
    let x1 = ((cx + radius).ceil() as i32).min(width as i32 - 1).max(0) as u32;
    let y1 = ((cy + radius).ceil() as i32).min(height as i32 - 1).max(0) as u32;
    (x0, y0, x1, y1)
}

fn capsule_rect(
    width: u32,
    height: u32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    radius: f32,
) -> (u32, u32, u32, u32) {
    let a = disk_rect(width, height, x0, y0, radius);
    let b = disk_rect(width, height, x1, y1, radius);
    union_bounds(Some(a), b)
}

fn padded_rect(rect: (u32, u32, u32, u32), width: u32, height: u32) -> (u32, u32, u32, u32) {
    (
        rect.0.saturating_sub(1),
        rect.1.saturating_sub(1),
        (rect.2 + 1).min(width.saturating_sub(1)),
        (rect.3 + 1).min(height.saturating_sub(1)),
    )
}

fn union_bounds(
    existing: Option<(u32, u32, u32, u32)>,
    rect: (u32, u32, u32, u32),
) -> (u32, u32, u32, u32) {
    match existing {
        None => rect,
        Some((x0, y0, x1, y1)) => (
            x0.min(rect.0),
            y0.min(rect.1),
            x1.max(rect.2),
            y1.max(rect.3),
        ),
    }
}

fn scan_bounds(mask: &GrayImage) -> Option<(u32, u32, u32, u32)> {
    let mut bounds = None;
    for (x, y, pixel) in mask.enumerate_pixels() {
        if pixel.0[0] > 0 {
            bounds = Some(union_bounds(bounds, (x, y, x, y)));
        }
    }
    bounds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn figure_eight_unions_and_has_no_interior_cross() {
        let mut session = PaintSession::new(80, 80);
        let radius = 6.0;
        session.begin_stroke((25.0, 25.0), radius, BrushMode::Add);
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
        assert_eq!(
            session.overlay.get_pixel(40, 40).0[3],
            FILL_ALPHA,
            "live overlay must match the silhouette rule"
        );
        assert!(session.overlay_gpu().is_some());
    }

    #[test]
    fn incremental_overlay_matches_full_rebuild() {
        let mut session = PaintSession::new(48, 48);
        session.begin_stroke((10.0, 10.0), 4.0, BrushMode::Add);
        session.extend_stroke((24.0, 18.0));
        session.extend_stroke((36.0, 30.0));
        session.end_stroke();
        let live = session.overlay.clone();
        session.rebuild_overlay_full();
        assert_eq!(live, session.overlay);
    }

    #[test]
    fn subtract_erases_from_the_union() {
        let mut session = PaintSession::new(40, 40);
        session.begin_stroke((20.0, 20.0), 8.0, BrushMode::Add);
        session.end_stroke();
        assert!(session.mask.get_pixel(20, 20).0[0] > 0);
        session.begin_stroke((20.0, 20.0), 5.0, BrushMode::Subtract);
        session.end_stroke();
        assert_eq!(session.mask.get_pixel(20, 20).0[0], 0);
        assert!(session.mask.get_pixel(12, 20).0[0] > 0);
        assert_eq!(session.overlay.get_pixel(20, 20).0[3], 0);
    }

    #[test]
    fn invert_flips_the_mask_and_is_undoable() {
        let mut session = PaintSession::new(24, 24);
        session.begin_stroke((8.0, 8.0), 3.0, BrushMode::Add);
        session.end_stroke();
        assert!(session.mask.get_pixel(8, 8).0[0] > 0);
        assert_eq!(session.mask.get_pixel(20, 20).0[0], 0);
        session.invert();
        assert_eq!(session.mask.get_pixel(8, 8).0[0], 0);
        assert!(session.mask.get_pixel(20, 20).0[0] > 0);
        assert!(session.undo());
        assert!(session.mask.get_pixel(8, 8).0[0] > 0);
        assert_eq!(session.mask.get_pixel(20, 20).0[0], 0);
    }

    #[test]
    fn reset_clears_paint() {
        let mut session = PaintSession::new(24, 24);
        session.begin_stroke((8.0, 8.0), 3.0, BrushMode::Add);
        session.end_stroke();
        session.invert();
        session.reset();
        assert!(!session.has_paint());
        assert_eq!(session.mask.get_pixel(8, 8).0[0], 0);
        assert!(session.overlay_gpu().is_none());
    }

    #[test]
    fn mask_png_is_white_on_black() {
        let mut session = PaintSession::new(32, 32);
        session.begin_stroke((16.0, 16.0), 4.0, BrushMode::Add);
        session.end_stroke();
        let png = session.mask_png().expect("painted mask");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        let decoded = image::load_from_memory(&png).unwrap().to_rgb8();
        assert_eq!(*decoded.get_pixel(16, 16), image::Rgb([255, 255, 255]));
        assert_eq!(*decoded.get_pixel(0, 0), image::Rgb([0, 0, 0]));
    }

    #[test]
    fn mask_png_can_be_resized_to_the_source() {
        let mut session = PaintSession::new(32, 32);
        session.begin_stroke((16.0, 16.0), 6.0, BrushMode::Add);
        session.end_stroke();
        let png = session.mask_png_sized(64, 48).expect("resized mask");
        let decoded = image::load_from_memory(&png).unwrap().to_rgb8();
        assert_eq!(decoded.dimensions(), (64, 48));
        assert_eq!(*decoded.get_pixel(32, 24), image::Rgb([255, 255, 255]));
        assert_eq!(*decoded.get_pixel(0, 0), image::Rgb([0, 0, 0]));
    }

    #[test]
    fn smallest_brush_is_four_source_pixels() {
        assert_eq!(PaintSession::brush_radius(1024, 1024, 0.0), 4.0);
        assert!(PaintSession::brush_radius(1024, 1024, 1.0) > 4.0);
    }

    #[test]
    fn diagonal_stroke_has_an_antialiased_fringe() {
        let mut session = PaintSession::new(48, 48);
        session.begin_stroke((8.0, 8.0), 5.0, BrushMode::Add);
        session.extend_stroke((40.0, 40.0));
        session.end_stroke();
        let overlay = overlay_rgba(&session.mask);
        let mut fringe = 0u32;
        for y in 0..48 {
            for x in 0..48 {
                let alpha = overlay.get_pixel(x, y).0[3];
                if alpha > 0 && alpha < 255 && alpha != FILL_ALPHA {
                    fringe += 1;
                }
            }
        }
        assert!(
            fringe > 0,
            "diagonal edge should have an anti-aliased fringe, got {fringe}"
        );

        let mut isolated = 0u32;
        let mut edge = 0u32;
        for y in 1..47 {
            for x in 1..47 {
                if overlay.get_pixel(x, y).0[3] != 255 {
                    continue;
                }
                edge += 1;
                let linked = [
                    overlay.get_pixel(x - 1, y - 1).0[3],
                    overlay.get_pixel(x, y - 1).0[3],
                    overlay.get_pixel(x + 1, y - 1).0[3],
                    overlay.get_pixel(x - 1, y).0[3],
                    overlay.get_pixel(x + 1, y).0[3],
                    overlay.get_pixel(x - 1, y + 1).0[3],
                    overlay.get_pixel(x, y + 1).0[3],
                    overlay.get_pixel(x + 1, y + 1).0[3],
                ]
                .into_iter()
                .any(|alpha| alpha == 255);
                if !linked {
                    isolated += 1;
                }
            }
        }
        assert!(edge > 20, "diagonal should have a connected silhouette");
        assert!(
            isolated * 4 < edge,
            "outline should not dash: {isolated} isolated of {edge} edge pixels"
        );
    }

    #[test]
    fn undo_clears_the_last_stroke() {
        let mut session = PaintSession::new(40, 40);
        session.begin_stroke((10.0, 10.0), 3.0, BrushMode::Add);
        session.end_stroke();
        session.begin_stroke((30.0, 30.0), 3.0, BrushMode::Add);
        session.end_stroke();
        assert!(session.mask.get_pixel(30, 30).0[0] > 0);
        assert!(session.undo());
        assert_eq!(session.mask.get_pixel(30, 30).0[0], 0);
        assert!(session.mask.get_pixel(10, 10).0[0] > 0);
        assert!(session.redo());
        assert!(session.mask.get_pixel(30, 30).0[0] > 0);
    }
}
