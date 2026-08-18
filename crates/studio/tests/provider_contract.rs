use std::collections::BTreeMap;

use zeron_studio::{
    FakeMediaProvider, FakeSubmissionMode, GenerationRequest, MediaKind, MediaOperation,
    MediaProvider, PollResult, ProviderArtifact, ProviderErrorKind, Secret, Submission,
    SubmitContext,
};

fn request() -> GenerationRequest {
    GenerationRequest {
        provider_id: "fake".into(),
        model_id: "image-model".into(),
        operation: MediaOperation::TextToImage,
        prompt: "a comet over a quiet lake".to_owned(),
        negative_prompt: None,
        output_count: 1,
        controls: BTreeMap::new(),
        inputs: Vec::new(),
        manifest_version: "fixture-v1".to_owned(),
        display_aspect_ratio: (1, 1),
    }
}

fn artifact() -> ProviderArtifact {
    ProviderArtifact {
        media_kind: MediaKind::Image,
        mime_type: "image/png".to_owned(),
        bytes: vec![1, 2, 3],
        width: Some(1),
        height: Some(1),
        duration_seconds: None,
        metadata: serde_json::Value::Null,
    }
}

#[tokio::test]
async fn fake_provider_rejects_bad_credentials_without_exposing_them() {
    let provider = FakeMediaProvider::new(
        "fake",
        Vec::new(),
        FakeSubmissionMode::Complete(vec![artifact()]),
    );
    let secret = Secret::new("do-not-print-me");
    let error = provider.validate_credentials(&secret).await.unwrap_err();

    assert_eq!(error.kind, ProviderErrorKind::InvalidCredential);
    assert_eq!(format!("{secret:?}"), "Secret([REDACTED])");
}

#[tokio::test]
async fn duplicate_idempotency_key_returns_the_same_queued_job() {
    let provider = FakeMediaProvider::new(
        "fake",
        Vec::new(),
        FakeSubmissionMode::Queue {
            polls_before_completion: 1,
            artifacts: vec![artifact()],
        },
    );
    let secret = Secret::new("valid");
    let context = SubmitContext {
        idempotency_key: "stable-attempt-key".to_owned(),
        inputs: Vec::new(),
    };

    let first = provider
        .submit(&secret, &request(), &context)
        .await
        .unwrap();
    let duplicate = provider
        .submit(&secret, &request(), &context)
        .await
        .unwrap();
    assert_eq!(first, duplicate);

    let Submission::Queued { remote_job } = first else {
        panic!("expected a queued fake job");
    };
    assert!(matches!(
        provider.poll(&secret, &remote_job).await.unwrap(),
        PollResult::Running { .. }
    ));
    assert!(matches!(
        provider.poll(&secret, &remote_job).await.unwrap(),
        PollResult::Completed { .. }
    ));
}

#[tokio::test]
async fn venice_hidden_video_models_are_not_queued() {
    let provider = zeron_studio::VeniceMediaProvider::with_base_url("http://127.0.0.1:1");
    let mut video = request();
    video.provider_id = "venice".into();
    video.model_id = "kling-o3-pro-reference-to-video".into();
    video.operation = MediaOperation::ReferenceToVideo;
    video.controls.insert(
        "duration".into(),
        zeron_studio::ControlValue::DurationSeconds { value: 8.0 },
    );
    let error = provider
        .submit(
            &Secret::new("token"),
            &video,
            &SubmitContext {
                idempotency_key: "hidden".into(),
                inputs: Vec::new(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Unsupported);
}

#[tokio::test]
async fn venice_poll_503_is_failed_transient_not_a_poll_variant() {
    let capacity = include_str!("fixtures/venice/video-503.json");
    let queue = include_str!("fixtures/venice/video-queue.json");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (status, body) in [(200_u16, queue), (503, capacity)] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut incoming = vec![0_u8; 65536];
            let _ = stream.read(&mut incoming).await;
            let response = format!(
                "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nRetry-After: 5\r\nConnection: close\r\n\r\n{body}",
                if status == 200 {
                    "OK"
                } else {
                    "Service Unavailable"
                },
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    let provider = zeron_studio::VeniceMediaProvider::with_base_url(format!("http://{addr}"));
    let mut video = request();
    video.provider_id = "venice".into();
    video.model_id = "seedance-1-5-pro-text-to-video-basic".into();
    video.operation = MediaOperation::TextToVideo;
    video.controls.insert(
        "duration".into(),
        zeron_studio::ControlValue::DurationSeconds { value: 10.0 },
    );
    let Submission::Queued { remote_job } = provider
        .submit(
            &Secret::new("token"),
            &video,
            &SubmitContext {
                idempotency_key: "transient".into(),
                inputs: Vec::new(),
            },
        )
        .await
        .unwrap()
    else {
        panic!("expected queued venice video");
    };
    let PollResult::Failed { error } = provider
        .poll(&Secret::new("token"), &remote_job)
        .await
        .unwrap()
    else {
        panic!("503 must be PollResult::Failed, not a Transient variant");
    };
    assert_eq!(error.kind, ProviderErrorKind::Transient);
    server.await.unwrap();
}
