use std::{fmt, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    GenerationRequest, MediaKind, MediaModel, ProviderAccountId, ProviderId, SubmitContext,
};

/// A provider credential. Its value is intentionally neither serializable nor printable.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret([REDACTED])")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmissionCapabilities {
    pub accepts_idempotency_key: bool,
    pub can_reconcile: bool,
    pub supports_cancellation: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAccount {
    pub id: ProviderAccountId,
    pub label: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuoteSource {
    #[default]
    Catalog,
    Provider,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Quote {
    pub currency: String,
    pub amount: f64,
    pub detail: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub source: QuoteSource,
}

impl Quote {
    pub fn catalog(currency: impl Into<String>, amount: f64) -> Self {
        Self {
            currency: currency.into(),
            amount,
            detail: None,
            expires_at: None,
            source: QuoteSource::Catalog,
        }
    }

    pub fn provider(currency: impl Into<String>, amount: f64) -> Self {
        Self {
            currency: currency.into(),
            amount,
            detail: None,
            expires_at: None,
            source: QuoteSource::Provider,
        }
    }

    pub fn saturating_sub(&self, other: &Self) -> Option<Self> {
        if self.currency != other.currency {
            return None;
        }
        Some(Self {
            currency: self.currency.clone(),
            amount: (self.amount - other.amount).max(0.0),
            detail: None,
            expires_at: None,
            source: self.source,
        })
    }

    pub fn saturating_add(&self, other: &Self) -> Option<Self> {
        if self.currency != other.currency {
            return None;
        }
        Some(Self {
            currency: self.currency.clone(),
            amount: self.amount + other.amount,
            detail: None,
            expires_at: None,
            source: self.source,
        })
    }

    /// Sum quotes that share a currency. Mixed currencies yield `None`.
    pub fn total(quotes: impl IntoIterator<Item = Self>) -> Option<Self> {
        let mut quotes = quotes.into_iter();
        let first = quotes.next()?;
        let mut amount = first.amount;
        let mut source = first.source;
        for quote in quotes {
            if quote.currency != first.currency {
                return None;
            }
            amount += quote.amount;
            if quote.source == QuoteSource::Provider {
                source = QuoteSource::Provider;
            }
        }
        Some(Self {
            currency: first.currency,
            amount,
            detail: None,
            expires_at: None,
            source,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteJob {
    pub id: String,
    /// Provider-neutral, bounded metadata required to poll or clean up the job.
    pub metadata: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteAttempt {
    pub idempotency_key: String,
    pub remote_job_id: Option<String>,
    pub request_wire_hash: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderArtifact {
    pub media_kind: MediaKind,
    pub mime_type: String,
    pub bytes: Vec<u8>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_seconds: Option<f64>,
    pub metadata: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Submission {
    Completed { artifacts: Vec<ProviderArtifact> },
    Queued { remote_job: RemoteJob },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PollResult {
    Queued { progress: Option<f32> },
    Running { progress: Option<f32> },
    Completed { artifacts: Vec<ProviderArtifact> },
    Failed { error: ProviderError },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelResult {
    Cancelled,
    AlreadyTerminal,
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    InvalidCredential,
    InsufficientFunds,
    RateLimited,
    InvalidRequest,
    ModelUnavailable,
    ResponseTooLarge,
    MalformedResponse,
    Transient,
    Unsupported,
    Other,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
    pub retry_after_seconds: Option<u64>,
    pub provider_code: Option<String>,
}

impl ProviderError {
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retry_after_seconds: None,
            provider_code: None,
        }
    }

    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after_seconds.map(Duration::from_secs)
    }
}

pub type ProviderResult<T> = Result<T, ProviderError>;

/// Spendable prepaid credit. Venice maps this to USD / bundled credits only —
/// staking allotments stay out of the studio chrome.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountBalance {
    pub remaining: Quote,
}

#[async_trait]
pub trait MediaProvider: Send + Sync {
    fn id(&self) -> ProviderId;
    fn submission_capabilities(&self) -> SubmissionCapabilities;
    async fn validate_credentials(&self, secret: &Secret) -> ProviderResult<ProviderAccount>;
    async fn list_models(&self, secret: &Secret) -> ProviderResult<Vec<MediaModel>>;
    async fn quote(
        &self,
        secret: &Secret,
        request: &GenerationRequest,
        reference_video_total_duration: Option<f64>,
    ) -> ProviderResult<Option<Quote>>;
    async fn balance(&self, _secret: &Secret) -> ProviderResult<Option<AccountBalance>> {
        Ok(None)
    }
    async fn submit(
        &self,
        secret: &Secret,
        request: &GenerationRequest,
        context: &SubmitContext,
    ) -> ProviderResult<Submission>;
    async fn reconcile(
        &self,
        secret: &Secret,
        attempt: &RemoteAttempt,
    ) -> ProviderResult<Option<Submission>>;
    async fn poll(&self, secret: &Secret, remote_job: &RemoteJob) -> ProviderResult<PollResult>;
    async fn cancel(&self, secret: &Secret, remote_job: &RemoteJob)
    -> ProviderResult<CancelResult>;
    /// Best-effort provider cleanup after a queued job is published locally.
    async fn complete(&self, secret: &Secret, remote_job: &RemoteJob) -> ProviderResult<()> {
        let _ = (secret, remote_job);
        Ok(())
    }
}
