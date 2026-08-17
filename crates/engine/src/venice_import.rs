//! Load a Venice Studio image dump into provider-neutral completed history.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};

use chrono::{TimeZone, Utc};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;
use zeron_studio::{
    ControlId, ControlValue, GenerationRequest, MediaKind, MediaModel, MediaOperation, ModelId,
    ProviderId, StudioArtifactId, StudioConversationId, StudioTurnId,
};

const VENICE_PROVIDER_ID: &str = "venice";
const MAX_PROMPT_CHARS: usize = 32_000;

#[derive(Debug, thiserror::Error)]
pub enum VeniceImportError {
    #[error("venice dump: {0}")]
    Io(#[from] std::io::Error),
    #[error("venice dump json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid venice dump: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone)]
pub struct ImportedStudioHistory {
    pub conversations: Vec<ImportedConversation>,
    pub missing_files: usize,
}

#[derive(Debug, Clone)]
pub struct ImportedConversation {
    pub id: StudioConversationId,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub turns: Vec<ImportedTurn>,
}

#[derive(Debug, Clone)]
pub struct ImportedTurn {
    pub id: StudioTurnId,
    pub prompt: String,
    pub created_at: i64,
    pub runs: Vec<ImportedRun>,
}

#[derive(Debug, Clone)]
pub struct ImportedRun {
    pub model: MediaModel,
    pub request: GenerationRequest,
    pub succeeded: bool,
    pub artifacts: Vec<ImportedArtifact>,
}

#[derive(Debug, Clone)]
pub struct ImportedArtifact {
    pub id: StudioArtifactId,
    pub path: PathBuf,
    pub mime_type: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub created_at: i64,
    pub metadata: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub conversations_imported: usize,
    pub conversations_skipped: usize,
    pub turns_imported: usize,
    pub artifacts_imported: usize,
    pub missing_files: usize,
    pub failed_turns: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DumpFile {
    sessions: Vec<Value>,
    turns: Vec<Value>,
    media: Vec<Value>,
}

pub fn load_venice_image_dump(
    dump_dir: &Path,
    catalog: Option<&[MediaModel]>,
) -> Result<ImportedStudioHistory, VeniceImportError> {
    let meta_path = dump_dir.join("venice-studio-image-meta.json");
    let dump: DumpFile = serde_json::from_str(&std::fs::read_to_string(meta_path)?)?;
    if dump.sessions.is_empty() {
        return Err(VeniceImportError::Invalid(
            "dump contains no sessions".into(),
        ));
    }

    let mut turns_by_session: HashMap<String, Vec<Value>> = HashMap::new();
    for turn in dump.turns {
        let session_id = required_text(&turn, "sessionId")?;
        turns_by_session.entry(session_id).or_default().push(turn);
    }
    let mut media_by_turn: HashMap<String, Vec<Value>> = HashMap::new();
    for item in dump.media {
        let turn_id = required_text(&item, "turnId")?;
        media_by_turn.entry(turn_id).or_default().push(item);
    }

    let catalog_by_id = catalog
        .unwrap_or(&[])
        .iter()
        .map(|model| (model.id.as_str().to_owned(), model.clone()))
        .collect::<HashMap<_, _>>();

    let mut sessions = dump.sessions;
    sessions.sort_by_key(|session| json_i64(session.get("createdAtUnixTimestamp")).unwrap_or(0));

    let mut conversations = Vec::with_capacity(sessions.len());
    let mut missing_files = 0;
    for session in sessions {
        let session_id = required_text(&session, "id")?;
        let session_type = json_text(session.get("type")).unwrap_or_else(|| "generate".into());
        let created_at = json_i64(session.get("createdAtUnixTimestamp")).unwrap_or(0);
        let updated_at = json_i64(session.get("updatedAtUnixTimestamp")).unwrap_or(created_at);
        let mut session_turns = turns_by_session.remove(&session_id).unwrap_or_default();
        session_turns.sort_by_key(|turn| json_i64(turn.get("createdAtUnixTimestamp")).unwrap_or(0));

        let mut imported_turns = Vec::with_capacity(session_turns.len());
        for turn in session_turns {
            let (imported, missing) = import_turn(
                dump_dir,
                &session_type,
                &turn,
                media_by_turn
                    .remove(&required_text(&turn, "id")?)
                    .unwrap_or_default(),
                &catalog_by_id,
            )?;
            missing_files += missing;
            imported_turns.push(imported);
        }

        let title = imported_turns
            .iter()
            .find_map(|turn| title_from_prompt(&turn.prompt))
            .unwrap_or_else(|| "Venice import".to_owned());
        conversations.push(ImportedConversation {
            id: StudioConversationId(parse_uuid(&session_id)?),
            title,
            created_at,
            updated_at,
            turns: imported_turns,
        });
    }

    Ok(ImportedStudioHistory {
        conversations,
        missing_files,
    })
}

fn import_turn(
    dump_dir: &Path,
    session_type: &str,
    turn: &Value,
    mut media: Vec<Value>,
    catalog: &HashMap<String, MediaModel>,
) -> Result<(ImportedTurn, usize), VeniceImportError> {
    let turn_id = required_text(turn, "id")?;
    let created_at = json_i64(turn.get("createdAtUnixTimestamp")).unwrap_or(0);
    let mut prompt = json_text(turn.get("prompt")).unwrap_or_default();
    media.sort_by_key(|item| {
        (
            json_i64(item.get("createdAtUnixTimestamp")).unwrap_or(0),
            json_text(item.get("id")).unwrap_or_default(),
        )
    });

    let mut groups: BTreeMap<(String, String), Vec<Value>> = BTreeMap::new();
    for item in media {
        if json_text(item.get("mediaRole")).as_deref() != Some("output") {
            continue;
        }
        if prompt.trim().is_empty() {
            if let Some(media_prompt) = json_text(item.get("prompt")) {
                prompt = media_prompt;
            }
        }
        let model_id = json_text(item.get("modelId")).unwrap_or_default();
        let settings_key = item
            .get("imageSettings")
            .map(|value| value.to_string())
            .unwrap_or_default();
        groups
            .entry((model_id, settings_key))
            .or_default()
            .push(item);
    }

    let prompt = truncate_prompt(prompt);
    if prompt.is_empty() {
        return Err(VeniceImportError::Invalid(format!(
            "turn {turn_id} has an empty prompt"
        )));
    }

    let mut runs = Vec::new();
    let mut missing_files = 0;
    for ((model_id, _), items) in groups {
        let first = &items[0];
        let settings = first.get("imageSettings").cloned().unwrap_or(Value::Null);
        let (width, height) = display_aspect(&settings);
        let operation = if session_type == "edit"
            || json_text(first.get("source")).as_deref() == Some("edit")
        {
            MediaOperation::ImageEdit
        } else {
            MediaOperation::TextToImage
        };
        let display_name = json_text(first.get("modelName"))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| {
                if model_id.is_empty() {
                    "Venice model".into()
                } else {
                    model_id.clone()
                }
            });
        let model = catalog
            .get(&model_id)
            .cloned()
            .unwrap_or_else(|| stub_model(&model_id, &display_name, operation, created_at));
        let controls = mapped_controls(&settings, model.controls.iter().map(|c| c.id.as_str()));
        let negative = json_text(settings.get("negativePrompt"));
        let mut artifacts = Vec::new();
        for item in &items {
            match imported_artifact(dump_dir, item)? {
                ArtifactLoad::Ready(artifact) => artifacts.push(artifact),
                ArtifactLoad::Missing => missing_files += 1,
                ArtifactLoad::Skip => {}
            }
        }
        let output_count = artifacts.len().max(1) as u32;
        let request = GenerationRequest {
            provider_id: ProviderId::from(VENICE_PROVIDER_ID),
            model_id: if model_id.is_empty() {
                model.id.clone()
            } else {
                ModelId::from(model_id.as_str())
            },
            operation,
            prompt: prompt.clone(),
            negative_prompt: negative,
            output_count,
            controls,
            inputs: Vec::new(),
            manifest_version: model.manifest_version.clone(),
            display_aspect_ratio: (width, height),
        };
        runs.push(ImportedRun {
            model,
            request,
            succeeded: !artifacts.is_empty(),
            artifacts,
        });
    }

    if runs.is_empty() {
        let model = stub_model(
            "venice-import",
            "Venice import",
            MediaOperation::TextToImage,
            created_at,
        );
        runs.push(ImportedRun {
            request: GenerationRequest {
                provider_id: ProviderId::from(VENICE_PROVIDER_ID),
                model_id: model.id.clone(),
                operation: model.operation,
                prompt: prompt.clone(),
                negative_prompt: None,
                output_count: 1,
                controls: BTreeMap::new(),
                inputs: Vec::new(),
                manifest_version: model.manifest_version.clone(),
                display_aspect_ratio: (1, 1),
            },
            model,
            succeeded: false,
            artifacts: Vec::new(),
        });
    }

    Ok((
        ImportedTurn {
            id: StudioTurnId(parse_uuid(&turn_id)?),
            prompt,
            created_at,
            runs,
        },
        missing_files,
    ))
}

enum ArtifactLoad {
    Ready(ImportedArtifact),
    Missing,
    Skip,
}

fn imported_artifact(dump_dir: &Path, item: &Value) -> Result<ArtifactLoad, VeniceImportError> {
    let Some(id) = json_text(item.get("id")) else {
        return Ok(ArtifactLoad::Skip);
    };
    let Some(file_name) = json_text(item.get("fileName")) else {
        return Ok(ArtifactLoad::Missing);
    };
    let path = dump_dir.join(file_name);
    if !path.is_file() {
        return Ok(ArtifactLoad::Missing);
    }
    let settings = item.get("imageSettings").cloned().unwrap_or(Value::Null);
    let mime = json_text(item.get("mimeType")).unwrap_or_else(|| "image/webp".into());
    Ok(ArtifactLoad::Ready(ImportedArtifact {
        id: StudioArtifactId(parse_uuid(&id)?),
        path,
        mime_type: mime,
        width: json_u32(settings.get("width")),
        height: json_u32(settings.get("height")),
        created_at: json_i64(item.get("createdAtUnixTimestamp")).unwrap_or(0),
        metadata: serde_json::json!({
            "venice": {
                "fileName": item.get("fileName"),
                "modelId": item.get("modelId"),
                "modelName": item.get("modelName"),
                "source": item.get("source"),
                "imageSettings": settings,
            }
        }),
    }))
}

fn stub_model(
    model_id: &str,
    display_name: &str,
    operation: MediaOperation,
    created_at: i64,
) -> MediaModel {
    let id = if model_id.is_empty() {
        "venice-import"
    } else {
        model_id
    };
    MediaModel {
        provider_id: ProviderId::from(VENICE_PROVIDER_ID),
        id: ModelId::from(id),
        display_name: display_name.to_owned(),
        description: Some("Imported from Venice Studio".into()),
        operation,
        output_kind: MediaKind::Image,
        output_mime_types: vec!["image/webp".into(), "image/png".into(), "image/jpeg".into()],
        input_constraints: Vec::new(),
        prompt_maximum_chars: Some(MAX_PROMPT_CHARS as u32),
        negative_prompt_maximum_chars: None,
        maximum_output_count: 8,
        controls: Vec::new(),
        pricing: None,
        features: Vec::new(),
        manifest_version: "venice-import-v1".into(),
        fetched_at: Utc
            .timestamp_millis_opt(created_at)
            .single()
            .unwrap_or_else(Utc::now),
    }
}

fn mapped_controls(
    settings: &Value,
    allowed: impl IntoIterator<Item = impl AsRef<str>>,
) -> BTreeMap<ControlId, ControlValue> {
    let allowed: Vec<String> = allowed
        .into_iter()
        .map(|id| id.as_ref().to_owned())
        .collect();
    let allow = |id: &str| allowed.is_empty() || allowed.iter().any(|item| item == id);
    let mut controls = BTreeMap::new();
    if let Some((width, height)) = parse_aspect(json_text(settings.get("aspectRatio")).as_deref())
        && allow("aspect_ratio")
    {
        controls.insert(
            ControlId::from("aspect_ratio"),
            ControlValue::AspectRatio { width, height },
        );
    }
    if let Some(seed) = json_text(settings.get("seed")).and_then(|value| value.parse::<i64>().ok())
        && seed != 0
        && allow("seed")
    {
        controls.insert(
            ControlId::from("seed"),
            ControlValue::Integer { value: seed },
        );
    }
    if let Some(steps) = json_i64(settings.get("steps")).filter(|value| *value > 0)
        && allow("steps")
    {
        controls.insert(
            ControlId::from("steps"),
            ControlValue::Integer { value: steps },
        );
    }
    if let Some(resolution) =
        json_text(settings.get("resolution")).filter(|value| !value.is_empty())
        && allow("resolution")
    {
        controls.insert(
            ControlId::from("resolution"),
            ControlValue::Resolution { value: resolution },
        );
    }
    if let Some(format) = json_text(settings.get("format")).filter(|value| !value.is_empty())
        && allow("format")
    {
        controls.insert(
            ControlId::from("format"),
            ControlValue::Enum { value: format },
        );
    }
    if let Some(quality) = json_text(settings.get("quality")).filter(|value| !value.is_empty())
        && allow("quality")
    {
        controls.insert(
            ControlId::from("quality"),
            ControlValue::Enum { value: quality },
        );
    }
    controls
}

fn display_aspect(settings: &Value) -> (u32, u32) {
    if let Some(aspect) = parse_aspect(json_text(settings.get("aspectRatio")).as_deref()) {
        return aspect;
    }
    match (
        json_u32(settings.get("width")).filter(|value| *value > 0),
        json_u32(settings.get("height")).filter(|value| *value > 0),
    ) {
        (Some(width), Some(height)) => reduce_ratio(width, height),
        _ => (1, 1),
    }
}

fn parse_aspect(value: Option<&str>) -> Option<(u32, u32)> {
    let value = value?;
    let (width, height) = value.split_once(':')?;
    let width = width
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|value| *value > 0)?;
    let height = height
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|value| *value > 0)?;
    Some((width, height))
}

fn reduce_ratio(mut width: u32, mut height: u32) -> (u32, u32) {
    let mut a = width;
    let mut b = height;
    while b != 0 {
        let rest = a % b;
        a = b;
        b = rest;
    }
    if a > 0 {
        width /= a;
        height /= a;
    }
    (width.max(1), height.max(1))
}

fn title_from_prompt(prompt: &str) -> Option<String> {
    let title: String = prompt
        .split_whitespace()
        .take(7)
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(48)
        .collect();
    let title = title.trim().to_string();
    (!title.is_empty()).then_some(title)
}

fn truncate_prompt(prompt: String) -> String {
    let trimmed = prompt.trim();
    if trimmed.chars().count() <= MAX_PROMPT_CHARS {
        return trimmed.to_owned();
    }
    trimmed.chars().take(MAX_PROMPT_CHARS).collect()
}

fn required_text(value: &Value, key: &str) -> Result<String, VeniceImportError> {
    json_text(value.get(key)).ok_or_else(|| VeniceImportError::Invalid(format!("missing {key}")))
}

fn json_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn json_i64(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

fn json_u32(value: Option<&Value>) -> Option<u32> {
    json_i64(value).and_then(|value| u32::try_from(value).ok())
}

fn parse_uuid(value: &str) -> Result<Uuid, VeniceImportError> {
    Uuid::parse_str(value).map_err(|error| VeniceImportError::Invalid(error.to_string()))
}
