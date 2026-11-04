//! The edge's `/xfer` routes, with the caller's bearer.

use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

use crate::{Manifest, TransferError, valid_id};

/// The account's current bearer, read fresh for every request so a refresh
/// in between applies.
#[async_trait::async_trait]
pub trait Bearer: Send + Sync {
    async fn bearer(&self) -> Result<String, TransferError>;
}

/// A create's answer: the stored manifest and what has already arrived.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Created {
    pub manifest: Manifest,
    pub present: Vec<usize>,
    pub done: bool,
    /// Write token (48 h): what an uploader that can't hold the bearer uses.
    pub token: String,
    /// Read token (24 h): for URL-only openers.
    pub read_token: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    /// `None` only for a cancelled transfer whose parts are gone.
    pub manifest: Option<Manifest>,
    #[serde(default)]
    pub present: Vec<usize>,
    pub last_part_at: Option<i64>,
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub cancelled: bool,
}

#[derive(Clone)]
pub struct EdgeClient {
    http: reqwest::Client,
    base: String,
    bearer: Arc<dyn Bearer>,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: Option<String>,
    message: Option<String>,
}

impl EdgeClient {
    pub fn new(http: reqwest::Client, base: impl Into<String>, bearer: Arc<dyn Bearer>) -> Self {
        Self {
            http,
            base: base.into().trim_end_matches('/').to_owned(),
            bearer,
        }
    }

    fn url(&self, chat_id: &str, transfer_id: &str, tail: &str) -> Result<String, TransferError> {
        for id in [chat_id, transfer_id] {
            if !valid_id(id) {
                return Err(TransferError::BadId(id.to_owned()));
            }
        }
        Ok(format!("{}/xfer/{chat_id}/{transfer_id}{tail}", self.base))
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, TransferError> {
        let response = request
            .bearer_auth(self.bearer.bearer().await?)
            .send()
            .await?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let text = response.text().await.unwrap_or_default();
        let body: Option<ErrorBody> = serde_json::from_str(&text).ok();
        let message = body
            .as_ref()
            .and_then(|b| b.message.clone().or_else(|| b.error.clone()))
            .unwrap_or_else(|| text.chars().take(200).collect());
        Err(match status.as_u16() {
            401 => TransferError::Unauthorized,
            404 => TransferError::NotFound,
            410 => TransferError::Cancelled,
            413 => TransferError::TooLarge,
            429 | 500..=599 => TransferError::Unavailable {
                status: status.as_u16(),
                message,
            },
            _ => TransferError::Refused(message),
        })
    }

    /// Create the transfer, or answer the existing one with the same manifest.
    pub async fn create(
        &self,
        chat_id: &str,
        transfer_id: &str,
        manifest: &Manifest,
    ) -> Result<Created, TransferError> {
        let url = self.url(chat_id, transfer_id, "")?;
        Ok(self
            .send(self.http.post(url).json(manifest))
            .await?
            .json()
            .await?)
    }

    /// `None` when there is no such transfer.
    pub async fn status(
        &self,
        chat_id: &str,
        transfer_id: &str,
    ) -> Result<Option<Status>, TransferError> {
        let url = self.url(chat_id, transfer_id, "")?;
        match self.send(self.http.get(url)).await {
            Ok(response) => Ok(Some(response.json().await?)),
            Err(TransferError::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Send part `n`: `len` bytes of `path` from `offset`, streamed.
    pub async fn put_part(
        &self,
        chat_id: &str,
        transfer_id: &str,
        n: usize,
        path: &Path,
        offset: u64,
        len: u64,
    ) -> Result<(), TransferError> {
        let url = self.url(chat_id, transfer_id, &format!("/p/{n}"))?;
        let mut file = tokio::fs::File::open(path).await?;
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file.take(len)));
        let request = self
            .http
            .put(url)
            .header(reqwest::header::CONTENT_LENGTH, len)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(body);
        self.send(request).await.map(drop)
    }

    /// Seal the transfer once every part is in (nudging its host, if any).
    pub async fn done(&self, chat_id: &str, transfer_id: &str) -> Result<(), TransferError> {
        let url = self.url(chat_id, transfer_id, "/done")?;
        self.send(self.http.post(url)).await.map(drop)
    }

    /// Part `n` as a byte stream.
    pub async fn get_part(
        &self,
        chat_id: &str,
        transfer_id: &str,
        n: usize,
    ) -> Result<reqwest::Response, TransferError> {
        let url = self.url(chat_id, transfer_id, &format!("/p/{n}"))?;
        self.send(self.http.get(url)).await
    }

    /// Remove the transfer; `cancel` leaves a tombstone the receiver reads as
    /// "the sender gave up" instead of "not uploaded yet".
    pub async fn delete(
        &self,
        chat_id: &str,
        transfer_id: &str,
        cancel: bool,
    ) -> Result<(), TransferError> {
        let url = self.url(chat_id, transfer_id, if cancel { "?cancel=1" } else { "" })?;
        self.send(self.http.delete(url)).await.map(drop)
    }

    /// The `/content` URL a URL-only opener (a video player, QuickLook) can
    /// read with `read_token`.
    pub fn content_url(
        &self,
        chat_id: &str,
        transfer_id: &str,
        read_token: &str,
    ) -> Result<String, TransferError> {
        Ok(format!(
            "{}?t={read_token}",
            self.url(chat_id, transfer_id, "/content")?
        ))
    }
}
