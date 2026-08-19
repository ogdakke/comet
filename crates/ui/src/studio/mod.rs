//! First-release Studio viewport and provider settings.
//!
//! - [`page`] — conversation load/submit and the feed/lightbox outlet;
//! - [`gallery`] — profile-wide image grid, multi-select, and bulk actions;
//! - [`feed`] — tiles, turns, the tick rail, and drag-to-composer references;
//! - [`composer`] — model picker, per-model controls, and generate;
//! - [`tray`] — reference attach, ImportStudioAsset, budgets, Make video;
//! - [`artifact`] — full-bleed image/video strip, filmstrip, and inspector;
//! - [`video`] — in-lightbox playback and OS-player fallback;
//! - [`image_menu`] — right-click / two-finger-tap actions on visible images;
//! - [`upscale`] — artifact-viewer upscale action, settings, and completion;
//! - [`providers`] — connect / validate / remove image accounts;
//! - [`draft`] — per-model draft settings and control chrome;
//! - [`defaults`] — sticky last-used models and settings.

mod artifact;
mod composer;
mod conflict;
mod cost;
mod defaults;
mod draft;
mod edit;
mod feed;
mod gallery;
mod image_menu;
mod images;
mod lineage;
mod page;
mod paint;
mod providers;
mod tray;
mod upscale;
mod video;

pub use feed::grid_columns;
pub use page::StudioPage;
pub use providers::ProvidersPage;

use zeron_studio::{StudioArtifactId, StudioConversationId};

#[derive(Clone, Debug)]
pub enum StudioEvent {
    OpenProviders,
    SidebarChanged,
    OpenArtifact {
        conversation_id: StudioConversationId,
        artifact_id: StudioArtifactId,
    },
    CloseArtifact,
    /// Navigate to a thread. `focus_artifact` is a one-shot scroll on this
    /// click; it is not stored on the thread's history entry.
    ShowThread {
        conversation_id: StudioConversationId,
        focus_artifact: Option<StudioArtifactId>,
    },
}
