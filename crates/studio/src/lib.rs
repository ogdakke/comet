//! Provider-neutral media generation contracts for Zeron Studio.
//!
//! Provider wire types belong in adapter modules. The types exported here are the stable boundary
//! consumed by the engine, RPC layer, and UI.

mod catalog;
mod composer;
mod mime;
mod model;
mod probe;
mod provider;
mod request;

pub mod fake;
pub mod venice;
pub mod venice_overlay;
mod venice_provider;
mod venice_video;

pub use catalog::{CapabilityIntersection, intersect_video_globals, picker_models};
pub use composer::*;
pub use fake::{FakeMediaProvider, FakeSubmissionMode};
pub use mime::{accepted_output_mime, sniff_media_mime};
pub use model::*;
pub use probe::{MediaProbe, probe_media};
pub use provider::*;
pub use request::*;
pub use venice_provider::VeniceMediaProvider;
