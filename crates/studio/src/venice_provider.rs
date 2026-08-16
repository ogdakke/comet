//! Native Venice image provider adapter.

use async_trait::async_trait;
use base64::Engine as _;
use futures::StreamExt;
use reqwest::{Client, Response, StatusCode};
use serde::Deserialize;

use crate::{
    CancelResult, ControlValue, GenerationRequest, MediaKind, MediaOperation, MediaProvider,
    PollResult, ProviderAccount, ProviderAccountId, ProviderArtifact, ProviderError,
    ProviderErrorKind, ProviderId, ProviderResult, Quote, RemoteAttempt, RemoteJob, Secret,
    Submission, SubmissionCapabilities, SubmitContext,
    venice::{VENICE_PROVIDER_ID, normalize_model_catalog},
};

const DEFAULT_BASE_URL: &str = "https://api.venice.ai/api/v1";
const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;
const MAX_IMAGE_RESPONSE_BYTES: usize = 256 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub struct VeniceMediaProvider {
    client: Client,
    base_url: String,
}

impl VeniceMediaProvider {
    pub fn new() -> Result<Self, ProviderError> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(180))
            .build()
            .map_err(|error| ProviderError::new(ProviderErrorKind::Other, error.to_string()))?;
        Ok(Self {
            client,
            base_url: DEFAULT_BASE_URL.into(),
        })
    }

    #[cfg(test)]
    fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.into(),
        }
    }

    fn authenticated(
        &self,
        method: reqwest::Method,
        path: &str,
        secret: &Secret,
    ) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base_url))
            .bearer_auth(secret.expose())
    }

    async fn models(&self, secret: &Secret) -> ProviderResult<Vec<crate::MediaModel>> {
        let response = self
            .authenticated(reqwest::Method::GET, "/models?type=image", secret)
            .send()
            .await
            .map_err(network_error)?;
        let response = require_success(response).await?;
        let bytes = read_limited(response, MAX_CATALOG_BYTES).await?;
        normalize_model_catalog(&bytes, chrono::Utc::now()).map_err(|error| {
            ProviderError::new(ProviderErrorKind::MalformedResponse, error.to_string())
        })
    }
}

#[async_trait]
impl MediaProvider for VeniceMediaProvider {
    fn id(&self) -> ProviderId {
        VENICE_PROVIDER_ID.into()
    }

    fn submission_capabilities(&self) -> SubmissionCapabilities {
        SubmissionCapabilities {
            accepts_idempotency_key: false,
            can_reconcile: false,
            supports_cancellation: false,
        }
    }

    async fn validate_credentials(&self, secret: &Secret) -> ProviderResult<ProviderAccount> {
        self.models(secret).await?;
        Ok(ProviderAccount {
            id: ProviderAccountId::new(VENICE_PROVIDER_ID),
            label: "Venice AI".into(),
        })
    }

    async fn list_models(&self, secret: &Secret) -> ProviderResult<Vec<crate::MediaModel>> {
        self.models(secret).await
    }

    async fn quote(
        &self,
        secret: &Secret,
        request: &GenerationRequest,
    ) -> ProviderResult<Option<Quote>> {
        if !matches!(
            request.operation,
            MediaOperation::TextToVideo
                | MediaOperation::ImageToVideo
                | MediaOperation::ReferenceToVideo
                | MediaOperation::VideoToVideo
        ) {
            return Ok(None);
        }
        let payload = video_quote_payload(request)?;
        let response = self
            .authenticated(reqwest::Method::POST, "/video/quote", secret)
            .json(&payload)
            .send()
            .await
            .map_err(network_error)?;
        let response = require_success(response).await?;
        let bytes = read_limited(response, MAX_ERROR_BYTES).await?;
        let quoted: VideoQuoteResponse = serde_json::from_slice(&bytes).map_err(|error| {
            ProviderError::new(ProviderErrorKind::MalformedResponse, error.to_string())
        })?;
        if !quoted.quote.is_finite() || quoted.quote < 0.0 {
            return Err(ProviderError::new(
                ProviderErrorKind::MalformedResponse,
                "Venice returned a non-finite video quote",
            ));
        }
        Ok(Some(Quote::provider("USD", quoted.quote)))
    }

    async fn submit(
        &self,
        secret: &Secret,
        request: &GenerationRequest,
        _context: &SubmitContext,
    ) -> ProviderResult<Submission> {
        if request.operation != crate::MediaOperation::TextToImage || !request.inputs.is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::Unsupported,
                "this Venice adapter slice supports text-to-image without inputs",
            ));
        }
        let binary = request.output_count == 1;
        let payload = image_payload(request, binary)?;
        let response = self
            .authenticated(reqwest::Method::POST, "/image/generate", secret)
            .json(&payload)
            .send()
            .await
            .map_err(network_error)?;
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let moderation = ModerationFlags::from_headers(response.headers());
        let response = require_success(response).await?;
        let bytes = read_limited(response, MAX_IMAGE_RESPONSE_BYTES).await?;

        let artifacts = if binary {
            if !content_type.starts_with("image/") {
                return Err(ProviderError::new(
                    ProviderErrorKind::MalformedResponse,
                    format!("Venice returned {content_type} for a binary image request"),
                ));
            }
            vec![image_artifact(bytes, moderation.metadata(None))?]
        } else {
            if content_type != "application/json" {
                return Err(ProviderError::new(
                    ProviderErrorKind::MalformedResponse,
                    format!("Venice returned {content_type} for a variant image request"),
                ));
            }
            let response: ImageResponse = serde_json::from_slice(&bytes).map_err(|error| {
                ProviderError::new(ProviderErrorKind::MalformedResponse, error.to_string())
            })?;
            if response.images.len() != request.output_count as usize {
                return Err(ProviderError::new(
                    ProviderErrorKind::MalformedResponse,
                    "Venice returned an unexpected number of images",
                ));
            }
            response
                .images
                .into_iter()
                .map(|encoded| {
                    let encoded = encoded
                        .rsplit_once(',')
                        .map_or(encoded.as_str(), |(_, data)| data);
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .map_err(|error| {
                            ProviderError::new(
                                ProviderErrorKind::MalformedResponse,
                                error.to_string(),
                            )
                        })?;
                    image_artifact(bytes, moderation.metadata(Some(response.id.as_str())))
                })
                .collect::<ProviderResult<Vec<_>>>()?
        };
        Ok(Submission::Completed { artifacts })
    }

    async fn reconcile(
        &self,
        _secret: &Secret,
        _attempt: &RemoteAttempt,
    ) -> ProviderResult<Option<Submission>> {
        Ok(None)
    }

    async fn poll(&self, _secret: &Secret, _remote_job: &RemoteJob) -> ProviderResult<PollResult> {
        Err(ProviderError::new(
            ProviderErrorKind::Unsupported,
            "Venice image generation is synchronous",
        ))
    }

    async fn cancel(
        &self,
        _secret: &Secret,
        _remote_job: &RemoteJob,
    ) -> ProviderResult<CancelResult> {
        Ok(CancelResult::Unsupported)
    }
}

fn video_quote_payload(request: &GenerationRequest) -> ProviderResult<serde_json::Value> {
    let mut payload = serde_json::Map::from_iter([(
        "model".into(),
        serde_json::Value::String(request.model_id.as_str().into()),
    )]);
    for (id, value) in &request.controls {
        match (id.as_str(), value) {
            ("duration", ControlValue::DurationSeconds { value }) => {
                let duration = if value.fract() == 0.0 {
                    format!("{}s", *value as i64)
                } else {
                    format!("{value}s")
                };
                payload.insert("duration".into(), duration.into());
            }
            ("resolution", ControlValue::Resolution { value }) => {
                payload.insert("resolution".into(), value.clone().into());
            }
            ("aspect_ratio", ControlValue::AspectRatio { width, height }) => {
                payload.insert("aspect_ratio".into(), format!("{width}:{height}").into());
            }
            ("aspect_ratio", ControlValue::AspectRatioAuto) => {
                payload.insert("aspect_ratio".into(), "auto".into());
            }
            ("audio", ControlValue::Boolean { value }) => {
                payload.insert("audio".into(), (*value).into());
            }
            _ => {}
        }
    }
    if !payload.contains_key("duration") {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "video quote requires a duration",
        ));
    }
    Ok(payload.into())
}

#[derive(Deserialize)]
struct VideoQuoteResponse {
    quote: f64,
}

fn image_payload(request: &GenerationRequest, binary: bool) -> ProviderResult<serde_json::Value> {
    let mut payload = serde_json::Map::from_iter([
        (
            "model".into(),
            serde_json::Value::String(request.model_id.as_str().into()),
        ),
        (
            "prompt".into(),
            serde_json::Value::String(request.prompt.clone()),
        ),
        ("return_binary".into(), serde_json::Value::Bool(binary)),
        // Venice defaults `safe_mode` to true and returns a blurred placeholder
        // for adult-classified images. Send false unless the job asked otherwise.
        ("safe_mode".into(), serde_json::Value::Bool(false)),
    ]);
    if !binary {
        payload.insert("variants".into(), request.output_count.into());
    }
    if let Some(negative) = &request.negative_prompt {
        payload.insert("negative_prompt".into(), negative.clone().into());
    }
    for (id, value) in &request.controls {
        let wire = match (id.as_str(), value) {
            ("steps" | "seed", ControlValue::Integer { value }) => (*value).into(),
            ("cfg_scale", ControlValue::Number { value }) => (*value).into(),
            ("format" | "quality", ControlValue::Enum { value }) => value.clone().into(),
            ("resolution", ControlValue::Resolution { value }) => value.clone().into(),
            ("aspect_ratio", ControlValue::AspectRatio { width, height }) => {
                format!("{width}:{height}").into()
            }
            ("aspect_ratio", ControlValue::AspectRatioAuto) => "auto".into(),
            ("reasoning", ControlValue::Boolean { value }) => {
                payload.insert(
                    "disable_prompt_optimization_thinking".into(),
                    (!value).into(),
                );
                continue;
            }
            (
                "safe_mode"
                | "hide_watermark"
                | "embed_exif_metadata"
                | "enable_web_search"
                | "disable_prompt_optimization_thinking"
                | "enhance_prompt",
                ControlValue::Boolean { value },
            ) => (*value).into(),
            (unknown, _) => {
                return Err(ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    format!("unsupported Venice image control {unknown}"),
                ));
            }
        };
        payload.insert(id.as_str().into(), wire);
    }
    Ok(payload.into())
}

const PERSISTABLE_IMAGE_MIMES: &[&str] = &["image/webp", "image/png", "image/jpeg"];

fn image_artifact(bytes: Vec<u8>, metadata: serde_json::Value) -> ProviderResult<ProviderArtifact> {
    let mime_type =
        crate::accepted_output_mime(&bytes, PERSISTABLE_IMAGE_MIMES).ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::MalformedResponse,
                "Venice image bytes are not a supported format",
            )
        })?;
    Ok(ProviderArtifact {
        media_kind: MediaKind::Image,
        mime_type,
        bytes,
        width: None,
        height: None,
        duration_seconds: None,
        metadata,
    })
}

#[derive(Deserialize)]
struct ImageResponse {
    id: String,
    images: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ModerationFlags {
    blurred: Option<bool>,
    content_violation: Option<bool>,
}

impl ModerationFlags {
    fn from_headers(headers: &reqwest::header::HeaderMap) -> Self {
        Self {
            blurred: header_bool(headers, "x-venice-is-blurred"),
            content_violation: header_bool(headers, "x-venice-is-content-violation"),
        }
    }

    fn metadata(self, request_id: Option<&str>) -> serde_json::Value {
        let mut metadata = serde_json::Map::new();
        if let Some(request_id) = request_id {
            metadata.insert("requestId".into(), request_id.into());
        }
        if let Some(blurred) = self.blurred {
            metadata.insert("blurred".into(), blurred.into());
        }
        if let Some(content_violation) = self.content_violation {
            metadata.insert("contentViolation".into(), content_violation.into());
        }
        if metadata.is_empty() {
            serde_json::Value::Null
        } else {
            metadata.into()
        }
    }
}

fn header_bool(headers: &reqwest::header::HeaderMap, name: &str) -> Option<bool> {
    let value = headers.get(name)?.to_str().ok()?.trim();
    match value {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

async fn require_success(response: Response) -> ProviderResult<Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let retry_after_seconds = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok());
    let body = read_limited(response, MAX_ERROR_BYTES)
        .await
        .unwrap_or_default();
    let message = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| format!("Venice request failed with HTTP {status}"));
    let kind = match status {
        StatusCode::UNAUTHORIZED => ProviderErrorKind::InvalidCredential,
        StatusCode::PAYMENT_REQUIRED => ProviderErrorKind::InsufficientFunds,
        StatusCode::TOO_MANY_REQUESTS => ProviderErrorKind::RateLimited,
        StatusCode::BAD_REQUEST | StatusCode::UNSUPPORTED_MEDIA_TYPE => {
            ProviderErrorKind::InvalidRequest
        }
        status if status.is_server_error() => ProviderErrorKind::Transient,
        _ => ProviderErrorKind::Other,
    };
    let mut error = ProviderError::new(kind, message);
    error.retry_after_seconds = retry_after_seconds;
    error.provider_code = Some(status.as_u16().to_string());
    Err(error)
}

async fn read_limited(response: Response, maximum: usize) -> ProviderResult<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        return Err(ProviderError::new(
            ProviderErrorKind::ResponseTooLarge,
            "Venice response exceeds the configured limit",
        ));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(network_error)?;
        if bytes.len().saturating_add(chunk.len()) > maximum {
            return Err(ProviderError::new(
                ProviderErrorKind::ResponseTooLarge,
                "Venice response exceeds the configured limit",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn network_error(error: reqwest::Error) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Transient,
        format!("Venice network request failed: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn request() -> GenerationRequest {
        GenerationRequest {
            provider_id: VENICE_PROVIDER_ID.into(),
            model_id: "image-model".into(),
            operation: crate::MediaOperation::TextToImage,
            prompt: "a comet".into(),
            negative_prompt: None,
            output_count: 1,
            controls: BTreeMap::from([
                (
                    "aspect_ratio".into(),
                    ControlValue::AspectRatio {
                        width: 16,
                        height: 9,
                    },
                ),
                (
                    "format".into(),
                    ControlValue::Enum {
                        value: "png".into(),
                    },
                ),
                ("reasoning".into(), ControlValue::Boolean { value: true }),
            ]),
            inputs: Vec::new(),
            manifest_version: "v1".into(),
            display_aspect_ratio: (16, 9),
        }
    }

    #[test]
    fn semantic_controls_translate_to_native_venice_fields() {
        let value = image_payload(&request(), true).unwrap();
        assert_eq!(value["aspect_ratio"], "16:9");
        assert_eq!(value["format"], "png");
        assert_eq!(value["disable_prompt_optimization_thinking"], false);
        assert_eq!(value["return_binary"], true);
        assert_eq!(value["safe_mode"], false);
        assert!(value.get("variants").is_none());
    }

    #[test]
    fn omitted_safe_mode_disables_venice_adult_content_blur() {
        let value = image_payload(&request(), true).unwrap();
        assert_eq!(value["safe_mode"], false);
    }

    #[test]
    fn explicit_safe_mode_is_forwarded() {
        let mut request = request();
        request
            .controls
            .insert("safe_mode".into(), ControlValue::Boolean { value: true });
        let value = image_payload(&request, true).unwrap();
        assert_eq!(value["safe_mode"], true);
    }

    #[test]
    fn auto_aspect_ratio_is_forwarded() {
        let mut request = request();
        request
            .controls
            .insert("aspect_ratio".into(), ControlValue::AspectRatioAuto);
        let value = image_payload(&request, true).unwrap();
        assert_eq!(value["aspect_ratio"], "auto");
    }

    #[test]
    fn moderation_headers_are_recorded_on_artifacts() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-venice-is-blurred", "true".parse().unwrap());
        headers.insert("x-venice-is-content-violation", "false".parse().unwrap());
        let metadata = ModerationFlags::from_headers(&headers).metadata(Some("req-1"));
        assert_eq!(
            metadata,
            serde_json::json!({
                "requestId": "req-1",
                "blurred": true,
                "contentViolation": false,
            })
        );
    }

    #[test]
    fn unknown_controls_fail_closed() {
        let mut request = request();
        request
            .controls
            .insert("mystery".into(), ControlValue::Boolean { value: true });
        assert_eq!(
            image_payload(&request, true).unwrap_err().kind,
            ProviderErrorKind::InvalidRequest
        );
    }

    #[test]
    fn test_constructor_keeps_provider_calls_redirectable() {
        let provider = VeniceMediaProvider::with_base_url("http://127.0.0.1:1");
        assert_eq!(provider.id().as_str(), VENICE_PROVIDER_ID);
    }

    fn video_request() -> GenerationRequest {
        GenerationRequest {
            provider_id: VENICE_PROVIDER_ID.into(),
            model_id: "seedance-2-0-text-to-video-basic".into(),
            operation: crate::MediaOperation::TextToVideo,
            prompt: "a comet".into(),
            negative_prompt: None,
            output_count: 1,
            controls: BTreeMap::from([
                (
                    "duration".into(),
                    ControlValue::DurationSeconds { value: 10.0 },
                ),
                (
                    "resolution".into(),
                    ControlValue::Resolution {
                        value: "1080p".into(),
                    },
                ),
                (
                    "aspect_ratio".into(),
                    ControlValue::AspectRatio {
                        width: 16,
                        height: 9,
                    },
                ),
                ("audio".into(), ControlValue::Boolean { value: true }),
            ]),
            inputs: Vec::new(),
            manifest_version: "v1".into(),
            display_aspect_ratio: (16, 9),
        }
    }

    #[test]
    fn video_quote_payload_sends_pricing_inputs() {
        let value = video_quote_payload(&video_request()).unwrap();
        assert_eq!(value["model"], "seedance-2-0-text-to-video-basic");
        assert_eq!(value["duration"], "10s");
        assert_eq!(value["resolution"], "1080p");
        assert_eq!(value["aspect_ratio"], "16:9");
        assert_eq!(value["audio"], true);
    }

    #[test]
    fn video_quote_requires_duration() {
        let mut request = video_request();
        request.controls.remove(&crate::ControlId::from("duration"));
        assert_eq!(
            video_quote_payload(&request).unwrap_err().kind,
            ProviderErrorKind::InvalidRequest
        );
    }

    #[test]
    fn image_bytes_are_labeled_from_magic_not_requested_format() {
        let jpeg = vec![0xff, 0xd8, 0xff, 0xdb, 1, 2, 3];
        let artifact = image_artifact(jpeg.clone(), serde_json::Value::Null).unwrap();
        assert_eq!(artifact.mime_type, "image/jpeg");
        assert_eq!(artifact.bytes, jpeg);

        let png = b"\x89PNG\r\n\x1a\nrest".to_vec();
        let artifact = image_artifact(png, serde_json::Value::Null).unwrap();
        assert_eq!(artifact.mime_type, "image/png");

        assert_eq!(
            image_artifact(b"not-an-image".to_vec(), serde_json::Value::Null)
                .unwrap_err()
                .kind,
            ProviderErrorKind::MalformedResponse
        );
    }

    #[tokio::test]
    async fn image_quote_is_catalog_only() {
        let provider = VeniceMediaProvider::with_base_url("http://127.0.0.1:1");
        let quoted = provider
            .quote(&Secret::new("token"), &request())
            .await
            .unwrap();
        assert!(quoted.is_none());
    }

    #[tokio::test]
    async fn video_quote_reads_the_provider_amount() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut incoming = vec![0_u8; 4096];
            let _ = stream.read(&mut incoming).await;
            let body = r#"{"quote":0.085}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
        let provider = VeniceMediaProvider::with_base_url(format!("http://{addr}"));
        let quoted = provider
            .quote(&Secret::new("token"), &video_request())
            .await
            .unwrap()
            .expect("venice video quote");
        assert_eq!(quoted.source, crate::QuoteSource::Provider);
        assert_eq!(quoted.currency, "USD");
        assert!((quoted.amount - 0.085).abs() < f64::EPSILON);
        server.await.unwrap();
    }
}
