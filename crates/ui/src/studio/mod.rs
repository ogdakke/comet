//! First-release Studio viewport and provider settings.
//!
//! - [`page`] — conversation load/submit and the feed/lightbox outlet;
//! - [`feed`] — tiles, turns, and the tick rail;
//! - [`composer`] — model picker, per-model controls, and generate;
//! - [`artifact`] — full-bleed image strip, filmstrip, and inspector;
//! - [`providers`] — connect / validate / remove image accounts;
//! - [`draft`] — per-model draft settings and control chrome;
//! - [`defaults`] — sticky last-used models and settings.

mod artifact;
mod composer;
mod defaults;
mod draft;
mod feed;
mod page;
mod providers;

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
}
