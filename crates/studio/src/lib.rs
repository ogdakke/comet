//! Provider-neutral media generation contracts for Zeron Studio.
//!
//! Provider wire types belong in adapter modules. The types exported here are the stable boundary
//! consumed by the engine, RPC layer, and UI.

mod model;
mod provider;
mod request;

pub mod fake;
pub mod venice;
mod venice_provider;

pub use fake::{FakeMediaProvider, FakeSubmissionMode};
pub use model::*;
pub use provider::*;
pub use request::*;
pub use venice_provider::VeniceMediaProvider;
