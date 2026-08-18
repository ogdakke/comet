//! Order derived images next to their source, not at the end of the turn.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

#[cfg(test)]
use zeron_proto::StudioTurnView;
use zeron_proto::{StudioConversationView, StudioRunState, StudioRunView};
#[cfg(test)]
use zeron_studio::MediaOperation;
use zeron_studio::{
    GenerationInputSource, StudioArtifactId, StudioConversationId, StudioRunId, StudioTurnId,
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
    own_aspect: (u32, u32),
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

#[cfg(test)]
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

fn nonzero_size(width: Option<u32>, height: Option<u32>) -> Option<(u32, u32)> {
    match (width, height) {
        (Some(width), Some(height)) if width > 0 && height > 0 => Some((width, height)),
        _ => None,
    }
}

fn slot_own_aspect(run: &StudioRunView, artifact_id: Option<StudioArtifactId>) -> (u32, u32) {
    artifact_id
        .and_then(|id| {
            run.artifacts
                .iter()
                .find(|artifact| artifact.id == id)
                .and_then(|artifact| nonzero_size(artifact.width, artifact.height))
        })
        .unwrap_or(run.display_aspect_ratio)
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
                    own_aspect: slot_own_aspect(run, artifact_id),
                    state: run.state,
                    progress: run.progress,
                });
            }
        }
    }
    slots
}

fn resolved_aspect(
    index: usize,
    slots: &[Slot],
    by_artifact: &HashMap<StudioArtifactId, usize>,
) -> (u32, u32) {
    let mut current = index;
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(current) {
            return slots[current].own_aspect;
        }
        let Some(source) = slots[current].source_artifact_id else {
            return slots[current].own_aspect;
        };
        let Some(&parent) = by_artifact.get(&source) else {
            return slots[current].own_aspect;
        };
        current = parent;
    }
}

/// Cheap identity of a conversation's slot graph. Progress is included so
/// in-flight tiles refresh; the feed must not rebuild this on every scroll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LineageKey {
    conversation_id: Option<StudioConversationId>,
    turns: u32,
    runs: u32,
    artifacts: u32,
    last_artifact: Option<StudioArtifactId>,
    mix: u64,
}

pub(super) fn lineage_key(view: Option<&StudioConversationView>) -> LineageKey {
    let Some(view) = view else {
        return LineageKey {
            conversation_id: None,
            turns: 0,
            runs: 0,
            artifacts: 0,
            last_artifact: None,
            mix: 0,
        };
    };
    let mut runs = 0u32;
    let mut artifacts = 0u32;
    let mut last_artifact = None;
    let mut mix = 0xcbf2_9ce4_8422_2325u64;
    for turn in &view.turns {
        mix = mix
            .wrapping_mul(0x1000_0000_01b3)
            .wrapping_add(turn.id.0.as_u128() as u64);
        for run in &turn.runs {
            runs = runs.saturating_add(1);
            artifacts = artifacts.saturating_add(run.artifacts.len() as u32);
            mix = mix
                .wrapping_mul(0x1000_0000_01b3)
                .wrapping_add(run_state_tag(run.state) as u64)
                .wrapping_add(run.output_count as u64);
            if let Some(progress) = run.progress {
                mix ^= u64::from(progress.to_bits());
            }
            if let Some(artifact) = run.artifacts.last() {
                last_artifact = Some(artifact.id);
                mix ^= artifact.id.0.as_u128() as u64;
            }
        }
    }
    LineageKey {
        conversation_id: Some(view.conversation.id),
        turns: view.turns.len() as u32,
        runs,
        artifacts,
        last_artifact,
        mix,
    }
}

fn run_state_tag(state: StudioRunState) -> u8 {
    match state {
        StudioRunState::Draft => 0,
        StudioRunState::Quoting => 1,
        StudioRunState::AwaitingConfirmation => 2,
        StudioRunState::Queued => 3,
        StudioRunState::Running => 4,
        StudioRunState::Downloading => 5,
        StudioRunState::Succeeded => 6,
        StudioRunState::Failed => 7,
        StudioRunState::Cancelling => 8,
        StudioRunState::Cancelled => 9,
    }
}

/// One walk of the conversation: tiles in paint order, plus O(1) lookups.
///
/// `lineage_tiles_for_turn` used to rebuild this graph for every feed row
/// on every frame. A 200-image thread then cloned every turn and walked
/// every slot again for layout, image requests, and each visible row.
#[derive(Clone, Debug, Default)]
pub(super) struct LineageIndex {
    tiles: Vec<LineageTile>,
    root_turn_ids: Vec<StudioTurnId>,
    root_turn_ixs: Vec<usize>,
    tile_range: HashMap<StudioTurnId, Range<usize>>,
    aspects: HashMap<StudioArtifactId, (u32, u32)>,
    pixel_sizes: HashMap<StudioArtifactId, (u32, u32)>,
    thumbhashes: HashMap<StudioArtifactId, String>,
}

impl LineageIndex {
    pub(super) fn build(view: &StudioConversationView) -> Self {
        let mut pixel_sizes = HashMap::new();
        let mut thumbhashes = HashMap::new();
        for turn in &view.turns {
            for run in &turn.runs {
                for artifact in &run.artifacts {
                    if let Some(size) = nonzero_size(artifact.width, artifact.height) {
                        pixel_sizes.insert(artifact.id, size);
                    }
                    if let Some(hash) = artifact.thumbhash.as_ref() {
                        if !hash.is_empty() {
                            thumbhashes.insert(artifact.id, hash.clone());
                        }
                    }
                }
            }
        }

        let slots = collect_slots(view);
        let by_artifact: HashMap<StudioArtifactId, usize> = slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.artifact_id.map(|id| (id, index)))
            .collect();
        let known: HashSet<StudioArtifactId> = by_artifact.keys().copied().collect();
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

        let mut tiles = Vec::with_capacity(slots.len());
        let mut visited = HashSet::new();
        fn walk(
            index: usize,
            root_turn_id: StudioTurnId,
            slots: &[Slot],
            children: &HashMap<StudioArtifactId, Vec<usize>>,
            by_artifact: &HashMap<StudioArtifactId, usize>,
            visited: &mut HashSet<usize>,
            tiles: &mut Vec<LineageTile>,
        ) {
            if !visited.insert(index) {
                return;
            }
            let slot = &slots[index];
            tiles.push(LineageTile {
                turn_id: slot.turn_id,
                run_id: slot.run_id,
                run_ix: slot.run_ix,
                output_ix: slot.output_ix,
                artifact_id: slot.artifact_id,
                source_artifact_id: slot.source_artifact_id,
                root_turn_id,
                aspect: resolved_aspect(index, slots, by_artifact),
                state: slot.state,
                progress: slot.progress,
            });
            if let Some(artifact_id) = slot.artifact_id
                && let Some(kids) = children.get(&artifact_id)
            {
                for &child in kids {
                    walk(
                        child,
                        root_turn_id,
                        slots,
                        children,
                        by_artifact,
                        visited,
                        tiles,
                    );
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
                &by_artifact,
                &mut visited,
                &mut tiles,
            );
        }

        let mut root_turn_ids = Vec::new();
        let mut seen_roots = HashSet::new();
        for tile in &tiles {
            if seen_roots.insert(tile.root_turn_id) {
                root_turn_ids.push(tile.root_turn_id);
            }
        }
        let turn_pos: HashMap<StudioTurnId, usize> = view
            .turns
            .iter()
            .enumerate()
            .map(|(index, turn)| (turn.id, index))
            .collect();
        let root_turn_ixs: Vec<usize> = root_turn_ids
            .iter()
            .filter_map(|id| turn_pos.get(id).copied())
            .collect();

        let mut tile_range = HashMap::new();
        let mut start = 0;
        while start < tiles.len() {
            let id = tiles[start].root_turn_id;
            let mut end = start + 1;
            while end < tiles.len() && tiles[end].root_turn_id == id {
                end += 1;
            }
            tile_range.insert(id, start..end);
            start = end;
        }

        let aspects = tiles
            .iter()
            .filter_map(|tile| tile.artifact_id.map(|id| (id, tile.aspect)))
            .collect();

        Self {
            tiles,
            root_turn_ids,
            root_turn_ixs,
            tile_range,
            aspects,
            pixel_sizes,
            thumbhashes,
        }
    }

    pub(super) fn tiles(&self) -> &[LineageTile] {
        &self.tiles
    }

    pub(super) fn root_count(&self) -> usize {
        self.root_turn_ids.len()
    }

    pub(super) fn root_turn_id(&self, feed_ix: usize) -> Option<StudioTurnId> {
        self.root_turn_ids.get(feed_ix).copied()
    }

    pub(super) fn root_turn_ix(&self, feed_ix: usize) -> Option<usize> {
        self.root_turn_ixs.get(feed_ix).copied()
    }

    pub(super) fn feed_index_of_root(&self, turn_id: StudioTurnId) -> Option<usize> {
        self.root_turn_ids.iter().position(|&id| id == turn_id)
    }

    pub(super) fn tiles_for_root(&self, turn_id: StudioTurnId) -> &[LineageTile] {
        self.tile_range
            .get(&turn_id)
            .and_then(|range| self.tiles.get(range.clone()))
            .unwrap_or(&[])
    }

    pub(super) fn aspect(&self, artifact_id: StudioArtifactId) -> Option<(u32, u32)> {
        self.aspects.get(&artifact_id).copied()
    }

    pub(super) fn pixel_size(&self, artifact_id: StudioArtifactId) -> Option<(u32, u32)> {
        self.pixel_sizes.get(&artifact_id).copied()
    }

    pub(super) fn thumbhash(&self, artifact_id: StudioArtifactId) -> Option<&str> {
        self.thumbhashes.get(&artifact_id).map(String::as_str)
    }
}

pub(super) fn lineage_tiles(view: &StudioConversationView) -> Vec<LineageTile> {
    LineageIndex::build(view).tiles
}

/// Feed/lightbox aspect for one artifact: the original's box, walked through
/// edit/upscale children so a 2:3 upscale is never laid out as a 1:1 tile.
pub(super) fn artifact_display_aspect(
    view: &StudioConversationView,
    artifact_id: StudioArtifactId,
) -> Option<(u32, u32)> {
    LineageIndex::build(view).aspect(artifact_id)
}

impl super::page::StudioPage {
    pub(super) fn sync_lineage(&mut self) {
        let key = lineage_key(self.conversation.as_ref());
        if self.lineage_key == Some(key) {
            return;
        }
        self.lineage_key = Some(key);
        self.lineage = self
            .conversation
            .as_ref()
            .map(LineageIndex::build)
            .unwrap_or_default();
    }

    pub(super) fn display_aspect_for(&self, artifact_id: StudioArtifactId) -> (u32, u32) {
        self.lineage
            .aspect(artifact_id)
            .or_else(|| {
                self.conversation
                    .as_ref()
                    .and_then(|view| artifact_display_aspect(view, artifact_id))
            })
            .or_else(|| {
                self.artifact_frame(artifact_id)
                    .and_then(|frame| frame.width.zip(frame.height))
                    .filter(|(width, height)| *width > 0 && *height > 0)
            })
            .unwrap_or((1, 1))
    }
}

#[cfg(test)]
pub(super) fn lineage_tiles_for_turn(
    view: &StudioConversationView,
    turn_id: StudioTurnId,
) -> Vec<LineageTile> {
    LineageIndex::build(view).tiles_for_root(turn_id).to_vec()
}

#[cfg(test)]
pub(super) fn turn_has_root_outputs(view: &StudioConversationView, turn_id: StudioTurnId) -> bool {
    LineageIndex::build(view)
        .root_turn_ids
        .iter()
        .any(|&id| id == turn_id)
}

#[cfg(test)]
pub(super) fn visible_root_turns(view: &StudioConversationView) -> Vec<StudioTurnView> {
    let index = LineageIndex::build(view);
    index
        .root_turn_ixs
        .iter()
        .filter_map(|&ix| view.turns.get(ix).cloned())
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
        assert!(is_derived_run(&conversation.turns[0].runs[1]));
    }

    #[test]
    fn derived_tiles_use_the_source_image_aspect() {
        let source_id = StudioArtifactId::new();
        let upscale_id = StudioArtifactId::new();
        let mut source = artifact(source_id);
        source.width = None;
        source.height = None;
        let mut generate = run(
            MediaOperation::TextToImage,
            "flux",
            vec![source],
            None,
            StudioRunState::Succeeded,
            1,
        );
        generate.display_aspect_ratio = (2, 3);
        let mut upscale = run(
            MediaOperation::Upscale,
            "upscale",
            vec![artifact(upscale_id)],
            Some(source_id),
            StudioRunState::Succeeded,
            1,
        );
        upscale.display_aspect_ratio = (1, 1);
        let conversation = view(vec![turn("a comet", vec![generate, upscale])]);
        let tiles = lineage_tiles(&conversation);
        assert_eq!(tiles[0].aspect, (2, 3));
        assert_eq!(tiles[1].aspect, (2, 3));
        assert_eq!(
            artifact_display_aspect(&conversation, upscale_id),
            Some((2, 3))
        );
    }

    #[test]
    fn index_looks_up_tiles_without_refiltering_the_whole_graph() {
        let a = StudioArtifactId::new();
        let b = StudioArtifactId::new();
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
        let other = turn(
            "another",
            vec![run(
                MediaOperation::TextToImage,
                "flux",
                vec![artifact(StudioArtifactId::new())],
                None,
                StudioRunState::Succeeded,
                1,
            )],
        );
        let generate_id = generate.id;
        let other_id = other.id;
        let conversation = view(vec![generate, other]);
        let index = LineageIndex::build(&conversation);
        assert_eq!(index.root_count(), 2);
        assert_eq!(index.tiles_for_root(generate_id).len(), 2);
        assert_eq!(index.tiles_for_root(other_id).len(), 1);
        assert_eq!(index.aspect(a), Some((1, 1)));
        assert_eq!(index.feed_index_of_root(other_id), Some(1));
        assert!(turn_has_root_outputs(&conversation, generate_id));
        assert!(turn_has_root_outputs(&conversation, other_id));
        assert_eq!(visible_root_turns(&conversation).len(), 2);
    }
}
