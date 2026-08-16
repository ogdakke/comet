//! Bounded decoded-image cache for Studio. Gallery tiles keep small thumbs;
//! the lightbox keeps a handful of full-resolution frames.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::{App, Image, Window};
use zeron_studio::StudioArtifactId;

/// Encoded-byte budget across thumbs and full frames.
const IMAGE_CACHE_BUDGET_BYTES: usize = 128 * 1024 * 1024;
const IMAGE_CACHE_MAX_THUMBS: usize = 180;
const IMAGE_CACHE_MAX_FULL: usize = 36;

struct CachedImage {
    image: Arc<Image>,
    bytes: usize,
    last_used: u64,
}

#[derive(Default)]
pub(super) struct StudioImages {
    thumbs: HashMap<StudioArtifactId, CachedImage>,
    full: HashMap<StudioArtifactId, CachedImage>,
    tick: u64,
    loaded_bytes: usize,
    pending_free: Vec<Arc<Image>>,
}

impl StudioImages {
    pub(super) fn has_thumb_capacity(&self) -> bool {
        self.thumbs.len() < IMAGE_CACHE_MAX_THUMBS
    }

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

    pub(super) fn contains_full(&self, id: &StudioArtifactId) -> bool {
        self.full.contains_key(id)
    }

    pub(super) fn contains_thumb(&self, id: &StudioArtifactId) -> bool {
        self.thumbs.contains_key(id) || self.full.contains_key(id)
    }

    pub(super) fn insert_thumb(&mut self, id: StudioArtifactId, image: Arc<Image>) {
        if self.thumbs.contains_key(&id) {
            return;
        }
        self.insert_map(false, id, image);
    }

    pub(super) fn insert_full(&mut self, id: StudioArtifactId, image: Arc<Image>) {
        if self.full.contains_key(&id) {
            return;
        }
        self.insert_map(true, id, image);
    }

    fn insert_map(&mut self, full: bool, id: StudioArtifactId, image: Arc<Image>) {
        self.tick = self.tick.saturating_add(1);
        let bytes = image.bytes.len();
        let previous = if full {
            self.full.insert(
                id,
                CachedImage {
                    image,
                    bytes,
                    last_used: self.tick,
                },
            )
        } else {
            self.thumbs.insert(
                id,
                CachedImage {
                    image,
                    bytes,
                    last_used: self.tick,
                },
            )
        };
        if let Some(previous) = previous {
            self.loaded_bytes = self.loaded_bytes.saturating_sub(previous.bytes);
            self.pending_free.push(previous.image);
        }
        self.loaded_bytes = self.loaded_bytes.saturating_add(bytes);
    }

    pub(super) fn remove(&mut self, id: &StudioArtifactId) {
        if let Some(previous) = self.thumbs.remove(id) {
            self.loaded_bytes = self.loaded_bytes.saturating_sub(previous.bytes);
            self.pending_free.push(previous.image);
        }
        if let Some(previous) = self.full.remove(id) {
            self.loaded_bytes = self.loaded_bytes.saturating_sub(previous.bytes);
            self.pending_free.push(previous.image);
        }
    }

    pub(super) fn touch(&mut self, ids: impl IntoIterator<Item = StudioArtifactId>) {
        self.tick = self.tick.saturating_add(1);
        let tick = self.tick;
        for id in ids {
            if let Some(entry) = self.thumbs.get_mut(&id) {
                entry.last_used = tick;
            }
            if let Some(entry) = self.full.get_mut(&id) {
                entry.last_used = tick;
            }
        }
    }

    pub(super) fn evict(&mut self, protected: &HashSet<StudioArtifactId>) {
        while self.full.len() > IMAGE_CACHE_MAX_FULL
            || self.thumbs.len() > IMAGE_CACHE_MAX_THUMBS
            || self.loaded_bytes > IMAGE_CACHE_BUDGET_BYTES
        {
            let oldest_full = oldest_unprotected(&self.full, protected);
            let oldest_thumb = oldest_unprotected(&self.thumbs, protected);
            let evict_full = match (oldest_full, oldest_thumb) {
                (Some((_, used)), Some((_, thumb_used))) => {
                    self.full.len() > IMAGE_CACHE_MAX_FULL || used <= thumb_used
                }
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            if evict_full {
                let Some((id, _)) = oldest_full else { break };
                if let Some(previous) = self.full.remove(&id) {
                    self.loaded_bytes = self.loaded_bytes.saturating_sub(previous.bytes);
                    self.pending_free.push(previous.image);
                }
            } else {
                let Some((id, _)) = oldest_thumb else { break };
                if let Some(previous) = self.thumbs.remove(&id) {
                    self.loaded_bytes = self.loaded_bytes.saturating_sub(previous.bytes);
                    self.pending_free.push(previous.image);
                }
            }
        }
    }

    pub(super) fn flush(&mut self, window: Option<&mut Window>, cx: &mut App) {
        let evicted = std::mem::take(&mut self.pending_free);
        let mut window = window;
        for image in evicted {
            gpui::ImageSource::Image(image).evict(window.as_deref_mut(), cx);
        }
    }
}

fn oldest_unprotected(
    map: &HashMap<StudioArtifactId, CachedImage>,
    protected: &HashSet<StudioArtifactId>,
) -> Option<(StudioArtifactId, u64)> {
    map.iter()
        .filter(|(id, _)| !protected.contains(*id))
        .min_by_key(|(_, entry)| entry.last_used)
        .map(|(id, entry)| (*id, entry.last_used))
}
