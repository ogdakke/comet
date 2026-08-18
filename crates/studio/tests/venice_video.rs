use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use zeron_studio::{
    ControlValue, GenerationRequest, MediaKind, MediaOperation, MediaProvider, PollResult,
    ProviderErrorKind, Secret, Submission, SubmitContext, VeniceMediaProvider,
};

const QUEUE: &str = include_str!("fixtures/venice/video-queue.json");
const PROCESSING: &str = include_str!("fixtures/venice/video-processing.json");
const DOWNLOAD_URL: &str = include_str!("fixtures/venice/video-download-url.json");
const COMPLETED: &str = include_str!("fixtures/venice/video-completed.json");
const COMPLETE: &str = include_str!("fixtures/venice/video-complete.json");
const PAYMENT_REQUIRED: &str = include_str!("fixtures/venice/video-402.json");
const CONTENT_POLICY: &str = include_str!("fixtures/venice/video-422.json");
const CAPACITY: &str = include_str!("fixtures/venice/video-503.json");
const MP4: &[u8] = include_bytes!("fixtures/venice/video.mp4");

fn seedance_request() -> GenerationRequest {
    GenerationRequest {
        provider_id: "venice".into(),
        model_id: "seedance-1-5-pro-text-to-video-basic".into(),
        operation: MediaOperation::TextToVideo,
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

fn empty_context() -> SubmitContext {
    SubmitContext {
        idempotency_key: "attempt-1".into(),
        inputs: Vec::new(),
    }
}

struct RecordedRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

struct FixtureResponse {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl FixtureResponse {
    fn json(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.as_bytes().to_vec(),
        }
    }

    fn mp4() -> Self {
        Self {
            status: 200,
            content_type: "video/mp4",
            body: MP4.to_vec(),
        }
    }
}

async fn serve(
    replies: Vec<FixtureResponse>,
) -> (
    String,
    Arc<Mutex<Vec<RecordedRequest>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_server = seen.clone();
    let server = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        for reply in replies {
            let (mut stream, _) = listener.accept().await.unwrap();
            let recorded = read_http_request(&mut stream).await;
            seen_server.lock().unwrap().push(recorded);
            let reason = match reply.status {
                200 => "OK",
                402 => "Payment Required",
                404 => "Not Found",
                422 => "Unprocessable Entity",
                503 => "Service Unavailable",
                _ => "Error",
            };
            let extra = if reply.status == 503 {
                "Retry-After: 5\r\n"
            } else {
                ""
            };
            let header = format!(
                "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
                reply.status,
                reason,
                reply.content_type,
                reply.body.len()
            );
            let _ = stream.write_all(header.as_bytes()).await;
            let _ = stream.write_all(&reply.body).await;
        }
    });
    (format!("http://{addr}"), seen, server)
}

async fn read_http_request(stream: &mut tokio::net::TcpStream) -> RecordedRequest {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    loop {
        let mut chunk = [0_u8; 4096];
        let n = stream.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(header_end) = find_header_end(&buf) {
            let header = String::from_utf8_lossy(&buf[..header_end]);
            let mut lines = header.split("\r\n");
            let request_line = lines.next().unwrap_or("");
            let mut parts = request_line.split_whitespace();
            let method = parts.next().unwrap_or("").to_owned();
            let path = parts.next().unwrap_or("").to_owned();
            let content_length = header
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                    })
                })
                .flatten()
                .unwrap_or(0);
            let body_start = header_end + 4;
            while buf.len() < body_start + content_length {
                let mut chunk = [0_u8; 4096];
                let n = stream.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            return RecordedRequest {
                method,
                path,
                body: buf[body_start..body_start + content_length.min(buf.len() - body_start)]
                    .to_vec(),
            };
        }
    }
    RecordedRequest {
        method: String::new(),
        path: String::new(),
        body: buf,
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

fn json_body(request: &RecordedRequest) -> serde_json::Value {
    serde_json::from_slice(&request.body).unwrap_or(serde_json::Value::Null)
}

#[tokio::test]
async fn hidden_kling_is_not_queued() {
    let provider = VeniceMediaProvider::with_base_url("http://127.0.0.1:1");
    let mut request = seedance_request();
    request.model_id = "kling-o3-pro-reference-to-video".into();
    request.operation = MediaOperation::ReferenceToVideo;
    let error = provider
        .submit(&Secret::new("token"), &request, &empty_context())
        .await
        .unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Unsupported);
}

#[tokio::test]
async fn queue_fixture_returns_queued_job() {
    let (base, seen, server) = serve(vec![FixtureResponse::json(200, QUEUE)]).await;
    let provider = VeniceMediaProvider::with_base_url(base);
    let submission = provider
        .submit(&Secret::new("token"), &seedance_request(), &empty_context())
        .await
        .unwrap();
    let Submission::Queued { remote_job } = submission else {
        panic!("expected queued video job");
    };
    assert_eq!(remote_job.id, "123e4567-e89b-12d3-a456-426614174000");
    assert_eq!(
        remote_job.metadata["model"],
        "seedance-1-5-pro-text-to-video-basic"
    );
    assert!(remote_job.metadata.get("download_url").is_none());
    let recorded = seen.lock().unwrap();
    assert_eq!(recorded[0].method, "POST");
    assert_eq!(recorded[0].path, "/video/queue");
    let body = json_body(&recorded[0]);
    assert_eq!(body["duration"], "10s");
    assert_eq!(body["audio"], true);
    assert!(body.get("omni_reference_task_type").is_none());
    server.await.unwrap();
}

#[tokio::test]
async fn queue_402_is_insufficient_funds() {
    let (base, _, server) = serve(vec![FixtureResponse::json(402, PAYMENT_REQUIRED)]).await;
    let provider = VeniceMediaProvider::with_base_url(base);
    let error = provider
        .submit(&Secret::new("token"), &seedance_request(), &empty_context())
        .await
        .unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::InsufficientFunds);
    server.await.unwrap();
}

#[tokio::test]
async fn processing_fixture_reports_clamped_progress() {
    let (base, seen, server) = serve(vec![
        FixtureResponse::json(200, QUEUE),
        FixtureResponse::json(200, PROCESSING),
    ])
    .await;
    let provider = VeniceMediaProvider::with_base_url(base);
    let Submission::Queued { remote_job } = provider
        .submit(&Secret::new("token"), &seedance_request(), &empty_context())
        .await
        .unwrap()
    else {
        panic!("expected queued video job");
    };
    let PollResult::Running { progress } = provider
        .poll(&Secret::new("token"), &remote_job)
        .await
        .unwrap()
    else {
        panic!("expected running poll");
    };
    let progress = progress.expect("processing progress");
    assert!((progress - (53200.0 / 145000.0)).abs() < 1e-5);
    assert!(progress <= 0.99);
    let recorded = seen.lock().unwrap();
    assert_eq!(recorded[1].path, "/video/retrieve");
    let body = json_body(&recorded[1]);
    assert_eq!(body["model"], "seedance-1-5-pro-text-to-video-basic");
    assert_eq!(body["queue_id"], remote_job.id);
    server.await.unwrap();
}

#[tokio::test]
async fn mp4_retrieve_completes() {
    let (base, _, server) = serve(vec![
        FixtureResponse::json(200, QUEUE),
        FixtureResponse::mp4(),
    ])
    .await;
    let provider = VeniceMediaProvider::with_base_url(base);
    let Submission::Queued { remote_job } = provider
        .submit(&Secret::new("token"), &seedance_request(), &empty_context())
        .await
        .unwrap()
    else {
        panic!("expected queued video job");
    };
    let PollResult::Completed { artifacts } = provider
        .poll(&Secret::new("token"), &remote_job)
        .await
        .unwrap()
    else {
        panic!("expected completed mp4");
    };
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].media_kind, MediaKind::Video);
    assert_eq!(artifacts[0].mime_type, "video/mp4");
    assert_eq!(artifacts[0].bytes, MP4);
    server.await.unwrap();
}

#[tokio::test]
async fn completed_json_downloads_from_queue_url() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut queued: serde_json::Value = serde_json::from_str(DOWNLOAD_URL).unwrap();
    queued["download_url"] = format!("http://{addr}/venice/video-download").into();
    let queued = queued.to_string();
    let server = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        for (status, content_type, body) in [
            (200_u16, "application/json", queued.as_bytes().to_vec()),
            (200, "application/json", COMPLETED.as_bytes().to_vec()),
            (200, "video/mp4", MP4.to_vec()),
        ] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut stream).await;
            let header = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes()).await;
            let _ = stream.write_all(&body).await;
        }
    });
    let provider = VeniceMediaProvider::with_base_url(format!("http://{addr}"));
    let mut request = seedance_request();
    request.model_id = "grok-imagine-reference-to-video-private".into();
    request.operation = MediaOperation::ReferenceToVideo;
    request.controls = BTreeMap::from([
        (
            "duration".into(),
            ControlValue::DurationSeconds { value: 8.0 },
        ),
        (
            "aspect_ratio".into(),
            ControlValue::AspectRatio {
                width: 16,
                height: 9,
            },
        ),
    ]);
    let (path, input) = file_input("reference", b"reference-image");
    let error = provider
        .submit(
            &Secret::new("token"),
            &request,
            &SubmitContext {
                idempotency_key: "attempt-1".into(),
                inputs: vec![input],
            },
        )
        .await;
    let _ = std::fs::remove_file(path);
    let Submission::Queued { remote_job } = error.unwrap() else {
        panic!("expected queued grok job");
    };
    assert!(
        remote_job.metadata["download_url"]
            .as_str()
            .unwrap()
            .ends_with("/venice/video-download")
    );
    let PollResult::Completed { artifacts } = provider
        .poll(&Secret::new("token"), &remote_job)
        .await
        .unwrap()
    else {
        panic!("expected download_url completion");
    };
    assert_eq!(artifacts[0].mime_type, "video/mp4");
    server.await.unwrap();
}

#[tokio::test]
async fn retrieve_404_is_failed_other() {
    let (base, _, server) = serve(vec![
        FixtureResponse::json(200, QUEUE),
        FixtureResponse::json(404, r#"{"error":"Media could not be found."}"#),
    ])
    .await;
    let provider = VeniceMediaProvider::with_base_url(base);
    let Submission::Queued { remote_job } = provider
        .submit(&Secret::new("token"), &seedance_request(), &empty_context())
        .await
        .unwrap()
    else {
        panic!("expected queued video job");
    };
    let PollResult::Failed { error } = provider
        .poll(&Secret::new("token"), &remote_job)
        .await
        .unwrap()
    else {
        panic!("expected failed poll");
    };
    assert_eq!(error.kind, ProviderErrorKind::Other);
    server.await.unwrap();
}

#[tokio::test]
async fn retrieve_422_is_failed_invalid_request() {
    let (base, _, server) = serve(vec![
        FixtureResponse::json(200, QUEUE),
        FixtureResponse::json(422, CONTENT_POLICY),
    ])
    .await;
    let provider = VeniceMediaProvider::with_base_url(base);
    let Submission::Queued { remote_job } = provider
        .submit(&Secret::new("token"), &seedance_request(), &empty_context())
        .await
        .unwrap()
    else {
        panic!("expected queued video job");
    };
    let PollResult::Failed { error } = provider
        .poll(&Secret::new("token"), &remote_job)
        .await
        .unwrap()
    else {
        panic!("expected failed poll");
    };
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    assert!(error.message.contains("content policies"));
    server.await.unwrap();
}

#[tokio::test]
async fn retrieve_503_is_failed_transient() {
    let (base, _, server) = serve(vec![
        FixtureResponse::json(200, QUEUE),
        FixtureResponse::json(503, CAPACITY),
    ])
    .await;
    let provider = VeniceMediaProvider::with_base_url(base);
    let Submission::Queued { remote_job } = provider
        .submit(&Secret::new("token"), &seedance_request(), &empty_context())
        .await
        .unwrap()
    else {
        panic!("expected queued video job");
    };
    let PollResult::Failed { error } = provider
        .poll(&Secret::new("token"), &remote_job)
        .await
        .unwrap()
    else {
        panic!("expected failed poll");
    };
    assert_eq!(error.kind, ProviderErrorKind::Transient);
    assert_eq!(error.retry_after_seconds, Some(5));
    server.await.unwrap();
}

#[tokio::test]
async fn complete_posts_model_and_queue_id() {
    let (base, seen, server) = serve(vec![
        FixtureResponse::json(200, QUEUE),
        FixtureResponse::json(200, COMPLETE),
    ])
    .await;
    let provider = VeniceMediaProvider::with_base_url(base);
    let Submission::Queued { remote_job } = provider
        .submit(&Secret::new("token"), &seedance_request(), &empty_context())
        .await
        .unwrap()
    else {
        panic!("expected queued video job");
    };
    provider
        .complete_video(&Secret::new("token"), &remote_job)
        .await
        .unwrap();
    let recorded = seen.lock().unwrap();
    assert_eq!(recorded[1].path, "/video/complete");
    let body = json_body(&recorded[1]);
    assert_eq!(body["model"], "seedance-1-5-pro-text-to-video-basic");
    assert_eq!(body["queue_id"], remote_job.id);
    server.await.unwrap();
}

fn file_input(role: &str, bytes: &[u8]) -> (std::path::PathBuf, zeron_studio::ResolvedInput) {
    let path = std::env::temp_dir().join(format!(
        "zeron-venice-http-{}-{}.bin",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&path, bytes).unwrap();
    (
        path.clone(),
        zeron_studio::ResolvedInput {
            role: role.into(),
            ordinal: 0,
            path,
            mime_type: "image/png".into(),
            content_hash: "hash".into(),
            size_bytes: bytes.len() as u64,
        },
    )
}
