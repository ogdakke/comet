//! Order derived images next to their source, not at the end of the turn.

use std::collections::{HashMap, HashSet};

use zeron_proto::{StudioConversationView, StudioRunState, StudioRunView, StudioTurnView};
use zeron_studio::{
    GenerationInputSource, MediaOperation, StudioArtifactId, StudioRunId, StudioTurnId,
};

/// One output slot in lineage order: generate roots first, then their
/// descendants depth-first in submission order.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct LineageTile {
    pub turn_id: StudioTurnId,
    pub run_id: StudioRunId,
    pub run_ix: usize,
    pub output_ix: usize,
    pub artifact_id: Option<StudioArtifactId>,
    pub source_artifact_id: Option<StudioArtifactId>,
    pub root_turn_id: StudioTurnId,
    pub aspect: (u32, u32),
    pub state: StudioRunState,
    pub progress: Option<f32>,
}

#[derive(Clone, Debug)]
struct Slot {
    turn_id: StudioTurnId,
    run_id: StudioRunId,
    run_ix: usize,
    output_ix: usize,
    artifact_id: Option<StudioArtifactId>,
    source_artifact_id: Option<StudioArtifactId>,
    aspect: (u32, u32),
    state: StudioRunState,
    progress: Option<f32>,
}

pub(super) fn run_source_artifact(run: &StudioRunView) -> Option<StudioArtifactId> {
    run.inputs.iter().find_map(|input| {
        if input.role.as_str() != "source" {
            return None;
        }
        match &input.source {
            GenerationInputSource::Artifact { artifact_id } => Some(*artifact_id),
            GenerationInputSource::Asset { .. } => None,
        }
    })
}

pub(super) fn is_derived_run(run: &StudioRunView) -> bool {
    matches!(
        run.model.operation,
        MediaOperation::ImageEdit | MediaOperation::Upscale
    ) || run_source_artifact(run).is_some()
}

fn feed_output_slots(run: &StudioRunView) -> Vec<(usize, Option<StudioArtifactId>)> {
    if run.state == StudioRunState::Succeeded {
        run.artifacts
            .iter()
            .map(|artifact| (artifact.output_position as usize, Some(artifact.id)))
            .collect()
    } else {
        (0..run.output_count as usize)
            .map(|output_ix| {
                let artifact = run
                    .artifacts
                    .iter()
                    .find(|artifact| artifact.output_position as usize == output_ix)
                    .map(|artifact| artifact.id);
                (output_ix, artifact)
            })
            .collect()
    }
}

fn collect_slots(view: &StudioConversationView) -> Vec<Slot> {
    let mut slots = Vec::new();
    for turn in &view.turns {
        for (run_ix, run) in turn.runs.iter().enumerate() {
            let source_artifact_id = run_source_artifact(run);
            for (output_ix, artifact_id) in feed_output_slots(run) {
                slots.push(Slot {
                    turn_id: turn.id,
                    run_id: run.id,
                    run_ix,
                    output_ix,
                    artifact_id,
                    source_artifact_id,
                    aspect: run.display_aspect_ratio,
                    state: run.state,
                    progress: run.progress,
                });
            }
        }
    }
    slots
}

pub(super) fn lineage_tiles(view: &StudioConversationView) -> Vec<LineageTile> {
    let slots = collect_slots(view);
    let known: HashSet<StudioArtifactId> =
        slots.iter().filter_map(|slot| slot.artifact_id).collect();
    let mut children: HashMap<StudioArtifactId, Vec<usize>> = HashMap::new();
    let mut roots = Vec::new();
    for (index, slot) in slots.iter().enumerate() {
        match slot.source_artifact_id {
            Some(source) if known.contains(&source) => {
                children.entry(source).or_default().push(index);
            }
            _ => roots.push(index),
        }
    }

    let mut ordered = Vec::with_capacity(slots.len());
    let mut visited = HashSet::new();
    fn walk(
        index: usize,
        root_turn_id: StudioTurnId,
        slots: &[Slot],
        children: &HashMap<StudioArtifactId, Vec<usize>>,
        visited: &mut HashSet<usize>,
        ordered: &mut Vec<LineageTile>,
    ) {
        if !visited.insert(index) {
            return;
        }
        let slot = &slots[index];
        ordered.push(LineageTile {
            turn_id: slot.turn_id,
            run_id: slot.run_id,
            run_ix: slot.run_ix,
            output_ix: slot.output_ix,
            artifact_id: slot.artifact_id,
            source_artifact_id: slot.source_artifact_id,
            root_turn_id,
            aspect: slot.aspect,
            state: slot.state,
            progress: slot.progress,
        });
        if let Some(artifact_id) = slot.artifact_id
            && let Some(kids) = children.get(&artifact_id)
        {
            for &child in kids {
                walk(child, root_turn_id, slots, children, visited, ordered);
            }
        }
    }

    for root in roots {
        let root_turn_id = slots[root].turn_id;
        walk(
            root,
            root_turn_id,
            &slots,
            &children,
            &mut visited,
            &mut ordered,
        );
    }
    ordered
}

pub(super) fn lineage_tiles_for_turn(
    view: &StudioConversationView,
    turn_id: StudioTurnId,
) -> Vec<LineageTile> {
    lineage_tiles(view)
        .into_iter()
        .filter(|tile| tile.root_turn_id == turn_id)
        .collect()
}

pub(super) fn turn_has_root_outputs(view: &StudioConversationView, turn_id: StudioTurnId) -> bool {
    let slots = collect_slots(view);
    let known: HashSet<StudioArtifactId> =
        slots.iter().filter_map(|slot| slot.artifact_id).collect();
    slots.iter().any(|slot| {
        slot.turn_id == turn_id
            && !slot
                .source_artifact_id
                .is_some_and(|source| known.contains(&source))
    })
}

pub(super) fn visible_root_turns(view: &StudioConversationView) -> Vec<StudioTurnView> {
    view.turns
        .iter()
        .filter(|turn| turn_has_root_outputs(view, turn.id))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use zeron_studio::{GenerationInput, MediaKind, StudioArtifactId, StudioBatchId};

    fn artifact(id: StudioArtifactId) -> zeron_proto::StudioArtifactView {
        zeron_proto::StudioArtifactView {
            id,
            output_position: 0,
            media_kind: MediaKind::Image,
            mime_type: "image/png".into(),
            size_bytes: 1,
            width: Some(1),
            height: Some(1),
            duration_seconds: None,
            metadata: serde_json::Value::Null,
            created_at: Utc::now(),
            thumbhash: None,
            content_hash: String::new(),
        }
    }

    fn model(operation: MediaOperation, name: &str) -> zeron_studio::MediaModel {
        zeron_studio::MediaModel {
            provider_id: "venice".into(),
            id: name.into(),
            display_name: name.into(),
            description: None,
            operation,
            output_kind: MediaKind::Image,
            output_mime_types: vec!["image/png".into()],
            input_constraints: Vec::new(),
            prompt_maximum_chars: None,
            negative_prompt_maximum_chars: None,
            maximum_output_count: 4,
            controls: Vec::new(),
            pricing: None,
            features: Vec::new(),
            manifest_version: "test".into(),
            fetched_at: Utc::now(),
        }
    }

    fn run(
        operation: MediaOperation,
        name: &str,
        artifacts: Vec<zeron_proto::StudioArtifactView>,
        source: Option<StudioArtifactId>,
        state: StudioRunState,
        output_count: u32,
    ) -> StudioRunView {
        StudioRunView {
            id: zeron_studio::StudioRunId::new(),
            position: 0,
            provider_id: "venice".into(),
            model: model(operation, name),
            controls: Default::default(),
            output_count,
            display_aspect_ratio: (1, 1),
            state,
            progress: None,
            error: None,
            quote: None,
            prompt: None,
            inputs: source
                .map(|artifact_id| GenerationInput {
                    role: "source".into(),
                    ordinal: 0,
                    source: GenerationInputSource::Artifact { artifact_id },
                    content_hash: String::new(),
                })
                .into_iter()
                .collect(),
            artifacts,
        }
    }

    fn turn(prompt: &str, runs: Vec<StudioRunView>) -> StudioTurnView {
        StudioTurnView {
            id: StudioTurnId::new(),
            position: 0,
            prompt: prompt.into(),
            source_turn_id: None,
            batch_id: StudioBatchId::new(),
            runs,
            created_at: Utc::now(),
        }
    }

    fn view(turns: Vec<StudioTurnView>) -> StudioConversationView {
        StudioConversationView {
            conversation: zeron_proto::StudioConversationSummary {
                id: zeron_studio::StudioConversationId::new(),
                title: "Test".into(),
                turn_count: turns.len() as u32,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                archived: false,
                forked_from_turn_id: None,
                creating: false,
                done: false,
            },
            turns,
        }
    }

    #[test]
    fn lineage_places_descendants_next_to_their_source() {
        let a = StudioArtifactId::new();
        let b = StudioArtifactId::new();
        let a_edit = StudioArtifactId::new();
        let a_up = StudioArtifactId::new();
        let b_edit = StudioArtifactId::new();
        let generate = turn(
            "a comet",
            vec![run(
                MediaOperation::TextToImage,
                "flux",
                vec![
                    {
                        let mut art = artifact(a);
                        art.output_position = 0;
                        art
                    },
                    {
                        let mut art = artifact(b);
                        art.output_position = 1;
                        art
                    },
                ],
                None,
                StudioRunState::Succeeded,
                2,
            )],
        );
        let generate_id = generate.id;
        let conversation = view(vec![
            generate,
            turn(
                "make the sky sunrise",
                vec![run(
                    MediaOperation::ImageEdit,
                    "edit",
                    vec![artifact(a_edit)],
                    Some(a),
                    StudioRunState::Succeeded,
                    1,
                )],
            ),
            turn(
                "",
                vec![run(
                    MediaOperation::Upscale,
                    "upscale",
                    vec![artifact(a_up)],
                    Some(a_edit),
                    StudioRunState::Succeeded,
                    1,
                )],
            ),
            turn(
                "change B",
                vec![run(
                    MediaOperation::ImageEdit,
                    "edit",
                    vec![artifact(b_edit)],
                    Some(b),
                    StudioRunState::Succeeded,
                    1,
                )],
            ),
        ]);

        let ids: Vec<_> = lineage_tiles_for_turn(&conversation, generate_id)
            .into_iter()
            .map(|tile| tile.artifact_id)
            .collect();
        assert_eq!(
            ids,
            vec![Some(a), Some(a_edit), Some(a_up), Some(b), Some(b_edit)]
        );
        assert_eq!(visible_root_turns(&conversation).len(), 1);
        assert_eq!(visible_root_turns(&conversation)[0].id, generate_id);
    }

    #[test]
    fn in_flight_child_keeps_a_hole_after_its_parent() {
        let a = StudioArtifactId::new();
        let generate = turn(
            "a comet",
            vec![
                run(
                    MediaOperation::TextToImage,
                    "flux",
                    vec![artifact(a)],
                    None,
                    StudioRunState::Succeeded,
                    1,
                ),
                run(
                    MediaOperation::ImageEdit,
                    "edit",
                    Vec::new(),
                    Some(a),
                    StudioRunState::Running,
                    1,
                ),
            ],
        );
        let generate_id = generate.id;
        let conversation = view(vec![generate]);
        let tiles = lineage_tiles_for_turn(&conversation, generate_id);
        assert_eq!(tiles.len(), 2);
        assert_eq!(tiles[0].artifact_id, Some(a));
        assert_eq!(tiles[1].artifact_id, None);
        assert_eq!(tiles[1].source_artifact_id, Some(a));
        assert_eq!(tiles[1].state, StudioRunState::Running);
    }
}
