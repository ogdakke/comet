//! Studio (generated images and video) on a host engine. Its RPCs pass
//! through as JSON — the app owns the wire models — and are limited to the
//! Studio methods, so this can't become a general relay escape hatch.
//!
//! [`Client::studio_call`] is one unary call; [`Client::watch_studio`]
//! follows a `WatchStudio*` stream until the watch is dropped or the stream
//! ends (the app reopens it; watches replay their current state on open).

use std::sync::Arc;

use serde_json::Value;
use tokio_util::sync::CancellationToken;
use zeron_rpc::methods as m;

use crate::client::Client;
use crate::error::{ClientError, Result};

const CALLS: &[&str] = &[
    m::LIST_STUDIO_PROVIDERS,
    m::SET_STUDIO_PROVIDER_CREDENTIAL,
    m::REMOVE_STUDIO_PROVIDER_CREDENTIAL,
    m::VALIDATE_STUDIO_PROVIDER,
    m::SET_STUDIO_PROVIDER_PREFERENCES,
    m::LIST_STUDIO_MODELS,
    m::LIST_STUDIO_CONVERSATIONS,
    m::LIST_STUDIO_ARTIFACTS,
    m::CREATE_STUDIO_CONVERSATION,
    m::RENAME_STUDIO_CONVERSATION,
    m::ARCHIVE_STUDIO_CONVERSATION,
    m::DELETE_STUDIO_CONVERSATION,
    m::MARK_STUDIO_CONVERSATION_SEEN,
    m::CREATE_STUDIO_TURN,
    m::EXTEND_STUDIO_TURN,
    m::APPEND_STUDIO_DERIVED_RUN,
    m::QUOTE_STUDIO_BATCH,
    m::EVALUATE_STUDIO_COMPOSER,
    m::IMPORT_STUDIO_ASSET,
    m::GET_STUDIO_PROVIDER_BALANCE,
    m::RETRY_STUDIO_RUN,
    m::CHECK_STUDIO_RUN,
    m::DELETE_STUDIO_ARTIFACT,
    m::READ_STUDIO_ARTIFACT_CHUNK,
    m::READ_STUDIO_PREVIEW_CHUNK,
];

const WATCHES: &[&str] = &[
    m::WATCH_STUDIO_CONVERSATIONS,
    m::WATCH_STUDIO_GALLERY,
    m::WATCH_STUDIO_CONVERSATION,
];

/// Receives one Studio stream. Called on a client thread.
pub trait StudioSink: Send + Sync {
    /// The next item, as JSON.
    fn item(&self, json: String);
    /// The stream ended: `None` when the host closed it, else why it failed.
    fn ended(&self, error: Option<String>);
}

/// Following one Studio stream; dropping it stops (host-side too).
pub struct StudioWatch {
    cancel: CancellationToken,
}

impl Drop for StudioWatch {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Client {
    /// One Studio RPC on `device_id`'s engine; `params` and the reply are JSON.
    pub async fn studio_call(&self, device_id: &str, method: &str, params: &str) -> Result<String> {
        if !CALLS.contains(&method) {
            return Err(ClientError::InvalidArgument(format!(
                "{method} isn't a Studio call"
            )));
        }
        let params = parse(params)?;
        let reply = self.inner.host_rpc(device_id, method, params).await?;
        Ok(reply.to_string())
    }

    /// Follow a `WatchStudio*` stream on `device_id`'s engine.
    pub fn watch_studio(
        &self,
        device_id: &str,
        method: &str,
        params: &str,
        sink: Arc<dyn StudioSink>,
    ) -> Result<StudioWatch> {
        if !WATCHES.contains(&method) {
            return Err(ClientError::InvalidArgument(format!(
                "{method} isn't a Studio watch"
            )));
        }
        let params = parse(params)?;
        let cancel = CancellationToken::new();
        let weak = Arc::downgrade(&self.inner);
        let (device_id, method, token) = (device_id.to_owned(), method.to_owned(), cancel.clone());
        crate::runtime::shared().spawn(async move {
            let open = async {
                let inner = weak.upgrade().ok_or(ClientError::Closed)?;
                inner.host_subscribe(&device_id, &method, params).await
            };
            let mut stream = tokio::select! {
                _ = token.cancelled() => return,
                stream = open => match stream {
                    Ok(stream) => stream,
                    Err(err) => return sink.ended(Some(err.to_string())),
                },
            };
            loop {
                let item = tokio::select! {
                    _ = token.cancelled() => return,
                    item = stream.recv() => item,
                };
                match item {
                    Some(item) => sink.item(item.to_string()),
                    None => return sink.ended(None),
                }
            }
        });
        Ok(StudioWatch { cancel })
    }
}

fn parse(params: &str) -> Result<Value> {
    if params.trim().is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    serde_json::from_str(params)
        .map_err(|err| ClientError::InvalidArgument(format!("Studio params: {err}")))
}
