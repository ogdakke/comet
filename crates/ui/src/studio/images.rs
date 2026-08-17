//! Bounded decoded-image cache for Studio. Gallery tiles keep small thumbs;
//! visible thread tiles keep a 1280px display frame; the lightbox keeps a
//! handful of full-resolution originals. The three budgets are independent
//! so hover/lightbox originals cannot evict the grid.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::sync::Arc;

use base64::Engine as _;
use gpui::{App, Image, ImageFormat, Window};
use zeron_studio::StudioArtifactId;

/// Encoded JPEG budget. Decoded GPU size is much larger, so the count cap
/// is the real bound — see `THUMB_CACHE_MAX`.
const THUMB_CACHE_BUDGET_BYTES: usize = 16 * 1024 * 1024;
/// Visible grid plus a couple of screens of runway. 400 thumbs at 640px
/// decoded to ~1GB of Metal textures; keep this near what can be on screen.
const THUMB_CACHE_MAX: usize = 96;
const FULL_CACHE_BUDGET_BYTES: usize = 96 * 1024 * 1024;
/// Encoded originals for hover/lightbox only. These must not be uploaded to
/// the GPU except for the open lightbox slides.
const FULL_CACHE_MAX: usize = 16;
/// Viewport-sized thread frames. Larger than a 512 preview, far smaller than
/// a native 4K texture on every tile.
const DISPLAY_CACHE_BUDGET_BYTES: usize = 24 * 1024 * 1024;
const DISPLAY_CACHE_MAX: usize = 16;

enum ImageSlot {
    Thumb,
    Display,
    Full,
}

struct CachedImage {
    image: Arc<Image>,
    bytes: usize,
    last_used: u64,
}

#[derive(Default)]
pub(super) struct StudioImages {
    thumbs: HashMap<StudioArtifactId, CachedImage>,
    displays: HashMap<StudioArtifactId, CachedImage>,
    full: HashMap<StudioArtifactId, CachedImage>,
    placeholders: HashMap<StudioArtifactId, Arc<Image>>,
    tick: u64,
    thumb_bytes: usize,
    display_bytes: usize,
    full_bytes: usize,
    pending_free: Vec<Arc<Image>>,
}

pub(super) fn image_from_thumbhash(thumbhash: &str) -> Option<Arc<Image>> {
    let hash = base64::engine::general_purpose::STANDARD
        .decode(thumbhash)
        .ok()?;
    let (width, height, rgba) = thumbhash::thumb_hash_to_rgba(&hash).ok()?;
    let buffer = image::RgbaImage::from_raw(width as u32, height as u32, rgba)?;
    let mut encoded = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(buffer)
        .write_to(&mut encoded, image::ImageFormat::Png)
        .ok()?;
    Some(Arc::new(Image::from_bytes(
        ImageFormat::Png,
        encoded.into_inner(),
    )))
}

impl StudioImages {
    pub(super) fn get_full(&self, id: &StudioArtifactId) -> Option<Arc<Image>> {
        self.full.get(id).map(|entry| entry.image.clone())
    }

    pub(super) fn get_thumb(&self, id: &StudioArtifactId) -> Option<Arc<Image>> {
        self.thumbs
            .get(id)
            .or_else(|| self.full.get(id))
            .map(|entry| entry.image.clone())
    }

    pub(super) fn get_thumb_only(&self, id: &StudioArtifactId) -> Option<Arc<Image>> {
        self.thumbs.get(id).map(|entry| entry.image.clone())
    }

    pub(super) fn get_display(&self, id: &StudioArtifactId) -> Option<Arc<Image>> {
        self.displays.get(id).map(|entry| entry.image.clone())
    }

    pub(super) fn contains_full(&self, id: &StudioArtifactId) -> bool {
        self.full.contains_key(id)
    }

    pub(super) fn contains_thumb(&self, id: &StudioArtifactId) -> bool {
        self.thumbs.contains_key(id)
    }

    pub(super) fn contains_display(&self, id: &StudioArtifactId) -> bool {
        self.displays.contains_key(id)
    }

    pub(super) fn get_placeholder(&self, id: &StudioArtifactId) -> Option<Arc<Image>> {
        self.placeholders.get(id).cloned()
    }

    pub(super) fn ensure_placeholder(&mut self, id: StudioArtifactId, thumbhash: &str) {
        if self.placeholders.contains_key(&id) {
            return;
        }
        if let Some(image) = image_from_thumbhash(thumbhash) {
            self.placeholders.insert(id, image);
        }
    }

    pub(super) fn insert_thumb(&mut self, id: StudioArtifactId, image: Arc<Image>) {
        if self.thumbs.contains_key(&id) {
            return;
        }
        self.insert_map(ImageSlot::Thumb, id, image);
    }

    pub(super) fn insert_display(&mut self, id: StudioArtifactId, image: Arc<Image>) {
        if self.displays.contains_key(&id) {
            return;
        }
        self.insert_map(ImageSlot::Display, id, image);
    }

    pub(super) fn insert_full(&mut self, id: StudioArtifactId, image: Arc<Image>) {
        if self.full.contains_key(&id) {
            return;
        }
        self.insert_map(ImageSlot::Full, id, image);
    }

    fn insert_map(&mut self, slot: ImageSlot, id: StudioArtifactId, image: Arc<Image>) {
        self.tick = self.tick.saturating_add(1);
        let bytes = image.bytes.len();
        let entry = CachedImage {
            image,
            bytes,
            last_used: self.tick,
        };
        let (map, loaded) = match slot {
            ImageSlot::Thumb => (&mut self.thumbs, &mut self.thumb_bytes),
            ImageSlot::Display => (&mut self.displays, &mut self.display_bytes),
            ImageSlot::Full => (&mut self.full, &mut self.full_bytes),
        };
        if let Some(previous) = map.insert(id, entry) {
            *loaded = loaded.saturating_sub(previous.bytes);
            self.pending_free.push(previous.image);
        }
        *loaded = loaded.saturating_add(bytes);
    }

    pub(super) fn remove(&mut self, id: &StudioArtifactId) {
        if let Some(previous) = self.thumbs.remove(id) {
            self.thumb_bytes = self.thumb_bytes.saturating_sub(previous.bytes);
            self.pending_free.push(previous.image);
        }
        if let Some(previous) = self.displays.remove(id) {
            self.display_bytes = self.display_bytes.saturating_sub(previous.bytes);
            self.pending_free.push(previous.image);
        }
        if let Some(previous) = self.full.remove(id) {
            self.full_bytes = self.full_bytes.saturating_sub(previous.bytes);
            self.pending_free.push(previous.image);
        }
        self.placeholders.remove(id);
    }

    pub(super) fn touch(&mut self, ids: impl IntoIterator<Item = StudioArtifactId>) {
        self.tick = self.tick.saturating_add(1);
        let tick = self.tick;
        for id in ids {
            if let Some(entry) = self.thumbs.get_mut(&id) {
                entry.last_used = tick;
            }
            if let Some(entry) = self.displays.get_mut(&id) {
                entry.last_used = tick;
            }
            if let Some(entry) = self.full.get_mut(&id) {
                entry.last_used = tick;
            }
        }
    }

    /// Drop the GPU atlas tile but keep the encoded bytes. Thread tiles that
    /// just left the reading band must not keep a 1280px Metal texture.
    pub(super) fn release_gpu(&mut self, image: Arc<Image>) {
        self.pending_free.push(image);
    }

    pub(super) fn evict(&mut self, protected: &HashSet<StudioArtifactId>) {
        evict_map(
            &mut self.thumbs,
            &mut self.thumb_bytes,
            &mut self.pending_free,
            protected,
            THUMB_CACHE_MAX,
            THUMB_CACHE_BUDGET_BYTES,
        );
        evict_map(
            &mut self.displays,
            &mut self.display_bytes,
            &mut self.pending_free,
            protected,
            DISPLAY_CACHE_MAX,
            DISPLAY_CACHE_BUDGET_BYTES,
        );
        evict_map(
            &mut self.full,
            &mut self.full_bytes,
            &mut self.pending_free,
            protected,
            FULL_CACHE_MAX,
            FULL_CACHE_BUDGET_BYTES,
        );
    }

    pub(super) fn flush(&mut self, window: Option<&mut Window>, cx: &mut App) {
        let evicted = std::mem::take(&mut self.pending_free);
        let mut window = window;
        for image in evicted {
            gpui::ImageSource::Image(image).evict(window.as_deref_mut(), cx);
        }
    }
}

fn evict_map(
    map: &mut HashMap<StudioArtifactId, CachedImage>,
    loaded_bytes: &mut usize,
    pending_free: &mut Vec<Arc<Image>>,
    protected: &HashSet<StudioArtifactId>,
    max_count: usize,
    max_bytes: usize,
) {
    while map.len() > max_count || *loaded_bytes > max_bytes {
        let Some(id) = map
            .iter()
            .filter(|(id, _)| !protected.contains(*id))
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(id, _)| *id)
        else {
            break;
        };
        if let Some(previous) = map.remove(&id) {
            *loaded_bytes = loaded_bytes.saturating_sub(previous.bytes);
            pending_free.push(previous.image);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy(bytes: usize) -> Arc<Image> {
        Arc::new(Image::from_bytes(ImageFormat::Jpeg, vec![0; bytes]))
    }

    fn id(n: u128) -> StudioArtifactId {
        StudioArtifactId(uuid::Uuid::from_u128(n))
    }

    #[test]
    fn fulls_do_not_evict_thumbs() {
        let mut images = StudioImages::default();
        let thumb_id = id(1);
        images.insert_thumb(thumb_id, dummy(64));
        for n in 2..50 {
            images.insert_full(id(n), dummy(4 * 1024 * 1024));
        }
        images.evict(&HashSet::new());
        assert!(images.contains_thumb(&thumb_id));
        assert!(images.full.len() <= FULL_CACHE_MAX);
    }

    #[test]
    fn a_cached_original_is_not_a_gallery_thumb() {
        let mut images = StudioImages::default();
        let id = id(7);
        images.insert_full(id, dummy(1024));
        assert!(!images.contains_thumb(&id));
        assert!(images.get_thumb_only(&id).is_none());
        assert!(images.get_thumb(&id).is_some());
    }

    #[test]
    fn placeholders_survive_thumb_eviction() {
        let mut images = StudioImages::default();
        let keep = id(1);
        let rgba = vec![128u8; 16 * 16 * 4];
        let hash = thumbhash::rgba_to_thumb_hash(16, 16, &rgba);
        let encoded = base64::engine::general_purpose::STANDARD.encode(hash);
        images.ensure_placeholder(keep, &encoded);
        assert!(images.get_placeholder(&keep).is_some());
        for n in 1..=THUMB_CACHE_MAX as u128 + 8 {
            images.insert_thumb(id(n), dummy(8));
        }
        images.evict(&HashSet::new());
        assert!(images.get_placeholder(&keep).is_some());
        assert!(images.thumbs.len() <= THUMB_CACHE_MAX);
    }

    #[test]
    fn displays_do_not_evict_thumbs() {
        let mut images = StudioImages::default();
        let thumb_id = id(1);
        images.insert_thumb(thumb_id, dummy(64));
        for n in 2..40 {
            images.insert_display(id(n), dummy(2 * 1024 * 1024));
        }
        images.evict(&HashSet::new());
        assert!(images.contains_thumb(&thumb_id));
        assert!(images.displays.len() <= DISPLAY_CACHE_MAX);
    }

    #[test]
    fn a_cached_original_is_not_a_feed_display() {
        let mut images = StudioImages::default();
        let id = id(9);
        images.insert_full(id, dummy(1024));
        assert!(!images.contains_display(&id));
        assert!(images.get_display(&id).is_none());
    }
}
