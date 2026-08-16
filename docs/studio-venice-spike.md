# Studio Venice capability spike

Reviewed 2026-08-16 against Venice's public `GET /api/v1/models` responses and official
`swagger.yaml`. The sanitized fixtures under `crates/studio/tests/fixtures/venice` are complete
single-model records copied from the live public image and video catalogs; they contain no key,
prompt, request, or media data.

## Proven normalization

- Image controls declared by the live model are rendered as aspect ratio, resolution, quality, and
  bounded integer steps. The endpoint schema supplies the output-format choices.
- Text-to-video controls are rendered from `model_type`, `aspect_ratios`, `resolutions`, `durations`,
  and the audio capability flags.
- Image-to-video uses the same constraint-driven controls plus one required `source` image. An empty
  live `aspect_ratios` array hides that control instead of guessing a supported value.
- Tests replace the representative video's ID and display name while retaining its constraints,
  proving operation and control discovery do not depend on a known model name.
- Manifest versions hash only normalized submit-relevant state, so catalog refresh timestamps do
  not invalidate an otherwise identical saved manifest.

## Capability gaps

- The current catalog labels some reference-to-video entries as `image-to-video`; it does not expose
  enough structured input-role/count metadata to distinguish them safely. The adapter therefore
  normalizes the declared operation and does not infer reference support from an ID substring.
- Image input byte/dimension limits and video reference limits are partly documented by endpoint
  schema but are not consistently present per model. They remain unset until a reviewed family
  override or richer live field establishes support.
- Model pricing can be a matrix over resolution, quality, duration, audio, or other inputs. The
  normalized catalog marks pricing as provider-defined; the video quote endpoint remains the source
  of an exact pre-submit price.
- Several image endpoint fields are documented as model-specific or ignored by unsupported models.
  They stay hidden unless the live catalog explicitly advertises them. This currently excludes
  style references, web search, prompt-enhancement thinking, LoRA strength, and dimensions.

## First-release playback decision

Use OS-player handoff for video in the first release. Generated MP4 artifacts will expose Open,
Reveal, and Export actions; embedded GPUI playback remains a separate platform spike. This leaves no
unknown playback dependency on the Phase 2 MVP path.

The remaining Phase 0 rendering proof must be completed in the GPUI slice: resolve images by jailed
artifact ID through the engine/RPC boundary, never by accepting a UI-provided filesystem path.
