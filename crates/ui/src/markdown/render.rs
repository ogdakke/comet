//! BlockTree → gpui elements.
//!
//! Numbers drive layout (font sizes, line heights, paddings — all constants
//! here); colors are paint. Code blocks render per-line so their height is
//! exactly `lines × line_height`, and syntax highlighting arrives later as
//! recolored `TextRun`s on the identical mono font — layout never changes
//! (mugen's "highlight is pure paint"). Streaming fade-in is a per-appended-
//! chunk opacity veil over the text runs (see [`super::veil`]) — opacity only,
//! zero translate, applied after layout-relevant properties are fixed.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::time::Instant;

use gpui::{
    AnyElement, BorderStyle, Bounds, FontStyle, FontWeight, Hsla, InteractiveText, SharedString,
    StyledText, TextRun, UnderlineStyle, Window, canvas, div, font, point, prelude::*, px, quad,
    size,
};
use zeron_syntax::{HighlightKind, HighlightSpan, HighlightedDocument};

use crate::theme::Theme;

use super::parser::{Block, BlockTree, InlineRun, TableAlign};
use super::veil::{RowVeil, apply_veil, slice_spans};

/// Gap between markdown blocks inside one message (zeron mdBlockGap).
pub const MD_BLOCK_GAP: f32 = 12.0;
/// Body text size / line height (zeron: 14px / 22px).
pub const MD_TEXT_SIZE: f32 = 14.0;
pub const MD_LINE_HEIGHT: f32 = 22.0;
/// Code block metrics — height is `lines × CODE_LINE_HEIGHT + padding + header`.
pub const CODE_TEXT_SIZE: f32 = 12.5;
pub const CODE_LINE_HEIGHT: f32 = 18.0;
pub const CODE_PADDING_X: f32 = 12.0;
pub const CODE_PADDING_Y: f32 = 10.0;

// Table metrics — a port of mugen-markdown 0.6.2's `TableBlock` under zeron's
// resolved md theme. The design is frameless ("flat hairline"): 1px horizontal
// rules under the header and between rows are the only chrome — no outer box,
// no header fill, no corner radius (theme: headerBackground transparent,
// radius 0). Cells use the body scale (14/22) with a uniform 12px padding;
// the header row is weight-700 per `table.headerWeight`.
/// Uniform cell padding in px (zeron `table.cellPadding`).
pub const TABLE_CELL_PADDING: f32 = 12.0;
/// Hairline between rows in px (zeron `table.gap`).
pub const TABLE_DIVIDER: f32 = 1.0;
/// Header row font weight (zeron `table.headerWeight` = 700).
pub const TABLE_HEADER_WEIGHT: FontWeight = FontWeight::BOLD;
/// Floor for a column's max-content share, so a short column ("1k") beside a
/// prose column keeps a readable width (mugen `MIN_COLUMN_CONTENT`).
pub const TABLE_MIN_COLUMN_CONTENT: f32 = 48.0;
/// Minimum rendered column width in px, padding included (zeron
/// `table.minColumnWidth`). Naturally narrower columns keep their content
/// width; wider ones wrap down to this floor, then the table scrolls.
pub const TABLE_MIN_COLUMN_WIDTH: f32 = 96.0;
/// Hairline tone (zeron md theme `table.borderColor`: rgba(255,255,255,0.1)).
pub fn table_hairline() -> Hsla {
    crate::theme::hairline(0.10)
}

/// Options for one rendered tree (a transcript row or a whole live message).
pub struct RenderOptions {
    /// Stable row key — prefixes element ids (scroll state, animations).
    pub row_key: SharedString,
    /// Streaming veil state for a live row: newly appended text fades in via
    /// paint-only run opacity, keyed per (element, chunk offset) so each chunk
    /// fades exactly once. `None` renders without fades (completed rows).
    pub veil: Option<Rc<RefCell<RowVeil>>>,
    /// Flatten/shape input cache (see [`RenderCache`]): settled blocks reuse
    /// their flat text + runs across frames instead of rebuilding them — the
    /// per-frame cost of a fading live row stays O(tail block), flat in the
    /// total reply length. `None` rebuilds every pass.
    pub cache: Option<Rc<RefCell<RenderCache>>>,
    /// Frame timestamp driving veil opacities (one clock per render pass).
    pub now: Instant,
    /// Code-block copy-button plumbing (round 9): `None` renders no button
    /// (previews outside the transcript).
    pub copy: Option<CopyUi>,
    /// Override for markdown link clicks. `None` opens only safe external
    /// URLs ([`open_external_markdown_url`]) — fragment / relative dests
    /// must not hit `App::open_url` (macOS NSWorkspace error −50).
    pub on_link: Option<Rc<dyn Fn(&str, &mut Window, &mut gpui::App)>>,
}

/// Copy-button wiring for one row's code blocks: the handler writes the code
/// to the clipboard and flips a transient per-row "Copied" state owned by the
/// transcript entity; `copied_ix` is the block currently showing feedback.
#[derive(Clone)]
pub struct CopyUi {
    pub handler: Rc<dyn Fn(usize, SharedString, &mut Window, &mut gpui::App)>,
    pub copied_ix: Option<usize>,
}

impl RenderOptions {
    /// Options for a completed (non-streaming) row — no veil, no cache.
    pub fn settled(row_key: SharedString) -> Self {
        Self {
            row_key,
            veil: None,
            cache: None,
            now: Instant::now(),
            copy: None,
            on_link: None,
        }
    }
}

/// `http` / `https` / `mailto` only. Anything else (`#heading`, `comet`,
/// `./file.md`, `javascript:`) is not a browser URL — handing it to
/// [`gpui::App::open_url`] makes macOS Launch Services try to launch it
/// as an app and pop "The application can't be opened. −50".
pub fn is_safe_external_url(url: &str) -> bool {
    let url = url.trim();
    let Some((scheme, rest)) = url.split_once(':') else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    if rest.starts_with("//") {
        return scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https");
    }
    scheme.eq_ignore_ascii_case("mailto")
}

/// In-document dest: `#slug` or `page.md#slug`. `None` for external URLs.
pub fn markdown_fragment(url: &str) -> Option<String> {
    let url = url.trim();
    if is_safe_external_url(url) {
        return None;
    }
    let raw = url.strip_prefix('#').or_else(|| {
        url.split_once('#')
            .map(|(_, frag)| frag)
            .filter(|frag| !frag.is_empty())
    })?;
    let decoded = percent_decode(raw);
    let trimmed = decoded.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Open `url` in the system browser when it is a real external dest.
pub fn open_external_markdown_url(url: &str, cx: &mut gpui::App) {
    if is_safe_external_url(url) {
        cx.open_url(url);
    }
}

/// GFM / GitHub heading id ([`github-slugger`]): lowercase, drop the
/// punctuation class (includes em/en dashes), then each ASCII space
/// becomes `-`. Two spaces around a removed dash stay as `--` — that's
/// why `Appendix A — Fake Sequence` is `#appendix-a--fake-sequence`.
/// Duplicate headings get `-1`, `-2` via [`top_heading_slugs`].
pub fn gfm_heading_slug(text: &str) -> String {
    let mut stripped = String::new();
    for ch in text.chars() {
        if is_gfm_slug_punctuation(ch) {
            continue;
        }
        for c in ch.to_lowercase() {
            stripped.push(c);
        }
    }
    stripped.replace(' ', "-")
}

/// github-slugger's removed set: General Punctuation + Supplemental
/// Punctuation + the listed ASCII marks. Hyphen and underscore stay.
fn is_gfm_slug_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '\u{2000}'..='\u{206F}'
            | '\u{2E00}'..='\u{2E7F}'
            | '\\'
            | '\''
            | '!'
            | '"'
            | '#'
            | '$'
            | '%'
            | '&'
            | '('
            | ')'
            | '*'
            | '+'
            | ','
            | '.'
            | '/'
            | ':'
            | ';'
            | '<'
            | '='
            | '>'
            | '?'
            | '@'
            | '['
            | ']'
            | '^'
            | '`'
            | '{'
            | '|'
            | '}'
            | '~'
    )
}

/// `(top-level block index, slug)` for every heading in document order.
pub fn top_heading_slugs(tree: &BlockTree) -> Vec<(usize, String)> {
    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut out = Vec::new();
    for (ix, top) in tree.blocks.iter().enumerate() {
        let Block::Heading { runs, .. } = &top.block else {
            continue;
        };
        let base = gfm_heading_slug(&inline_plain_text(runs));
        if base.is_empty() {
            continue;
        }
        let n = counts.entry(base.clone()).or_insert(0);
        let slug = if *n == 0 { base } else { format!("{base}-{n}") };
        *n += 1;
        out.push((ix, slug));
    }
    out
}

/// Child index of the heading a `#fragment` should scroll to.
pub fn heading_index_for_fragment(tree: &BlockTree, fragment: &str) -> Option<usize> {
    let want = fragment.trim().trim_start_matches('#');
    if want.is_empty() {
        return None;
    }
    let slugged = gfm_heading_slug(want);
    let folded = fold_hyphens(want);
    top_heading_slugs(tree).into_iter().find_map(|(ix, slug)| {
        (slug.eq_ignore_ascii_case(want)
            || (!slugged.is_empty() && slug == slugged)
            || fold_hyphens(&slug) == folded)
            .then_some(ix)
    })
}

/// Collapse `--` runs so a generator that kept one hyphen still matches
/// GitHub's two-space `--` slugs (and the reverse).
fn fold_hyphens(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_hyphen = false;
    for ch in s.chars() {
        if ch == '-' {
            if !prev_hyphen {
                out.push('-');
            }
            prev_hyphen = true;
        } else {
            out.push(ch);
            prev_hyphen = false;
        }
    }
    out
}

fn inline_plain_text(runs: &[InlineRun]) -> String {
    let mut out = String::new();
    for run in runs {
        out.push_str(&run.text);
    }
    out
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out)
        .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned())
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Cross-frame cache of flatten results, keyed by
/// `(row key, top-level block ix, element discriminator)`.
///
/// During a streaming fade the live row re-renders every frame; without the
/// cache each frame re-derives every block's flat `String` + `TextRun`s —
/// O(reply length) per frame, growing through long replies. The incremental
/// parser only ever touches a suffix of the top-level blocks
/// ([`super::parser::IncrementalParser::stable_prefix_blocks`]), so everything
/// below that boundary is byte-identical and its flatten result (and, via
/// gpui's line-layout cache keyed on identical text+runs, its shaping) can be
/// reused as-is. `SharedString`/`Rc` make the reuse O(1) per block.
/// Cached runs carry a resolved [`gpui::Hsla`] per span, so an entry is only
/// valid for the palette that produced it — content-only keys silently serve
/// dark-mode text onto a light background after an appearance switch.
/// [`RenderCache::sync_palette`] drops everything when the palette moves.
#[derive(Default)]
pub struct RenderCache {
    flats: HashMap<(SharedString, usize, usize), Rc<FlatText>>,
    code: HashMap<(SharedString, usize, usize), Rc<CachedCode>>,
    /// The [`crate::theme::theme_generation`] these entries were shaped under.
    generation: u32,
}

/// Cached per-line code runs (validity: code length + highlight identity).
pub struct CachedCode {
    code_len: usize,
    /// Slice-pointer identity + len of the highlight Arc that produced this.
    hl_key: (usize, usize),
    lines: Vec<(SharedString, Vec<TextRun>)>,
}

impl RenderCache {
    /// Drop every cached entry for `row`.
    pub fn invalidate_row(&mut self, row: &str) {
        self.flats.retain(|(r, _, _), _| r.as_ref() != row);
        self.code.retain(|(r, _, _), _| r.as_ref() != row);
    }

    pub fn clear(&mut self) {
        self.flats.clear();
        self.code.clear();
    }

    /// Drop every entry if the palette changed since they were shaped. Cheap
    /// enough (one relaxed atomic load) to call on every cache access.
    fn sync_palette(&mut self) {
        let generation = crate::theme::theme_generation();
        if self.generation != generation {
            self.clear();
            self.generation = generation;
        }
    }
}

/// Per-line highlight tokens for a code block, or `None` while pending.
pub type CodeHighlight<'a> = Option<&'a [Vec<HighlightSpan>]>;

/// Render a whole tree stacked with the md block gap. `highlight` resolves
/// tokens for a top-level block index (code blocks only).
pub fn render_tree(
    tree: &BlockTree,
    opts: &RenderOptions,
    theme: &Theme,
    window: &Window,
    highlight: &dyn Fn(usize) -> Option<std::sync::Arc<HighlightedDocument>>,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(MD_BLOCK_GAP))
        .children(tree.blocks.iter().enumerate().map(|(ix, top)| {
            let document = highlight(ix);
            render_block(
                &top.block,
                ix,
                ix,
                opts,
                theme,
                window,
                document
                    .as_deref()
                    .map(|document| document.lines.as_slice()),
            )
        }))
        .into_any_element()
}

/// Render one block (top-level or nested). `top_ix` is the enclosing top-level
/// block index (cache invalidation scope); `ix` the per-element discriminator.
#[allow(clippy::too_many_arguments)]
pub fn render_block(
    block: &Block,
    top_ix: usize,
    ix: usize,
    opts: &RenderOptions,
    theme: &Theme,
    window: &Window,
    highlight: CodeHighlight,
) -> AnyElement {
    match block {
        Block::Paragraph { runs } => text_element(
            runs,
            MD_TEXT_SIZE,
            MD_LINE_HEIGHT,
            false,
            top_ix,
            ix,
            opts,
            theme,
        ),
        Block::Heading { level, runs } => {
            let (size, line) = heading_metrics(*level);
            text_element(runs, size, line, true, top_ix, ix, opts, theme)
        }
        Block::CodeBlock { language, code } => render_code_block(
            language.as_deref(),
            code,
            top_ix,
            ix,
            opts,
            theme,
            highlight,
        ),
        Block::BlockQuote { children } => div()
            // Accent-tinted quote: indigo rail + a whisper of the same hue
            // behind it (the inline-code treatment, dialed down).
            .border_l_2()
            .border_color(theme.accent.opacity(0.6))
            .bg(theme.accent.opacity(0.05))
            .rounded_tr(px(6.0))
            .rounded_br(px(6.0))
            .pl(px(12.0))
            .pr(px(10.0))
            .py(px(6.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .text_color(theme.text_muted)
            .children(children.iter().enumerate().map(|(ci, child)| {
                render_block(child, top_ix, ix * 100 + ci, opts, theme, window, None)
            }))
            .into_any_element(),
        Block::List {
            ordered_start,
            items,
        } => div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .children(items.iter().enumerate().map(|(item_ix, item)| {
                // Accent markers (the inline-code hue): ordered numbers as
                // tinted text, unordered as a REAL 5px disc — the glyph "•"
                // reads too small at 14px.
                let marker: gpui::AnyElement = match ordered_start {
                    Some(start) => div()
                        .flex_none()
                        .min_w(px(18.0))
                        .text_size(px(MD_TEXT_SIZE))
                        .line_height(px(MD_LINE_HEIGHT))
                        .text_color(theme.accent.opacity(0.85))
                        .child(SharedString::from(format!("{}.", start + item_ix as u64)))
                        .into_any_element(),
                    None => div()
                        .flex_none()
                        .min_w(px(18.0))
                        // Center the disc on the first text line's cap band.
                        .h(px(MD_LINE_HEIGHT))
                        .flex()
                        .items_center()
                        .child(
                            div()
                                .ml(px(1.0))
                                .w(px(5.0))
                                .h(px(5.0))
                                .rounded_full()
                                .bg(theme.accent.opacity(0.85)),
                        )
                        .into_any_element(),
                };
                div().flex().flex_row().gap(px(8.0)).child(marker).child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .children(item.iter().enumerate().map(|(ci, child)| {
                            render_block(
                                child,
                                top_ix,
                                ix * 100 + item_ix * 10 + ci,
                                opts,
                                theme,
                                window,
                                None,
                            )
                        })),
                )
            }))
            .into_any_element(),
        Block::Table {
            header,
            rows,
            align,
        } => render_table(header, rows, align, top_ix, ix, opts, theme, window),
        Block::Rule => div()
            .h(px(1.0))
            .w_full()
            .bg(theme.border)
            .into_any_element(),
    }
}

fn inline_plain(runs: &[InlineRun]) -> String {
    runs.iter().map(|run| run.text.as_str()).collect()
}

/// Document-order selectable slices for one block, keyed exactly the way
/// [`render_block`] registers them so a virtualized row can resolve a drag
/// against text that is not currently painted.
pub fn collect_block_selectables(
    block: &Block,
    ix: usize,
    row_key: &str,
    out: &mut Vec<(String, String)>,
) {
    match block {
        Block::Paragraph { runs } | Block::Heading { runs, .. } => {
            let text = inline_plain(runs);
            if !text.is_empty() {
                out.push((format!("{row_key}:{ix}"), text));
            }
        }
        Block::CodeBlock { code, .. } => {
            for (li, line) in code.split('\n').enumerate() {
                out.push((format!("{row_key}:{ix}:c{li}"), line.to_string()));
            }
        }
        Block::BlockQuote { children } => {
            for (ci, child) in children.iter().enumerate() {
                collect_block_selectables(child, ix * 100 + ci, row_key, out);
            }
        }
        Block::List { items, .. } => {
            for (item_ix, item) in items.iter().enumerate() {
                for (ci, child) in item.iter().enumerate() {
                    collect_block_selectables(child, ix * 100 + item_ix * 10 + ci, row_key, out);
                }
            }
        }
        Block::Table { header, rows, .. } => {
            let all: Vec<&[Vec<InlineRun>]> = std::iter::once(header.as_slice())
                .filter(|h| !h.is_empty())
                .chain(rows.iter().map(|r| r.as_slice()))
                .collect();
            for (r, row) in all.iter().enumerate() {
                for (c, cell) in row.iter().enumerate() {
                    let text = inline_plain(cell);
                    if !text.is_empty() {
                        out.push((format!("{row_key}:{}", table_cell_ix(ix, r, c)), text));
                    }
                }
            }
        }
        Block::Rule => {}
    }
}

/// Tight monochrome heading scale (zeron: h2 ≈ 16px semibold; headings step
/// down quickly toward body size).
fn heading_metrics(level: u8) -> (f32, f32) {
    match level {
        1 => (19.0, 27.0),
        2 => (16.0, 24.0),
        3 => (15.0, 22.0),
        _ => (14.0, 22.0),
    }
}

/// Shared per-column table geometry (port of mugen `tableColumns`).
pub struct TableColumns {
    /// Per-column max-content width, padding included.
    pub naturals: Vec<f32>,
    /// Per-column minimum width, padding included = `min(natural, minColumnWidth)`.
    pub minimums: Vec<f32>,
    /// Σ minimums — the width below which the table stops shrinking and scrolls.
    pub min_table_width: f32,
}

/// Resolve column geometry from measured per-column max-content widths
/// (content only — padding is added here, as the source adds `2 * cellPadding`).
pub fn table_columns(content_widths: &[f32]) -> TableColumns {
    let naturals: Vec<f32> = content_widths
        .iter()
        .map(|w| w.max(TABLE_MIN_COLUMN_CONTENT) + 2.0 * TABLE_CELL_PADDING)
        .collect();
    let minimums: Vec<f32> = naturals
        .iter()
        .map(|n| n.min(TABLE_MIN_COLUMN_WIDTH))
        .collect();
    let min_table_width = minimums.iter().sum();
    TableColumns {
        naturals,
        minimums,
        min_table_width,
    }
}

/// Element/cache discriminator for a table cell (row-major under the block ix).
fn table_cell_ix(ix: usize, r: usize, c: usize) -> usize {
    ix * 100_000 + r * 100 + c
}

/// A GFM table — a port of mugen-markdown's `TableBlock` under zeron's md
/// theme (see the `TABLE_*` constants).
///
/// Column widths resolve exactly the way the source's CSS does: each cell is
/// `flex: <max-content> <max-content> 0; min-width: min(max-content, 96px)`,
/// so widths are content-proportional with a readable per-column floor.
/// Naturals come from shaping each cell's runs unwrapped (gpui's line-layout
/// cache makes repeat frames cheap); the flex resolution itself is Taffy's —
/// the same algorithm as the web's. When even the floors no longer fit, the
/// rows overflow the viewport and the table scrolls horizontally instead of
/// crushing every column into per-character wrapping.
#[allow(clippy::too_many_arguments)]
fn render_table(
    header: &[Vec<InlineRun>],
    rows: &[Vec<Vec<InlineRun>>],
    align: &[TableAlign],
    top_ix: usize,
    ix: usize,
    opts: &RenderOptions,
    theme: &Theme,
    window: &Window,
) -> AnyElement {
    // Header row first, mirroring the source's `rows` shape (rows may be ragged).
    let all: Vec<&[Vec<InlineRun>]> = std::iter::once(header)
        .filter(|h| !h.is_empty())
        .map(|h| h as &[Vec<InlineRun>])
        .chain(rows.iter().map(|r| r.as_slice()))
        .collect();
    let cols = all.iter().map(|r| r.len()).max().unwrap_or(0);
    if cols == 0 {
        return gpui::Empty.into_any_element();
    }
    let has_header = !header.is_empty();

    // Flatten every cell (cache-aware) and take per-column max-content widths.
    let text_system = window.text_system();
    let mut flats: Vec<Vec<Option<Rc<FlatText>>>> = Vec::with_capacity(all.len());
    let mut content = vec![0.0f32; cols];
    for (r, row) in all.iter().enumerate() {
        let weight = if has_header && r == 0 {
            TABLE_HEADER_WEIGHT
        } else {
            FontWeight::NORMAL
        };
        let mut out: Vec<Option<Rc<FlatText>>> = Vec::with_capacity(cols);
        for (c, natural) in content.iter_mut().enumerate() {
            let Some(runs) = row.get(c) else {
                out.push(None);
                continue;
            };
            let flat = flatten_cached(runs, weight, top_ix, table_cell_ix(ix, r, c), opts, theme);
            if !flat.text.is_empty() {
                // Cell sources are single-line; guard anyway (same byte count,
                // so the runs still cover the text exactly).
                let line: SharedString = if flat.text.contains('\n') {
                    flat.text.replace('\n', " ").into()
                } else {
                    flat.text.clone()
                };
                let width = f32::from(
                    text_system
                        .shape_line(line, px(MD_TEXT_SIZE), &flat.runs, None)
                        .width(),
                );
                if width > *natural {
                    *natural = width;
                }
            }
            out.push(Some(flat));
        }
        flats.push(out);
    }
    let geo = table_columns(&content);

    // Frameless flat-hairline chrome: 1px rules under the header and between
    // rows are the only paint (`table.gap` = 1, borderColor white@10%); the
    // theme's headerBackground is transparent and its radius 0, so there is no
    // header fill, outer box, or rounding.
    let hairline = table_hairline();
    let mut inner = div()
        .flex()
        .flex_col()
        .w_full()
        .min_w(px(geo.min_table_width));
    for (r, row) in flats.iter().enumerate() {
        if r > 0 {
            inner = inner.child(div().flex_none().h(px(TABLE_DIVIDER)).w_full().bg(hairline));
        }
        let mut row_el = div().flex().flex_row();
        for (c, cell_flat) in row.iter().enumerate() {
            let mut cell = div()
                .flex_grow(geo.naturals[c])
                .flex_shrink(geo.naturals[c])
                .flex_basis(px(0.0))
                .min_w(px(geo.minimums[c]))
                .p(px(TABLE_CELL_PADDING))
                .text_size(px(MD_TEXT_SIZE))
                .line_height(px(MD_LINE_HEIGHT));
            cell = match align.get(c).copied().unwrap_or_default() {
                TableAlign::Left => cell,
                TableAlign::Center => cell.text_center(),
                TableAlign::Right => cell.text_right(),
            };
            if let Some(flat) = cell_flat {
                cell = cell.child(flat_text_element(
                    flat,
                    table_cell_ix(ix, r, c),
                    opts,
                    theme,
                ));
            }
            row_el = row_el.child(cell);
        }
        inner = inner.child(row_el);
    }

    // The horizontal scroller — when the floors exceed the viewport the inner
    // block keeps `min_table_width` and this viewport scrolls it.
    let scroll_id: SharedString = format!("{}-table{ix}", opts.row_key).into();
    div()
        .id(scroll_id)
        .w_full()
        .overflow_x_scroll()
        .child(inner)
        .into_any_element()
}

/// Flattened inline runs: one string + gpui `TextRun`s + clickable link ranges
/// + inline-code ranges (their rounded washes are painted by a canvas UNDER
/// the text — `TextRun::background_color` can only paint square boxes).
/// `text` is a `SharedString` so cached reuse across frames is an Arc clone.
pub struct FlatText {
    pub text: SharedString,
    pub runs: Vec<TextRun>,
    pub links: Vec<(Range<usize>, String)>,
    pub code_ranges: Vec<Range<usize>>,
}

/// Inline-code tint (round 9): the original is neutral (chat-view.tsx mdTheme
/// `inlineCode: #f0f0f0 on white/8%`), but the user asked for "a nice purple"
/// — violet-300 text over a violet-400 wash, readable on the #060606 panel.
pub fn inline_code_text(theme: &Theme) -> Hsla {
    theme.code_text // violet-300
}
pub fn inline_code_wash(theme: &Theme) -> Hsla {
    theme.code_wash // violet-400/12
}
/// Rounded-wash geometry: small radius on a slightly inset box (paint-only —
/// x extends 2px past the glyphs, y insets 2px from the 22px line box).
pub const INLINE_CODE_RADIUS: f32 = 4.5;
pub const INLINE_CODE_PAD_X: f32 = 2.0;
pub const INLINE_CODE_INSET_Y: f32 = 2.0;

/// Flatten inline runs into shaped-text inputs. Pure given a theme.
pub fn flatten_runs(runs: &[InlineRun], theme: &Theme, bold_default: bool) -> FlatText {
    flatten_runs_weighted(
        runs,
        theme,
        if bold_default {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::NORMAL
        },
    )
}

/// [`flatten_runs`] with an explicit base weight (table headers are 700 per
/// zeron's `table.headerWeight`; strong runs never drop below semibold).
fn flatten_runs_weighted(runs: &[InlineRun], theme: &Theme, base_weight: FontWeight) -> FlatText {
    let mut text = String::new();
    let mut out: Vec<TextRun> = Vec::with_capacity(runs.len());
    let mut links: Vec<(Range<usize>, String)> = Vec::new();
    let mut code_ranges: Vec<Range<usize>> = Vec::new();
    for run in runs {
        if run.text.is_empty() {
            continue;
        }
        let start = text.len();
        text.push_str(&run.text);
        let mut f = if run.style.code {
            font(theme.font_mono.clone())
        } else {
            font(theme.font_sans.clone())
        };
        f.weight = if run.style.bold && base_weight.0 < FontWeight::SEMIBOLD.0 {
            FontWeight::SEMIBOLD
        } else {
            base_weight
        };
        f.style = if run.style.italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        };
        // Links stay monochrome — foreground with an underline (zeron's md
        // theme underlines in the text color; indigo is reserved for primary
        // actions).
        let is_link = run.style.link.is_some();
        // Inline code reads violet (see `inline_code_text`); everything else
        // stays the monochrome foreground.
        let color = if run.style.code {
            inline_code_text(theme)
        } else {
            theme.text
        };
        if run.style.code {
            // Merge adjacent code runs into one wash box (like links below).
            match code_ranges.last_mut() {
                Some(range) if range.end == start => range.end = text.len(),
                _ => code_ranges.push(start..text.len()),
            }
        }
        if let Some(url) = &run.style.link {
            // A still-streaming link (mend.rs sentinel) keeps link styling —
            // so the URL's completion changes nothing visually — but is not
            // clickable until the real destination exists.
            if url != super::mend::PENDING_LINK_URL {
                // Merge adjacent runs of the same link into one clickable range.
                match links.last_mut() {
                    Some((range, last_url)) if range.end == start && last_url == url => {
                        range.end = text.len();
                    }
                    _ => links.push((start..text.len(), url.clone())),
                }
            }
        }
        out.push(TextRun {
            len: run.text.len(),
            font: f,
            color,
            // Inline code's wash is painted as ROUNDED quads by the canvas
            // underlay (`code_wash_underlay`) — a run background here could
            // only be a square box.
            background_color: None,
            underline: is_link.then_some(UnderlineStyle {
                color: Some(theme.text_muted),
                thickness: px(1.0),
                wavy: false,
            }),
            strikethrough: run.style.strikethrough.then_some(gpui::StrikethroughStyle {
                thickness: px(1.0),
                color: Some(theme.text_muted),
            }),
        });
    }
    FlatText {
        text: text.into(),
        runs: out,
        links,
        code_ranges,
    }
}

/// Flatten through the cross-frame cache when one is wired: settled blocks
/// reuse text + runs untouched (O(1) per block per frame); only blocks the
/// incremental parser invalidated rebuild.
fn flatten_cached(
    runs: &[InlineRun],
    base_weight: FontWeight,
    top_ix: usize,
    ix: usize,
    opts: &RenderOptions,
    theme: &Theme,
) -> Rc<FlatText> {
    match &opts.cache {
        Some(cache) => {
            let mut cache = cache.borrow_mut();
            cache.sync_palette();
            cache
                .flats
                .entry((opts.row_key.clone(), top_ix, ix))
                .or_insert_with(|| Rc::new(flatten_runs_weighted(runs, theme, base_weight)))
                .clone()
        }
        None => Rc::new(flatten_runs_weighted(runs, theme, base_weight)),
    }
}

/// Veiled, clickable text for a flattened block (no sizing wrapper).
fn flat_text_element(
    flat: &FlatText,
    ix: usize,
    opts: &RenderOptions,
    theme: &Theme,
) -> AnyElement {
    // Streaming veil: opacity-only recolor of the runs covering newly appended
    // chunks. Same text, same fonts, same lengths — layout is untouched.
    // Settled elements return no spans and reuse the cached runs unsplit.
    let text_runs = match &opts.veil {
        Some(veil) => {
            let spans = veil.borrow_mut().advance(ix, &flat.text, opts.now);
            apply_veil(flat.runs.clone(), &spans)
        }
        None => flat.runs.clone(),
    };
    let styled = StyledText::new(flat.text.clone()).with_runs(text_runs);
    let layout = styled.layout().clone();
    let text_el: AnyElement = if flat.links.is_empty() {
        styled.into_any_element()
    } else {
        let (ranges, urls): (Vec<_>, Vec<_>) = flat.links.iter().cloned().unzip();
        let id: SharedString = format!("{}-t{ix}", opts.row_key).into();
        let on_link = opts.on_link.clone();
        InteractiveText::new(id, styled)
            .on_click(ranges, move |clicked_ix, window, cx| {
                let Some(url) = urls.get(clicked_ix) else {
                    return;
                };
                if let Some(on_link) = &on_link {
                    on_link(url, window, cx);
                } else {
                    open_external_markdown_url(url, cx);
                }
            })
            .into_any_element()
    };
    // Underlay canvas: inline-code washes + the selection wash, painted
    // BEFORE the text (earlier sibling ⇒ underneath), reading glyph geometry
    // from the text's own layout handle. Pure paint — never in layout. The
    // same paint pass re-registers the frame-scoped window mouse listeners
    // that drive text selection (round 18; see markdown/selection.rs).
    let sel_key: std::sync::Arc<str> = format!("{}:{ix}", opts.row_key).into();
    let code_ranges = flat.code_ranges.clone();
    let flat_text = flat.text.clone();
    let wash = inline_code_wash(theme);
    let sel_wash = selection_wash(theme);
    let underlay = canvas(
        |_, _, _| (),
        move |_, _, window, _| {
            for range in &code_ranges {
                for rect in range_rects(&layout, range, INLINE_CODE_PAD_X, INLINE_CODE_INSET_Y) {
                    window.paint_quad(quad(
                        rect,
                        px(INLINE_CODE_RADIUS),
                        wash,
                        px(0.0),
                        gpui::transparent_black(),
                        BorderStyle::default(),
                    ));
                }
            }
            if let Some(range) = super::selection::wash_range(&sel_key) {
                for rect in range_rects(&layout, &range, 0.0, 0.0) {
                    window.paint_quad(quad(
                        rect,
                        px(0.0),
                        sel_wash,
                        px(0.0),
                        gpui::transparent_black(),
                        BorderStyle::default(),
                    ));
                }
            }
            // Register this element into the frame's document-ordered
            // registry (paint order IS document order), then the frame's
            // mouse listeners.
            let clip = window.content_mask().bounds;
            REGISTRY.with(|r| {
                r.borrow_mut().push(RegEntry {
                    key: sel_key.clone(),
                    text: flat_text.clone(),
                    layout: layout.clone(),
                    clip,
                })
            });
            register_selection_listeners(window, &sel_key, &flat_text, &layout, clip);
        },
    )
    .absolute()
    .size_full();
    div()
        .relative()
        .child(underlay)
        .child(text_el)
        .into_any_element()
}

/// Selection tint: the accent hue under the glyphs, dark-panel strength.
fn selection_wash(theme: &Theme) -> Hsla {
    theme.accent.opacity(0.35) // indigo-400
}

/// Selection support for a plain (non-markdown) text element — the user
/// bubble. Paints the selection wash under the glyphs, registers the element
/// into the frame's document-ordered registry (so drags span into adjacent
/// markdown rows and Cmd+C joins in order), and re-registers the mouse
/// listeners. Call from a paint-phase canvas that sits UNDER the text.
pub(crate) fn paint_text_selection(
    window: &mut Window,
    key: &std::sync::Arc<str>,
    text: &SharedString,
    layout: &gpui::TextLayout,
    theme: &Theme,
) {
    if let Some(range) = super::selection::wash_range(key) {
        for rect in range_rects(layout, &range, 0.0, 0.0) {
            window.paint_quad(quad(
                rect,
                px(0.0),
                selection_wash(theme),
                px(0.0),
                gpui::transparent_black(),
                BorderStyle::default(),
            ));
        }
    }
    let clip = window.content_mask().bounds;
    REGISTRY.with(|r| {
        r.borrow_mut().push(RegEntry {
            key: key.clone(),
            text: text.clone(),
            layout: layout.clone(),
            clip,
        })
    });
    register_selection_listeners(window, key, text, layout, clip);
}

/// Selectable plain text for surfaces that are not markdown (Studio prompt
/// bubbles). Same registry / wash / mouse path as the chat user bubble, so a
/// drag highlights, spans into neighboring prompts, and Cmd+C copies.
pub(crate) fn selectable_plain_text(
    key: impl Into<std::sync::Arc<str>>,
    text: SharedString,
    theme: &Theme,
) -> AnyElement {
    let run = TextRun {
        len: text.len(),
        font: font(theme.font_sans.clone()),
        color: theme.text,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let styled = StyledText::new(text.clone()).with_runs(vec![run]);
    selectable_styled_text(key, text, styled, theme)
}

/// Selection chrome around an already-styled text element (syntax-highlighted
/// code lines). Same registry / wash / mouse path as [`selectable_plain_text`].
pub(crate) fn selectable_styled_text(
    key: impl Into<std::sync::Arc<str>>,
    text: SharedString,
    styled: StyledText,
    theme: &Theme,
) -> AnyElement {
    let layout = styled.layout().clone();
    let sel_key: std::sync::Arc<str> = key.into();
    let sel_theme = theme.clone();
    let underlay = canvas(
        |_, _, _| (),
        move |_, _, window, _| {
            paint_text_selection(window, &sel_key, &text, &layout, &sel_theme);
        },
    )
    .absolute()
    .size_full();
    div()
        .relative()
        .child(underlay)
        .child(styled)
        .into_any_element()
}

/// One painted text element, registered per frame in document order — the
/// continuity model that lets a drag span paragraphs/list items (Zed gets
/// this for free from its single-element markdown; our tree rebuilds it).
struct RegEntry {
    key: std::sync::Arc<str>,
    text: SharedString,
    layout: gpui::TextLayout,
    /// Overflow clip at paint time (collapsed Studio prompts, list viewport).
    /// Hit-testing uses this so a clipped bubble cannot steal clicks below it.
    clip: Bounds<gpui::Pixels>,
}

thread_local! {
    static REGISTRY: RefCell<Vec<RegEntry>> = const { RefCell::new(Vec::new()) };
    static SCROLL_HOST: RefCell<Option<SelectionScrollHost>> = const { RefCell::new(None) };
    static AUTOSCROLL_ARMED: Cell<bool> = const { Cell::new(false) };
    /// Overlay bounds painted this frame (modals, menus). Window-level
    /// markdown listeners hit-test layout geometry and ignore z-order, so a
    /// drag that starts on a dialog over the transcript would otherwise
    /// select both. Later-painted occluders win.
    static OCCLUDERS: RefCell<Vec<Bounds<gpui::Pixels>>> = const { RefCell::new(Vec::new()) };
}

/// The list that should scroll when a drag-select hits its reading-band edge.
#[derive(Clone)]
struct SelectionScrollHost {
    top: f32,
    bottom: f32,
    /// Returns whether the list actually moved. False at either end so the
    /// hold-still loop does not spin frames against a clamp.
    scroll: Rc<dyn Fn(f32, &mut gpui::App) -> bool>,
}

/// Register the visible reading band that autoscrolls during a text-selection
/// drag. The list owner calls this each frame; `scroll` receives a signed
/// pixel delta (negative = toward earlier content) and returns whether the
/// list moved.
pub fn bind_selection_scroll(
    top: f32,
    bottom: f32,
    scroll: impl Fn(f32, &mut gpui::App) -> bool + 'static,
) {
    SCROLL_HOST.with(|host| {
        *host.borrow_mut() = Some(SelectionScrollHost {
            top,
            bottom,
            scroll: Rc::new(scroll),
        });
    });
}

/// Scroll a virtualized list by `delta` px in scrollbar space (negative =
/// toward earlier content). Uses the pixel offset, not `scroll_by`, so a
/// bottom-aligned list that is glued to the end actually moves.
pub fn scroll_list_by(list: &gpui::ListState, delta: f32) -> bool {
    let cur_y = f32::from(list.scroll_px_offset_for_scrollbar().y);
    let max = f32::from(list.max_offset_for_scrollbar().y).max(0.0);
    let old_top = (-cur_y).clamp(0.0, max);
    let new_top = (old_top + delta).clamp(0.0, max);
    if (new_top - old_top).abs() < 0.25 {
        return false;
    }
    list.set_offset_from_scrollbar(point(px(0.0), px(-new_top)));
    true
}

fn apply_selection_autoscroll(window: &mut Window, cx: &mut gpui::App) -> bool {
    if !super::selection::is_dragging() {
        return false;
    }
    let Some(host) = SCROLL_HOST.with(|h| h.borrow().clone()) else {
        return false;
    };
    let pos = window.mouse_position();
    let delta = super::selection::autoscroll_delta(f32::from(pos.y), host.top, host.bottom);
    if delta.abs() < 0.5 {
        return false;
    }
    if !(host.scroll)(delta, cx) {
        return false;
    }
    if super::selection::is_dragging()
        && let Some(head) = registry_point(pos)
    {
        let _ = resolve_current_drag(head);
    }
    window.refresh();
    true
}

fn schedule_selection_autoscroll(window: &mut Window) {
    if AUTOSCROLL_ARMED.get() {
        return;
    }
    AUTOSCROLL_ARMED.set(true);
    window.on_next_frame(|window, cx| {
        AUTOSCROLL_ARMED.set(false);
        if apply_selection_autoscroll(window, cx) {
            schedule_selection_autoscroll(window);
        }
    });
}

/// A zero-size canvas that clears the selection registry — paint it FIRST in
/// the transcript root (before any markdown), so each frame's registry holds
/// exactly that frame's visible text elements in paint order.
pub fn selection_frame_reset() -> impl IntoElement {
    canvas(
        |_, _, _| (),
        |_, _, window, _| {
            REGISTRY.with(|r| r.borrow_mut().clear());
            OCCLUDERS.with(|o| o.borrow_mut().clear());
            // Window-level drag listeners live here (once per frame), not on
            // each text element: the original click target can scroll out of
            // the virtualized window, and its listeners would vanish with it.
            register_drag_listeners(window);
        },
    )
    .absolute()
    .w(px(0.0))
    .h(px(0.0))
}

/// Paint-order occluder for window-level markdown selection. Place inside an
/// overlay that already `.occlude()`s hit-testing (dialogs, menus) so a drag
/// that starts on the overlay cannot also select the transcript underneath.
pub fn selection_occluder() -> impl IntoElement {
    canvas(
        |_, _, _| (),
        |bounds, _, _, _| {
            if !bounds.is_empty() {
                OCCLUDERS.with(|o| o.borrow_mut().push(bounds));
            }
        },
    )
    .absolute()
    .inset_0()
}

fn pointer_hits_occluder(position: gpui::Point<gpui::Pixels>) -> bool {
    OCCLUDERS.with(|o| pointer_hits_any_occluder(position, &o.borrow()))
}

fn pointer_hits_any_occluder(
    position: gpui::Point<gpui::Pixels>,
    occluders: &[Bounds<gpui::Pixels>],
) -> bool {
    occluders.iter().any(|bounds| bounds.contains(&position))
}

/// `(element index, byte offset)` for a window position: the registered
/// element whose vertical band contains it, else the nearest by vertical
/// distance (a drag past the gutter or between blocks clamps sensibly).
fn registry_point(position: gpui::Point<gpui::Pixels>) -> Option<(usize, usize)> {
    REGISTRY.with(|r| {
        let reg = r.borrow();
        let mut best: Option<(usize, f32)> = None;
        for (ei, entry) in reg.iter().enumerate() {
            let b = entry.layout.bounds().intersect(&entry.clip);
            if b.is_empty() {
                continue;
            }
            let dy = if position.y < b.top() {
                f32::from(b.top() - position.y)
            } else if position.y > b.bottom() {
                f32::from(position.y - b.bottom())
            } else {
                0.0
            };
            if best.is_none_or(|(_, d)| dy < d) {
                best = Some((ei, dy));
            }
            if dy == 0.0 {
                break;
            }
        }
        let (ei, _) = best?;
        let ix = match reg[ei].layout.index_for_position(position) {
            Ok(ix) | Err(ix) => ix,
        };
        Some((ei, ix))
    })
}

/// Resolve the live drag: the pointer names a visible element, then the
/// bound document (full thread, not the painted window) fills every
/// in-between slice — including rows virtualization never painted.
fn resolve_current_drag(head: (usize, usize)) -> bool {
    let Some((_, _, gran)) = super::selection::drag_owner() else {
        return false;
    };
    REGISTRY.with(|r| {
        let reg = r.borrow();
        if reg.is_empty() {
            return false;
        }
        let head_ei = head.0.min(reg.len() - 1);
        let head_range = super::selection::snap_unit(&reg[head_ei].text, head.1, gran);
        let visible: Vec<(&str, &str)> = reg
            .iter()
            .map(|e| (e.key.as_ref(), e.text.as_ref()))
            .collect();
        super::selection::resolve_against_document(&visible, &reg[head_ei].key, head_range)
    })
}

/// Mouse-down for one painted text element. Drag move/up live on the
/// frame reset so they survive the element virtualizing out.
fn register_selection_listeners(
    window: &mut Window,
    key: &std::sync::Arc<str>,
    text: &SharedString,
    layout: &gpui::TextLayout,
    clip: Bounds<gpui::Pixels>,
) {
    use gpui::{DispatchPhase, MouseButton, MouseDownEvent};
    {
        let (key, text, layout, clip) = (key.clone(), text.clone(), layout.clone(), clip);
        window.on_mouse_event(move |e: &MouseDownEvent, phase, window, _cx| {
            if phase != DispatchPhase::Bubble || e.button != MouseButton::Left {
                return;
            }
            // Overlay geometry wins over text layout: `.occlude()` only
            // stops element hit-testing, not these window-level listeners.
            if pointer_hits_occluder(e.position) {
                if super::selection::clear() {
                    window.refresh();
                }
                return;
            }
            let hit = layout.bounds().intersect(&clip);
            if !hit.is_empty() && hit.contains(&e.position) {
                let ix = match layout.index_for_position(e.position) {
                    Ok(ix) | Err(ix) => ix,
                };
                match e.click_count {
                    2 => {
                        let range = super::selection::word_range(&text, ix);
                        super::selection::begin_with_span(&key, &text, range);
                    }
                    n if n >= 3 => {
                        super::selection::begin_paragraph(&key, &text);
                    }
                    _ => super::selection::begin(&key, ix),
                }
                window.refresh();
            } else if super::selection::clear_if_owner(&key) {
                window.refresh();
            }
        });
    }
}

/// Window-level drag tracking — registered once per frame from the reset
/// canvas so a drag keeps going after its original element virtualizes out.
fn register_drag_listeners(window: &mut Window) {
    use gpui::{DispatchPhase, MouseMoveEvent, MouseUpEvent};
    window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
        if phase != DispatchPhase::Bubble || !e.dragging() {
            return;
        }
        if !super::selection::is_dragging() {
            return;
        }
        if let Some(head) = registry_point(e.position)
            && resolve_current_drag(head)
        {
            window.refresh();
        }
        if apply_selection_autoscroll(window, cx) {
            schedule_selection_autoscroll(window);
        }
    });
    window.on_mouse_event(move |_: &MouseUpEvent, phase, _window, _cx| {
        if phase != DispatchPhase::Bubble {
            return;
        }
        if let Some(_text) = super::selection::end_any_drag() {
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            _cx.write_to_primary(gpui::ClipboardItem::new_string(_text));
        }
    });
}

/// Byte ranges `[start, end)` of each visual row in a shaped layout, in
/// document order. Hard newlines advance by `len + 1`; wrap boundaries are
/// the first byte of the *next* visual row (gpui reports that index on the
/// previous row, which is why a y-probe walk skips the first wrapped glyph).
fn visual_line_byte_ranges(hard_lines: &[(usize, Vec<usize>)]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut hard_start = 0usize;
    for (len, wraps) in hard_lines {
        let mut starts = vec![0usize];
        for &wrap in wraps {
            if wrap > *starts.last().unwrap() && wrap <= *len {
                starts.push(wrap);
            }
        }
        starts.push(*len);
        for pair in starts.windows(2) {
            out.push((hard_start + pair[0], hard_start + pair[1]));
        }
        hard_start = hard_start.saturating_add(*len).saturating_add(1);
    }
    out
}

/// The wash boxes for one byte range: one box per visual line the range
/// covers (soft wraps split it), in window coordinates from the laid-out
/// text's own geometry. `pad_x` overhangs the box horizontally (inline code);
/// `inset_y` shrinks it vertically — both 0 for a selection wash, which wants
/// full-line-height boxes that tile seamlessly across wrapped rows.
pub(crate) fn range_rects(
    layout: &gpui::TextLayout,
    range: &Range<usize>,
    pad_x: f32,
    inset_y: f32,
) -> Vec<Bounds<gpui::Pixels>> {
    if range.start >= range.end {
        return Vec::new();
    }
    let line_height = layout.line_height();
    let bounds = layout.bounds();
    let hard = layout.line_layouts();
    if hard.is_empty() {
        return Vec::new();
    }
    let spec: Vec<(usize, Vec<usize>)> = hard
        .iter()
        .map(|wl| {
            let wraps = wl
                .wrap_boundaries()
                .iter()
                .filter_map(|b| {
                    let run = wl.runs().get(b.run_ix)?;
                    let glyph = run.glyphs.get(b.glyph_ix)?;
                    Some(glyph.index)
                })
                .collect();
            (wl.len(), wraps)
        })
        .collect();
    let origin_y = layout
        .position_for_index(0)
        .map(|p| p.y)
        .unwrap_or(bounds.origin.y);

    let mut rects = Vec::new();
    for (row, (vs, ve)) in visual_line_byte_ranges(&spec).into_iter().enumerate() {
        let from = vs.max(range.start);
        let to = ve.min(range.end);
        if from >= to {
            continue;
        }
        let y = origin_y + line_height * row as f32;
        let left = if from == vs {
            // Start of a visual row — including wrap starts, whose
            // `position_for_index` reports the previous row's trailing
            // edge. Pin to the layout's left so the first glyph is covered.
            bounds.left()
        } else {
            match layout.position_for_index(from) {
                Some(p) => p.x,
                None => continue,
            }
        };
        let right = if to == ve {
            // End of this visual row: the wrap-start index (or the hard
            // line's end) reports the trailing x on this row.
            match layout.position_for_index(to.min(layout.len())) {
                Some(p) => {
                    if (f32::from(p.y) - f32::from(y)).abs() <= f32::from(line_height) * 0.6 {
                        p.x
                    } else {
                        layout
                            .position_for_index(to.saturating_sub(1))
                            .map(|p| p.x.max(left + px(1.0)))
                            .unwrap_or(left)
                    }
                }
                None => layout
                    .position_for_index(to.saturating_sub(1))
                    .map(|p| p.x.max(left + px(1.0)))
                    .unwrap_or(left),
            }
        } else {
            match layout.position_for_index(to) {
                Some(p) => p.x,
                None => continue,
            }
        };
        if right > left {
            rects.push(Bounds::new(
                point(left - px(pad_x), y + px(inset_y)),
                size(
                    right - left + px(2.0 * pad_x),
                    line_height - px(2.0 * inset_y),
                ),
            ));
        }
    }
    rects
}

#[allow(clippy::too_many_arguments)]
fn text_element(
    runs: &[InlineRun],
    size: f32,
    line_height: f32,
    bold_default: bool,
    top_ix: usize,
    ix: usize,
    opts: &RenderOptions,
    theme: &Theme,
) -> AnyElement {
    let weight = if bold_default {
        FontWeight::SEMIBOLD
    } else {
        FontWeight::NORMAL
    };
    let flat = flatten_cached(runs, weight, top_ix, ix, opts, theme);
    let inner = flat_text_element(&flat, ix, opts, theme);
    div()
        .text_size(px(size))
        .line_height(px(line_height))
        .child(inner)
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn render_code_block(
    language: Option<&str>,
    code: &str,
    top_ix: usize,
    ix: usize,
    opts: &RenderOptions,
    theme: &Theme,
    highlight: CodeHighlight,
) -> AnyElement {
    let mono = font(theme.font_mono.clone());
    // Per-line strings + runs through the cross-frame cache (validity: code
    // length + highlight slice identity — a fresh highlight Arc re-derives).
    let hl_key = highlight.map_or((0, 0), |h| (h.as_ptr() as usize, h.len()));
    let build = || {
        let lines: Vec<(SharedString, Vec<TextRun>)> = code
            .split('\n')
            .enumerate()
            .map(|(li, line)| {
                let spans = highlight
                    .and_then(|h| h.get(li))
                    .map(|t| &t[..])
                    .unwrap_or(&[]);
                (
                    SharedString::from(line.to_string()),
                    runs_for_syntax_line(line, spans, &mono, theme),
                )
            })
            .collect();
        Rc::new(CachedCode {
            code_len: code.len(),
            hl_key,
            lines,
        })
    };
    let cached: Rc<CachedCode> = match &opts.cache {
        Some(cache) => {
            let mut cache = cache.borrow_mut();
            cache.sync_palette();
            let entry = cache
                .code
                .entry((opts.row_key.clone(), top_ix, ix))
                .or_insert_with(&build);
            if entry.code_len != code.len() || entry.hl_key != hl_key {
                *entry = build();
            }
            entry.clone()
        }
        None => build(),
    };
    // Streaming veil over appended code, tracked on the whole code text and
    // sliced per line below (paint-only run recolor — heights stay exact).
    let veil_spans = match &opts.veil {
        Some(veil) => veil.borrow_mut().advance(ix, code, opts.now),
        None => Vec::new(),
    };
    let scroll_id: SharedString = format!("{}-code{ix}", opts.row_key).into();
    let row_key = opts.row_key.clone();
    let block_ix = ix;
    let line_theme = theme.clone();
    // Copy affordance (round 9; no source counterpart — the original block is
    // header + body only): a small ghost button in the block's top-right,
    // absolutely overlaid so clicking / the "Copied" flash never shifts
    // layout. Sits centered in the header when there is one, floats over the
    // first code line otherwise.
    let copy_button = opts.copy.clone().map(|copy| {
        let copied = copy.copied_ix == Some(ix);
        let code_text: SharedString = code.to_string().into();
        let handler = copy.handler.clone();
        let fade_key = format!("{}-copy{ix}", opts.row_key);
        div()
            .id(SharedString::from(fade_key.clone()))
            .absolute()
            .top(px(3.0))
            .right(px(5.0))
            .h(px(20.0))
            .px(px(6.0))
            .rounded(px(5.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .cursor_pointer()
            // Ghost-button hover wash fades over transition-colors like every
            // other interactive chrome (crate::motion hover fades).
            .bg(crate::motion::hover_blend(
                &fade_key,
                gpui::transparent_black(),
                crate::theme::ink(0.08),
            ))
            .on_hover(crate::motion::hover_listener(fade_key))
            .text_size(px(10.5))
            .text_color(theme.text_muted)
            .on_click(move |_, window, cx| handler(ix, code_text.clone(), window, cx))
            .child(
                crate::icons::icon(if copied {
                    crate::icons::CHECK
                } else {
                    crate::icons::COPY
                })
                .size(px(12.0))
                .text_color(theme.text_muted),
            )
            .when(copied, |el| el.child(SharedString::from("Copied")))
    });
    div()
        .rounded(px(10.0))
        // Faint white wash over the near-black panel ≈ #101010 (zeron's code
        // surface), with the hairline border.
        .bg(crate::theme::ink(0.035))
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
        .relative()
        .when_some(language, |el, lang| {
            el.child(
                div()
                    .px(px(CODE_PADDING_X))
                    .py(px(5.0))
                    .border_b_1()
                    .border_color(theme.border)
                    // A whisper of tone separation between header and body.
                    .bg(crate::theme::ink(0.02))
                    .text_size(px(11.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(lang.to_string())),
            )
        })
        .child(
            div()
                .id(scroll_id)
                .overflow_x_scroll()
                .px(px(CODE_PADDING_X))
                .py(px(CODE_PADDING_Y))
                .font_family(theme.font_mono.clone())
                .text_size(px(CODE_TEXT_SIZE))
                .line_height(px(CODE_LINE_HEIGHT))
                .whitespace_nowrap()
                .flex()
                .flex_col()
                .children((0..cached.lines.len()).scan(0usize, move |off, li| {
                    let (line, runs) = &cached.lines[li];
                    let start = *off;
                    *off = start + line.len() + 1; // +1 for the '\n'
                    let local = slice_spans(&veil_spans, start, start + line.len());
                    let runs = apply_veil(runs.clone(), &local);
                    let styled = StyledText::new(line.clone()).with_runs(runs);
                    let key: std::sync::Arc<str> = format!("{row_key}:{block_ix}:c{li}").into();
                    Some(
                        div()
                            .h(px(CODE_LINE_HEIGHT))
                            .flex_none()
                            .child(selectable_styled_text(
                                key,
                                line.clone(),
                                styled,
                                &line_theme,
                            )),
                    )
                })),
        )
        // Overlay LAST so it paints above the header/body.
        .children(copy_button)
        .into_any_element()
}

/// Paint color for a token class — the soft syntax palette (round 9: the
/// original's mdTheme code blocks are monochrome `#e7e7e7`, but the user
/// asked for color; these are the diff pane's hues, now shared by both).
pub fn token_color(kind: HighlightKind, theme: &Theme) -> Hsla {
    theme.syntax.color(kind)
}

/// Build the exact-cover `TextRun` list for one code line from its tokens.
/// Same font everywhere — recoloring can never change layout.
/// Build paint-only runs from the neutral Tree-sitter contract.
pub fn runs_for_syntax_line(
    line: &str,
    spans: &[HighlightSpan],
    mono: &gpui::Font,
    theme: &Theme,
) -> Vec<TextRun> {
    runs_for_syntax_line_with_plain(line, spans, mono, theme.text, theme)
}

pub fn runs_for_syntax_line_with_plain(
    line: &str,
    spans: &[HighlightSpan],
    mono: &gpui::Font,
    plain_color: Hsla,
    theme: &Theme,
) -> Vec<TextRun> {
    let plain = |len: usize| TextRun {
        len,
        font: mono.clone(),
        color: plain_color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let mut runs = Vec::new();
    let mut at = 0usize;
    for span in spans {
        if span.range.start > at {
            runs.push(plain(span.range.start - at));
        }
        let mut run = plain(span.range.len());
        run.color = token_color(span.kind, theme);
        runs.push(run);
        at = span.range.end;
    }
    if at < line.len() {
        runs.push(plain(line.len() - at));
    }
    runs.retain(|run| run.len > 0);
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::parser::{Block, InlineRun, InlineStyle};

    #[test]
    fn only_http_https_mailto_are_safe_external_urls() {
        assert!(is_safe_external_url("https://example.com/a"));
        assert!(is_safe_external_url("http://localhost:3000"));
        assert!(is_safe_external_url("mailto:dev@example.com"));
        assert!(!is_safe_external_url("#executive-summary"));
        assert!(!is_safe_external_url("comet"));
        assert!(!is_safe_external_url("./plan.md"));
        assert!(!is_safe_external_url("javascript:alert(1)"));
        assert!(!is_safe_external_url("file:///tmp/x"));
        assert!(!is_safe_external_url("https:"));
        assert!(!is_safe_external_url(""));
    }

    #[test]
    fn markdown_fragment_reads_hash_and_relative_hashes() {
        assert_eq!(
            markdown_fragment("#executive-summary").as_deref(),
            Some("executive-summary")
        );
        assert_eq!(
            markdown_fragment("plan.md#decision-matrix").as_deref(),
            Some("decision-matrix")
        );
        assert_eq!(
            markdown_fragment("#file%20touch").as_deref(),
            Some("file touch")
        );
        assert_eq!(markdown_fragment("https://x.test/#nope"), None);
        assert_eq!(markdown_fragment("comet"), None);
    }

    #[test]
    fn gfm_heading_slug_matches_toc_destinations() {
        assert_eq!(gfm_heading_slug("Executive Summary"), "executive-summary");
        assert_eq!(
            gfm_heading_slug("File Touch Map (Fictional)"),
            "file-touch-map-fictional"
        );
        assert_eq!(
            gfm_heading_slug("Architecture Sketch"),
            "architecture-sketch"
        );
        // Em dash is punctuation; the two surrounding spaces become `--`.
        assert_eq!(
            gfm_heading_slug("Appendix A — Fake Sequence"),
            "appendix-a--fake-sequence"
        );
        assert_eq!(
            gfm_heading_slug("Appendix B — Nested Spec"),
            "appendix-b--nested-spec"
        );
        assert_eq!(
            heading_index_for_fragment(
                &super::super::parse_full("## Appendix A — Fake Sequence\n\nbody\n"),
                "appendix-a--fake-sequence"
            ),
            Some(0)
        );
        assert_eq!(
            heading_index_for_fragment(
                &super::super::parse_full("## Appendix A — Fake Sequence\n\nbody\n"),
                "appendix-a-fake-sequence"
            ),
            Some(0)
        );
        let tree = super::super::parse_full(
            "## Executive Summary\n\ntext\n\n## Executive Summary\n\nmore\n",
        );
        assert_eq!(
            top_heading_slugs(&tree)
                .into_iter()
                .map(|(_, s)| s)
                .collect::<Vec<_>>(),
            vec!["executive-summary", "executive-summary-1"]
        );
        assert_eq!(
            heading_index_for_fragment(&tree, "executive-summary"),
            Some(0)
        );
        assert_eq!(
            heading_index_for_fragment(&tree, "executive-summary-1"),
            Some(2)
        );
    }

    #[test]
    fn collect_block_selectables_matches_render_keys() {
        let para = Block::Paragraph {
            runs: vec![InlineRun {
                text: "hello".into(),
                style: Default::default(),
            }],
        };
        let code = Block::CodeBlock {
            language: Some("rs".into()),
            code: "a\nb".into(),
        };
        let mut out = Vec::new();
        collect_block_selectables(&para, 0, "row1", &mut out);
        collect_block_selectables(&code, 1, "row1", &mut out);
        assert_eq!(
            out,
            vec![
                ("row1:0".into(), "hello".into()),
                ("row1:1:c0".into(), "a".into()),
                ("row1:1:c1".into(), "b".into()),
            ]
        );
    }

    #[test]
    fn visual_line_ranges_split_wraps_and_hard_breaks() {
        // One hard line wrapped at byte 10, then a second hard line of 6.
        let lines = visual_line_byte_ranges(&[(20, vec![10]), (6, vec![])]);
        assert_eq!(lines, vec![(0, 10), (10, 20), (21, 27)]);
    }

    #[test]
    fn visual_line_ranges_keep_the_wrap_start_on_the_next_row() {
        // The wrap index is the first glyph of the next visual row — it must
        // start that row, not be consumed as the previous row's exclusive end
        // plus one (that skipped the first highlighted character).
        let lines = visual_line_byte_ranges(&[(15, vec![8])]);
        assert_eq!(lines, vec![(0, 8), (8, 15)]);
    }

    #[test]
    fn code_line_runs_cover_exactly() {
        let theme = Theme::dark();
        let mono = font(theme.font_mono.clone());
        let line = r#"let x = "hi"; // done"#;
        let document = zeron_syntax::highlight(zeron_syntax::HighlightRequest {
            source: line,
            path: None,
            fence_tag: Some("rust"),
        })
        .unwrap();
        let runs = runs_for_syntax_line(line, &document.lines[0], &mono, &theme);
        let total: usize = runs.iter().map(|r| r.len).sum();
        assert_eq!(total, line.len());
        assert!(
            runs.iter().all(|r| r.font == mono),
            "highlight must not change fonts"
        );
        // At least one non-plain color made it through.
        assert!(runs.iter().any(|r| r.color != theme.text));
    }

    #[test]
    fn tree_sitter_runs_are_rich_and_paint_only() {
        let theme = Theme::dark();
        let mono = font(theme.font_mono.clone());
        let line = "let widget = build!(42);";
        let document = zeron_syntax::highlight(zeron_syntax::HighlightRequest {
            source: line,
            path: None,
            fence_tag: Some("rust"),
        })
        .unwrap();
        let runs = runs_for_syntax_line(line, &document.lines[0], &mono, &theme);
        assert_eq!(runs.iter().map(|run| run.len).sum::<usize>(), line.len());
        assert!(runs.iter().all(|run| run.font == mono));
        let colors = runs.iter().map(|run| run.color).collect::<Vec<_>>();
        assert!(colors.contains(&theme.syntax.keyword));
        assert!(colors.contains(&theme.syntax.macro_name));
        assert!(colors.contains(&theme.syntax.number));
    }

    #[test]
    fn code_line_runs_with_no_tokens_are_one_plain_run() {
        let theme = Theme::dark();
        let mono = font(theme.font_mono.clone());
        let runs = runs_for_syntax_line("plain text", &[], &mono, &theme);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].len, 10);
    }

    #[test]
    fn flatten_collects_and_merges_inline_code_ranges() {
        let theme = Theme::dark();
        let code = |text: &str| InlineRun {
            text: text.into(),
            style: InlineStyle {
                code: true,
                ..Default::default()
            },
        };
        let plain = |text: &str| InlineRun {
            text: text.into(),
            style: InlineStyle::default(),
        };
        let flat = flatten_runs(
            &[
                plain("use "),
                code("foo"),
                code("()"),
                plain(" and "),
                code("bar"),
            ],
            &theme,
            false,
        );
        // Adjacent code runs merge into ONE wash box; separated ones don't.
        assert_eq!(flat.code_ranges, vec![4..9, 14..17]);
        // Code text is the violet tint; the square run background is gone
        // (the rounded wash is painted by the canvas underlay instead).
        assert_eq!(flat.runs[1].color, inline_code_text(&theme));
        assert_eq!(flat.runs[1].background_color, None);
        assert_eq!(flat.runs[0].color, theme.text);
    }

    #[test]
    fn code_palette_is_colored_and_shared() {
        // Round 9: transcript code blocks paint the soft hues (rose keyword,
        // green string, amber number); comments stay faint neutral.
        let theme = Theme::dark();
        assert_ne!(token_color(HighlightKind::Keyword, &theme), theme.text);
        assert_ne!(
            token_color(HighlightKind::String, &theme),
            token_color(HighlightKind::Keyword, &theme)
        );
        assert_ne!(token_color(HighlightKind::Comment, &theme), theme.text);
    }

    #[test]
    fn flatten_runs_maps_links_and_styles() {
        let theme = Theme::dark();
        let runs = vec![
            InlineRun {
                text: "go ".into(),
                style: InlineStyle::default(),
            },
            InlineRun {
                text: "here".into(),
                style: InlineStyle {
                    link: Some("https://x.dev".into()),
                    ..Default::default()
                },
            },
            InlineRun {
                text: " now".into(),
                style: InlineStyle {
                    bold: true,
                    ..Default::default()
                },
            },
        ];
        let flat = flatten_runs(&runs, &theme, false);
        assert_eq!(flat.text, "go here now");
        assert_eq!(flat.links, vec![(3..7, "https://x.dev".to_string())]);
        let total: usize = flat.runs.iter().map(|r| r.len).sum();
        assert_eq!(total, flat.text.len());
        // Links stay monochrome (foreground + underline), never accent-tinted.
        assert_eq!(flat.runs[1].color, theme.text);
        assert!(flat.runs[1].underline.is_some());
        assert_eq!(flat.runs[2].font.weight, FontWeight::SEMIBOLD);
    }

    #[test]
    fn table_columns_floor_and_padding() {
        // A short column keeps its content width (floored at MIN_COLUMN_CONTENT
        // + padding); a wide one may wrap but no narrower than minColumnWidth.
        let geo = table_columns(&[10.0, 200.0]);
        assert_eq!(geo.naturals, vec![72.0, 224.0]); // 48+24, 200+24
        assert_eq!(geo.minimums, vec![72.0, 96.0]);
        assert_eq!(geo.min_table_width, 168.0);
    }

    #[test]
    fn table_columns_are_content_proportional_not_equal() {
        let geo = table_columns(&[300.0, 60.0, 60.0]);
        // Flex grow factors are the naturals — a prose column gets a larger
        // share than short ones (not equal thirds).
        assert!(geo.naturals[0] > 3.0 * geo.naturals[1] * 0.9);
        assert_eq!(geo.naturals[1], geo.naturals[2]);
    }

    #[test]
    fn table_header_flattens_at_weight_700() {
        let theme = Theme::dark();
        let runs = vec![InlineRun {
            text: "Header".into(),
            style: InlineStyle::default(),
        }];
        let flat = flatten_runs_weighted(&runs, &theme, TABLE_HEADER_WEIGHT);
        assert_eq!(flat.runs[0].font.weight, FontWeight::BOLD);
        // Strong runs inside a 700 header stay 700 (never drop to semibold).
        let bold_runs = vec![InlineRun {
            text: "Strong".into(),
            style: InlineStyle {
                bold: true,
                ..Default::default()
            },
        }];
        let flat = flatten_runs_weighted(&bold_runs, &theme, TABLE_HEADER_WEIGHT);
        assert_eq!(flat.runs[0].font.weight, FontWeight::BOLD);
    }

    #[test]
    fn adjacent_same_link_runs_merge_into_one_range() {
        let theme = Theme::dark();
        let style = InlineStyle {
            link: Some("https://x.dev".into()),
            ..Default::default()
        };
        let runs = vec![
            InlineRun {
                text: "bold".into(),
                style: InlineStyle {
                    bold: true,
                    ..style.clone()
                },
            },
            InlineRun {
                text: " tail".into(),
                style,
            },
        ];
        let flat = flatten_runs(&runs, &theme, false);
        assert_eq!(flat.links, vec![(0..9, "https://x.dev".to_string())]);
    }

    #[test]
    fn overlay_bounds_block_markdown_hit_testing() {
        let overlay = Bounds::new(point(px(100.0), px(80.0)), size(px(360.0), px(160.0)));
        assert!(pointer_hits_any_occluder(
            point(px(180.0), px(120.0)),
            &[overlay]
        ));
        assert!(!pointer_hits_any_occluder(
            point(px(20.0), px(20.0)),
            &[overlay]
        ));
        assert!(!pointer_hits_any_occluder(point(px(180.0), px(120.0)), &[]));
    }
}
