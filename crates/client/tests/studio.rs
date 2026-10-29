//! The demo Mac's Studio, through the client's real Studio passthrough
//! (`studio_call` / `watch_studio`). Every reply is decoded into the
//! engine's wire types, so a shape the engine wouldn't send fails here.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use zeron_client::events::NullListener;
use zeron_client::{Client, ClientConfig, Credentials, DemoOptions, StreamSpeed, StudioSink};
use zeron_proto::studio::{
    ListStudioArtifactsResponse, ListStudioConversationsResponse, ListStudioModelsResponse,
    ListStudioProvidersResponse, ProviderValidationState, StudioArtifactChunk,
    StudioConversationSummary, StudioConversationView, StudioProviderBalanceResponse,
    StudioProviderConnection, StudioRunState,
};
use zeron_studio::{
    ComposerMode, ComposerSnapshot, ComposerView, ControlKind, ControlValue, MediaKind,
    MediaOperation, SelectedModelRef,
};

const MAC: &str = "dev-mac";
const INSUFFICIENT: &str = "Insufficient USD or Diem balance to complete request";

fn demo() -> (Client, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = ClientConfig::new("https://edge.invalid", dir.path());
    config.device_id = "ios-test".into();
    config.device_name = "Test iPhone".into();
    let options = DemoOptions {
        stream_speed: StreamSpeed::Fast,
        ..Default::default()
    };
    let client = Client::new(config, Credentials::Demo(options), Arc::new(NullListener)).unwrap();
    (client, dir)
}

fn call<T: DeserializeOwned>(client: &Client, method: &str, params: Value) -> T {
    let reply = zeron_client::runtime::shared()
        .block_on(client.studio_call(MAC, method, &params.to_string()))
        .unwrap_or_else(|err| panic!("{method}: {err}"));
    serde_json::from_str(&reply).unwrap_or_else(|err| panic!("{method} reply: {err}\n{reply}"))
}

fn call_err(client: &Client, method: &str, params: Value) -> String {
    zeron_client::runtime::shared()
        .block_on(client.studio_call(MAC, method, &params.to_string()))
        .expect_err(method)
        .to_string()
}

#[derive(Default)]
struct Items {
    items: Mutex<Vec<String>>,
    ended: Mutex<Option<Option<String>>>,
}

impl StudioSink for Items {
    fn item(&self, json: String) {
        self.items.lock().unwrap().push(json);
    }
    fn ended(&self, error: Option<String>) {
        *self.ended.lock().unwrap() = Some(error);
    }
}

impl Items {
    fn all<T: DeserializeOwned>(&self) -> Vec<T> {
        self.items
            .lock()
            .unwrap()
            .iter()
            .map(|json| serde_json::from_str(json).unwrap_or_else(|e| panic!("{e}\n{json}")))
            .collect()
    }

    fn last<T: DeserializeOwned>(&self) -> Option<T> {
        self.all().pop()
    }
}

fn wait_for(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn conversations(client: &Client) -> Vec<StudioConversationSummary> {
    let list: ListStudioConversationsResponse = call(
        client,
        "ListStudioConversations",
        json!({ "includeArchived": true }),
    );
    list.conversations
}

fn thread(client: &Client, title: &str) -> StudioConversationSummary {
    conversations(client)
        .into_iter()
        .find(|c| c.title == title)
        .unwrap_or_else(|| panic!("no thread {title}"))
}

fn read_all(client: &Client, method: &str, artifact_id: &str) -> (Vec<u8>, String, usize) {
    let mut bytes = Vec::new();
    let mut offset = 0u64;
    let mut chunks = 0;
    loop {
        let chunk: StudioArtifactChunk = call(
            client,
            method,
            json!({ "artifactId": artifact_id, "offset": offset }),
        );
        assert_eq!(chunk.artifact_id.0.to_string(), artifact_id);
        let data = base64::engine::general_purpose::STANDARD
            .decode(&chunk.data)
            .unwrap();
        assert_eq!(chunk.next_offset, offset + data.len() as u64);
        bytes.extend(data);
        chunks += 1;
        if chunk.done {
            return (bytes, chunk.mime_type, chunks);
        }
        assert!(chunk.next_offset > offset, "read did not advance");
        offset = chunk.next_offset;
    }
}

#[test]
fn providers_are_configured_and_editable() {
    let (client, _dir) = demo();
    let list: ListStudioProvidersResponse = call(&client, "ListStudioProviders", json!({}));
    assert_eq!(list.providers.len(), 1);
    let venice = &list.providers[0];
    assert_eq!(venice.provider_id.as_str(), "venice");
    assert!(venice.configured);
    assert_eq!(venice.validation_state, ProviderValidationState::Valid);

    // The phone's flow: set, then validate.
    let set: StudioProviderConnection = call(
        &client,
        "SetStudioProviderCredential",
        json!({ "providerId": "venice", "displayLabel": "Venice", "secret": "bad-key" }),
    );
    assert_eq!(set.validation_state, ProviderValidationState::NotValidated);
    let checked: StudioProviderConnection = call(
        &client,
        "ValidateStudioProvider",
        json!({ "providerId": "venice" }),
    );
    assert_eq!(checked.validation_state, ProviderValidationState::Invalid);
    assert!(checked.validation_message.is_some());

    let _: StudioProviderConnection = call(
        &client,
        "SetStudioProviderCredential",
        json!({ "providerId": "venice", "displayLabel": "Venice", "secret": "vk-live-123" }),
    );
    let checked: StudioProviderConnection = call(
        &client,
        "ValidateStudioProvider",
        json!({ "providerId": "venice" }),
    );
    assert_eq!(checked.validation_state, ProviderValidationState::Valid);
    let balance: StudioProviderBalanceResponse = call(
        &client,
        "GetStudioProviderBalance",
        json!({ "providerId": "venice" }),
    );
    assert_eq!(balance.balance.unwrap().remaining.currency, "USD");

    let removed: Value = call(
        &client,
        "RemoveStudioProviderCredential",
        json!({ "providerId": "venice" }),
    );
    assert_eq!(removed, json!({ "ok": true }));
    let list: ListStudioProvidersResponse = call(&client, "ListStudioProviders", json!({}));
    assert!(!list.providers[0].configured);
    assert!(
        call_err(
            &client,
            "ValidateStudioProvider",
            json!({ "providerId": "venice" })
        )
        .contains("not configured")
    );
}

#[test]
fn models_are_a_real_venice_catalog() {
    let (client, _dir) = demo();
    let all: ListStudioModelsResponse = call(
        &client,
        "ListStudioModels",
        json!({ "providerId": "venice", "refresh": false }),
    );
    assert!(!all.stale);
    let images: Vec<_> = all
        .models
        .iter()
        .filter(|m| m.output_kind == MediaKind::Image)
        .collect();
    assert!(images.len() >= 3);
    for model in &images {
        assert_eq!(model.operation, MediaOperation::TextToImage);
        assert_eq!(model.maximum_output_count, 4);
        assert!(
            model
                .controls
                .iter()
                .any(|c| c.kind == ControlKind::AspectRatio)
        );
        assert!(model.pricing.is_some());
    }
    assert!(
        images
            .iter()
            .any(|m| m.controls.iter().any(|c| c.kind == ControlKind::Resolution))
    );
    let video = all
        .models
        .iter()
        .find(|m| m.output_kind == MediaKind::Video)
        .expect("a video model");
    let durations = video.video_capability().unwrap().durations;
    assert!(durations.contains(&ControlValue::DurationSeconds { value: 5.0 }));

    let only_video: ListStudioModelsResponse = call(
        &client,
        "ListStudioModels",
        json!({ "providerId": "venice", "mediaKind": "video" }),
    );
    assert_eq!(only_video.models.len(), 1);
    let unknown = call_err(&client, "ListStudioModels", json!({ "providerId": "nope" }));
    assert!(unknown.contains("unknown studio provider"), "{unknown}");
}

fn snapshot(conversation: &str, prompt: &str, model: &str, outputs: u32) -> ComposerSnapshot {
    let mut selected = SelectedModelRef::new("venice", model);
    selected.output_count = outputs;
    selected.controls.insert(
        "aspect_ratio".into(),
        ControlValue::AspectRatio {
            width: 16,
            height: 9,
        },
    );
    ComposerSnapshot {
        conversation_id: Some(serde_json::from_value(json!(conversation)).unwrap()),
        mode: ComposerMode::Image,
        prompt: prompt.into(),
        selected: vec![selected],
        ..Default::default()
    }
}

#[test]
fn composer_evaluates_with_the_real_evaluator() {
    let (client, _dir) = demo();
    let id = thread(&client, "Neon koi pond at night").id.0.to_string();
    let ok: ComposerView = call(
        &client,
        "EvaluateStudioComposer",
        json!({ "composer": snapshot(&id, "a lantern", "gpt-image-2", 4), "providerId": "venice" }),
    );
    assert!(ok.send.enabled, "{:?}", ok.conflicts);
    assert_eq!(ok.models.len(), 1);
    assert_eq!(ok.models[0].output_count, 4);

    let too_many: ComposerView = call(
        &client,
        "EvaluateStudioComposer",
        json!({ "composer": snapshot(&id, "a lantern", "gpt-image-2", 9), "providerId": "venice" }),
    );
    assert!(!too_many.send.enabled);
    assert!(!too_many.conflicts.is_empty());

    // The engine refuses a blocked composer at send, with the conflict's title.
    let refused = call_err(
        &client,
        "CreateStudioTurn",
        json!({
            "conversationId": id,
            "prompt": "a lantern",
            "composer": snapshot(&id, "a lantern", "gpt-image-2", 9),
        }),
    );
    assert!(refused.contains("output count"), "{refused}");
}

#[test]
fn gallery_pages_newest_first_through_every_artifact() {
    let (client, _dir) = demo();
    let first: ListStudioArtifactsResponse =
        call(&client, "ListStudioArtifacts", json!({ "limit": 24 }));
    assert_eq!(first.artifacts.len(), 24);
    let mut all = first.artifacts.clone();
    let mut cursor = first.next_cursor.clone();
    let mut pages = 1;
    while let Some(next) = cursor {
        let page: ListStudioArtifactsResponse = call(
            &client,
            "ListStudioArtifacts",
            json!({ "limit": 24, "cursor": next }),
        );
        all.extend(page.artifacts);
        cursor = page.next_cursor;
        pages += 1;
    }
    assert!(pages >= 4, "{pages} pages");
    assert!((75..=95).contains(&all.len()), "{} artifacts", all.len());
    let unpaged: ListStudioArtifactsResponse = call(&client, "ListStudioArtifacts", json!({}));
    assert_eq!(unpaged.artifacts, all);
    assert!(unpaged.next_cursor.is_none());

    for pair in all.windows(2) {
        assert!(
            (pair[0].created_at, pair[0].id) > (pair[1].created_at, pair[1].id),
            "not newest first"
        );
    }
    assert!(
        all.iter()
            .all(|a| a.thumbhash.is_some() && a.size_bytes > 0)
    );
    let shape = |a: &&zeron_proto::studio::StudioGalleryItem| {
        let (w, h) = (a.width.unwrap(), a.height.unwrap());
        w.cmp(&h)
    };
    use std::cmp::Ordering::*;
    for wanted in [Less, Equal, Greater] {
        assert!(all.iter().any(|a| shape(&a) == wanted), "no {wanted:?}");
    }
    let video = all
        .iter()
        .find(|a| a.media_kind == MediaKind::Video)
        .expect("a video");
    assert!(video.duration_seconds.is_some());
    assert_eq!(video.mime_type, "video/mp4");
}

#[test]
fn threads_list_and_watch_like_the_engine() {
    let (client, _dir) = demo();
    let active: ListStudioConversationsResponse =
        call(&client, "ListStudioConversations", json!({}));
    let everything = conversations(&client);
    assert_eq!(everything.len(), active.conversations.len() + 1);
    assert!(active.conversations.iter().all(|c| !c.archived));
    assert!(active.conversations.iter().any(|c| c.creating));
    assert!(active.conversations.iter().any(|c| c.done));
    for pair in active.conversations.windows(2) {
        assert!(pair[0].updated_at >= pair[1].updated_at);
    }

    // A thread with a failed run, followed through its watch.
    let lake = thread(&client, "Mountain lake at golden hour");
    assert!(lake.turn_count >= 5);
    let sink = Arc::new(Items::default());
    let _watch = client
        .watch_studio(
            MAC,
            "WatchStudioConversation",
            &json!({ "conversationId": lake.id }).to_string(),
            sink.clone(),
        )
        .unwrap();
    wait_for("the thread", Duration::from_secs(5), || {
        !sink.items.lock().unwrap().is_empty()
    });
    let view: StudioConversationView = sink.last().unwrap();
    assert_eq!(view.conversation.id, lake.id);
    let runs: Vec<_> = view.turns.iter().flat_map(|t| &t.runs).collect();
    let failed = runs
        .iter()
        .find(|r| r.state == StudioRunState::Failed)
        .expect("a failed run");
    assert_eq!(failed.error.as_deref(), Some(INSUFFICIENT));
    assert!(
        runs.iter()
            .any(|r| r.state == StudioRunState::Succeeded && (1..=4).contains(&r.artifacts.len()))
    );
    assert!(
        runs.iter()
            .flat_map(|r| &r.artifacts)
            .any(|a| a.media_kind == MediaKind::Video)
    );

    // The "done" thread clears once seen.
    let done = active.conversations.iter().find(|c| c.done).unwrap();
    let seen: StudioConversationSummary = call(
        &client,
        "MarkStudioConversationSeen",
        json!({ "conversationId": done.id }),
    );
    assert!(!seen.done);

    // An unknown thread fails the watch, on the sink.
    let missing = Arc::new(Items::default());
    let _watch = client
        .watch_studio(
            MAC,
            "WatchStudioConversation",
            &json!({ "conversationId": uuid_nil() }).to_string(),
            missing.clone(),
        )
        .unwrap();
    wait_for("the watch to fail", Duration::from_secs(5), || {
        missing.ended.lock().unwrap().is_some()
    });
    let error = missing.ended.lock().unwrap().clone().flatten().unwrap();
    assert!(
        error.contains("studio conversation was not found"),
        "{error}"
    );
}

fn uuid_nil() -> &'static str {
    "00000000-0000-0000-0000-000000000000"
}

#[test]
fn a_new_turn_plays_out_on_every_watch() {
    let (client, _dir) = demo();
    let koi = thread(&client, "Neon koi pond at night");
    let id = koi.id.0.to_string();

    let thread_sink = Arc::new(Items::default());
    let gallery_sink = Arc::new(Items::default());
    let list_sink = Arc::new(Items::default());
    let _watches = [
        client.watch_studio(
            MAC,
            "WatchStudioConversation",
            &json!({ "conversationId": id }).to_string(),
            thread_sink.clone(),
        ),
        client.watch_studio(
            MAC,
            "WatchStudioGallery",
            &json!({ "limit": 30 }).to_string(),
            gallery_sink.clone(),
        ),
        client.watch_studio(
            MAC,
            "WatchStudioConversations",
            &json!({ "includeArchived": false }).to_string(),
            list_sink.clone(),
        ),
    ];
    wait_for("watches to open", Duration::from_secs(5), || {
        [&thread_sink, &gallery_sink, &list_sink]
            .iter()
            .all(|s| !s.items.lock().unwrap().is_empty())
    });
    let before: ListStudioArtifactsResponse = gallery_sink.last().unwrap();

    let prompt = "Koi circling a single lantern, overhead";
    let reply: StudioConversationView = call(
        &client,
        "CreateStudioTurn",
        json!({
            "conversationId": id,
            "prompt": prompt,
            "composer": snapshot(&id, prompt, "gpt-image-2", 3),
        }),
    );
    let turn = reply.turns.last().unwrap();
    assert_eq!(turn.prompt, prompt);
    assert_eq!(turn.runs.len(), 1);
    assert_eq!(turn.runs[0].state, StudioRunState::Queued);
    assert!(turn.runs[0].quote.is_some());
    assert!(reply.conversation.creating);
    let run_id = turn.runs[0].id;

    wait_for("the run to land", Duration::from_secs(10), || {
        thread_sink
            .last::<StudioConversationView>()
            .and_then(|v| {
                v.turns
                    .iter()
                    .flat_map(|t| &t.runs)
                    .find(|r| r.id == run_id)
                    .map(|r| r.state == StudioRunState::Succeeded)
            })
            .unwrap_or(false)
    });
    let view: StudioConversationView = thread_sink.last().unwrap();
    let run = view
        .turns
        .iter()
        .flat_map(|t| &t.runs)
        .find(|r| r.id == run_id)
        .unwrap();
    assert_eq!(run.artifacts.len(), 3);
    let artifact = &run.artifacts[0];
    assert_eq!(
        (artifact.width, artifact.height),
        (Some(1024), Some(576)),
        "16:9 at 1K"
    );

    wait_for("the gallery to show it", Duration::from_secs(5), || {
        gallery_sink
            .last::<ListStudioArtifactsResponse>()
            .is_some_and(|g| g.artifacts.first().is_some_and(|a| a.turn_id == turn.id))
    });
    let after: ListStudioArtifactsResponse = gallery_sink.last().unwrap();
    assert_eq!(after.artifacts.len(), before.artifacts.len());
    assert_eq!(after.artifacts[0].prompt, prompt);
    let listed: ListStudioConversationsResponse = list_sink.last().unwrap();
    assert_eq!(listed.conversations[0].id, koi.id);
    assert_eq!(listed.conversations[0].turn_count, koi.turn_count + 1);

    // Its original reads back as the PNG the listing described.
    let (png, mime, chunks) = read_all(
        &client,
        "ReadStudioArtifactChunk",
        &artifact.id.0.to_string(),
    );
    assert_eq!(mime, artifact.mime_type);
    assert_eq!(png.len() as u64, artifact.size_bytes);
    assert!(chunks > 1, "originals span several chunks");
    let decoded = image::load_from_memory(&png).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (1024, 576));

    // "fail" fails like an empty account; retrying still fails for it.
    let doomed = "Please fail this one";
    let reply: StudioConversationView = call(
        &client,
        "CreateStudioTurn",
        json!({
            "conversationId": id,
            "prompt": doomed,
            "composer": snapshot(&id, doomed, "qwen-image", 1),
        }),
    );
    let failed_id = reply.turns.last().unwrap().runs[0].id;
    wait_for("the run to fail", Duration::from_secs(10), || {
        thread_sink
            .last::<StudioConversationView>()
            .and_then(|v| {
                v.turns
                    .iter()
                    .flat_map(|t| &t.runs)
                    .find(|r| r.id == failed_id)
                    .map(|r| r.error.as_deref() == Some(INSUFFICIENT))
            })
            .unwrap_or(false)
    });

    // Delete the new image: the gallery drops it.
    let ok: Value = call(
        &client,
        "DeleteStudioArtifact",
        json!({ "artifactId": artifact.id }),
    );
    assert_eq!(ok, json!({ "ok": true }));
    let page: ListStudioArtifactsResponse =
        call(&client, "ListStudioArtifacts", json!({ "limit": 200 }));
    assert!(page.artifacts.iter().all(|a| a.id != artifact.id));
}

#[test]
fn a_failed_seed_run_retries_to_success() {
    let (client, _dir) = demo();
    let lake = thread(&client, "Mountain lake at golden hour");
    let view = view_of(&client, &lake);
    let failed = view
        .turns
        .iter()
        .flat_map(|t| &t.runs)
        .find(|r| r.state == StudioRunState::Failed)
        .unwrap()
        .id;
    let reply: StudioConversationView = call(&client, "RetryStudioRun", json!({ "runId": failed }));
    let run = reply
        .turns
        .iter()
        .flat_map(|t| &t.runs)
        .find(|r| r.id == failed)
        .unwrap();
    assert_eq!(run.state, StudioRunState::Queued);
    assert!(run.error.is_none());
    let again = call_err(&client, "RetryStudioRun", json!({ "runId": failed }));
    assert!(again.contains("still queued"), "{again}");
}

/// A thread's view, off a one-shot watch (the engine has no unary read).
fn view_of(client: &Client, summary: &StudioConversationSummary) -> StudioConversationView {
    let sink = Arc::new(Items::default());
    let _watch = client
        .watch_studio(
            MAC,
            "WatchStudioConversation",
            &json!({ "conversationId": summary.id }).to_string(),
            sink.clone(),
        )
        .unwrap();
    wait_for("the thread", Duration::from_secs(5), || {
        !sink.items.lock().unwrap().is_empty()
    });
    sink.last().unwrap()
}

#[test]
fn previews_are_chunked_jpegs_and_video_originals_say_why_not() {
    let (client, _dir) = demo();
    let page: ListStudioArtifactsResponse =
        call(&client, "ListStudioArtifacts", json!({ "limit": 200 }));
    let image = page
        .artifacts
        .iter()
        .find(|a| a.media_kind == MediaKind::Image && a.width > a.height)
        .unwrap();
    let (jpeg, mime, _) = read_all(&client, "ReadStudioPreviewChunk", &image.id.0.to_string());
    assert_eq!(mime, "image/jpeg");
    let decoded = image::load_from_memory(&jpeg).unwrap();
    assert_eq!(
        decoded.height(),
        512,
        "short edge at the engine's preview size"
    );

    // Offsets are honoured: a read from the middle returns the tail.
    let tail: StudioArtifactChunk = call(
        &client,
        "ReadStudioPreviewChunk",
        json!({ "artifactId": image.id, "offset": 100 }),
    );
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(tail.data)
        .unwrap();
    assert_eq!(&bytes[..16], &jpeg[100..116]);

    let video = page
        .artifacts
        .iter()
        .find(|a| a.media_kind == MediaKind::Video)
        .unwrap();
    let (poster, _, _) = read_all(&client, "ReadStudioPreviewChunk", &video.id.0.to_string());
    assert!(image::load_from_memory(&poster).is_ok());
    let refused = call_err(
        &client,
        "ReadStudioArtifactChunk",
        json!({ "artifactId": video.id, "offset": 0 }),
    );
    assert!(refused.contains("video"), "{refused}");
    let missing = call_err(
        &client,
        "ReadStudioPreviewChunk",
        json!({ "artifactId": uuid_nil(), "offset": 0 }),
    );
    assert!(
        missing.contains("studio artifact was not found"),
        "{missing}"
    );
}

#[test]
fn only_the_mac_hosts_studio() {
    let (client, _dir) = demo();
    let err = zeron_client::runtime::shared()
        .block_on(client.studio_call("dev-vps", "ListStudioProviders", "{}"))
        .unwrap_err();
    assert!(
        matches!(err, zeron_client::ClientError::Unsupported(_)),
        "{err}"
    );
}
