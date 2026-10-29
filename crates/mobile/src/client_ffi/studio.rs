//! Studio over FFI: JSON calls and streams to a host engine's Studio (see
//! `zeron_client::studio`). The app decodes the replies into its own models.

use std::sync::Arc;

use zeron_client as zc;

use super::types::CoreResult;
use super::{CoreClient, on_runtime};

/// Receives one Studio stream. Called on a client thread.
#[uniffi::export(with_foreign)]
pub trait StudioStreamSink: Send + Sync {
    /// The next item, as JSON.
    fn on_item(&self, json: String);
    /// The stream ended: `None` when the host closed it, else why it failed.
    fn on_end(&self, error: Option<String>);
}

struct SinkBridge(Arc<dyn StudioStreamSink>);

impl zc::StudioSink for SinkBridge {
    fn item(&self, json: String) {
        self.0.on_item(json);
    }

    fn ended(&self, error: Option<String>) {
        self.0.on_end(error);
    }
}

/// A live Studio stream; `cancel()` (or dropping it) stops it.
#[derive(uniffi::Object)]
pub struct StudioStream {
    watch: std::sync::Mutex<Option<zc::StudioWatch>>,
}

#[uniffi::export]
impl StudioStream {
    pub fn cancel(&self) {
        self.watch.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

#[uniffi::export]
impl CoreClient {
    /// One Studio RPC on `device_id`'s engine. `params_json` is a JSON
    /// object; returns the reply's JSON.
    pub async fn studio_call(
        &self,
        device_id: String,
        method: String,
        params_json: String,
    ) -> CoreResult<String> {
        let client = self.client.clone();
        on_runtime(async move { client.studio_call(&device_id, &method, &params_json).await }).await
    }

    /// Follow a `WatchStudio*` stream on `device_id`'s engine.
    pub fn studio_watch(
        &self,
        device_id: String,
        method: String,
        params_json: String,
        sink: Arc<dyn StudioStreamSink>,
    ) -> CoreResult<Arc<StudioStream>> {
        let watch = self.client.watch_studio(
            &device_id,
            &method,
            &params_json,
            Arc::new(SinkBridge(sink)),
        )?;
        Ok(Arc::new(StudioStream {
            watch: std::sync::Mutex::new(Some(watch)),
        }))
    }
}
