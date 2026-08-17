//! Text selection for rendered markdown (round 18).
//!
//! gpui has no built-in selection for plain text elements. Zed's markdown
//! selects continuously because its whole document is ONE element over one
//! text model; zeron renders a TREE of text elements inside a virtualized
//! list, so this module rebuilds that continuity.
//!
//! The list owner binds a document-order catalog of *(key, text)* from its
//! full row model each frame. Painted elements still register geometry for
//! hit-testing; a drag's head is a visible key, then resolve walks the
//! bound catalog so rows the virtualizer never painted stay in the
//! selection. The wash paints per visible element from its span; copy
//! joins every span in document order.
//!
//! This module is the pure state half (gpui-free, unit-tested); the
//! registry, geometry and mouse listeners live in `render.rs`.

use std::ops::Range;
use std::sync::{Mutex, OnceLock};

/// One element's slice of the selection, in document order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    /// Element key (`{row_key}:{element ix}`).
    pub key: String,
    /// Selected byte range of the element's flat text.
    pub range: Range<usize>,
    /// The element's full flat text (copy source, snapshotted at drag time
    /// so copy still works after the element scrolls out of the registry).
    pub text: String,
}

/// How a drag grows as the pointer moves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Granularity {
    /// Character caret (single click).
    #[default]
    Char,
    /// Whole words (double-click, then drag).
    Word,
    /// Whole elements (triple-click, then drag).
    Paragraph,
}

#[derive(Clone, Default)]
struct MdSelection {
    /// Element that owns the drag (where the mouse went down).
    anchor_key: String,
    /// The initial unit — a caret for char, the clicked word/block otherwise.
    /// Always included in the selection, even when the head moves backward.
    anchor_range: Range<usize>,
    granularity: Granularity,
    dragging: bool,
    /// Resolved spans over the bound document, not just the painted window.
    spans: Vec<Span>,
}

fn state() -> &'static Mutex<Option<MdSelection>> {
    static STATE: OnceLock<Mutex<Option<MdSelection>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(None))
}

fn document() -> &'static Mutex<Vec<(String, String)>> {
    static DOC: OnceLock<Mutex<Vec<(String, String)>>> = OnceLock::new();
    DOC.get_or_init(|| Mutex::new(Vec::new()))
}

/// Replace the document-order catalog of selectable text. The list owner
/// binds this every frame from its full row model (not the virtualized
/// paint window) so a drag can cover rows that have never been on screen.
pub fn bind_document(elements: Vec<(String, String)>) {
    *document().lock().unwrap() = elements;
}

/// The bound document, for resolve / tests.
pub fn bound_document() -> Vec<(String, String)> {
    document().lock().unwrap().clone()
}

/// Resolve the spans for a selection between `a` and `b`, each an
/// `(element index, byte offset)` into `elements` (document-ordered
/// `(key, text)` pairs). Handles either direction; empty slices are skipped.
pub fn resolve_spans(elements: &[(&str, &str)], a: (usize, usize), b: (usize, usize)) -> Vec<Span> {
    resolve_span_ranges(elements, (a.0, a.1..a.1), (b.0, b.1..b.1))
}

/// Like [`resolve_spans`], but each end is a range so a double-clicked word
/// stays selected when the head moves past it (the union of the two units).
pub fn resolve_span_ranges(
    elements: &[(&str, &str)],
    a: (usize, Range<usize>),
    b: (usize, Range<usize>),
) -> Vec<Span> {
    if elements.is_empty() {
        return Vec::new();
    }
    if a.0 == b.0 {
        let text = elements[a.0.min(elements.len() - 1)].1;
        let from = a.1.start.min(b.1.start).min(text.len());
        let to = a.1.end.max(b.1.end).min(text.len());
        if from >= to {
            return Vec::new();
        }
        return vec![Span {
            key: elements[a.0].0.to_string(),
            range: from..to,
            text: text.to_string(),
        }];
    }
    let (start, end) = if a.0 < b.0 { (a, b) } else { (b, a) };
    let mut spans = Vec::new();
    for (ei, (key, text)) in elements.iter().enumerate().take(end.0 + 1).skip(start.0) {
        let from = if ei == start.0 { start.1.start } else { 0 };
        let to = if ei == end.0 { end.1.end } else { text.len() };
        let (from, to) = (from.min(text.len()), to.min(text.len()));
        if from < to {
            spans.push(Span {
                key: (*key).to_string(),
                range: from..to,
                text: (*text).to_string(),
            });
        }
    }
    spans
}

/// Snap `ix` to the current granularity's unit.
pub fn snap_unit(text: &str, ix: usize, granularity: Granularity) -> Range<usize> {
    match granularity {
        Granularity::Char => {
            let ix = ix.min(text.len());
            ix..ix
        }
        Granularity::Word => word_range(text, ix),
        Granularity::Paragraph => 0..text.len(),
    }
}

/// Begin a drag anchored at `(key, ix)`; claims the global selection.
pub fn begin(key: &str, ix: usize) {
    *state().lock().unwrap() = Some(MdSelection {
        anchor_key: key.to_string(),
        anchor_range: ix..ix,
        granularity: Granularity::Char,
        dragging: true,
        spans: Vec::new(),
    });
}

/// Begin with an immediate span (double-click word / triple-click block).
pub fn begin_with_span(key: &str, text: &str, range: Range<usize>) {
    begin_granular(key, text, range, Granularity::Word);
}

/// Triple-click: the whole element, and a drag grows by whole elements.
pub fn begin_paragraph(key: &str, text: &str) {
    begin_granular(key, text, 0..text.len(), Granularity::Paragraph);
}

fn begin_granular(key: &str, text: &str, range: Range<usize>, granularity: Granularity) {
    *state().lock().unwrap() = Some(MdSelection {
        anchor_key: key.to_string(),
        anchor_range: range.clone(),
        granularity,
        dragging: true,
        spans: vec![Span {
            key: key.to_string(),
            range,
            text: text.to_string(),
        }],
    });
}

/// The live drag's anchor, if `key` owns it: `(anchor byte offset)`.
pub fn drag_anchor(key: &str) -> Option<usize> {
    let guard = state().lock().unwrap();
    let sel = guard.as_ref()?;
    (sel.dragging && sel.anchor_key == key).then_some(sel.anchor_range.start)
}

/// Whether a drag is in flight (used to keep the edge-autoscroll loop alive).
pub fn is_dragging() -> bool {
    state()
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|sel| sel.dragging)
}

/// The drag's owning element, initial unit, and granularity, if a drag is live.
pub fn drag_owner() -> Option<(String, Range<usize>, Granularity)> {
    let guard = state().lock().unwrap();
    let sel = guard.as_ref()?;
    sel.dragging.then(|| {
        (
            sel.anchor_key.clone(),
            sel.anchor_range.clone(),
            sel.granularity,
        )
    })
}

/// Resolve the live drag against the bound document (falling back to
/// `visible` when the catalog is empty). The original anchor is never
/// rewritten, so reversing a drag cannot invert the highlight.
pub fn resolve_against_document(
    visible: &[(&str, &str)],
    head_key: &str,
    head_range: Range<usize>,
) -> bool {
    let Some((anchor_key, anchor_range, _)) = drag_owner() else {
        return false;
    };
    let doc = document().lock().unwrap().clone();
    let fallback: Vec<(String, String)> = visible
        .iter()
        .map(|(k, t)| ((*k).to_string(), (*t).to_string()))
        .collect();
    let source = if doc.is_empty() { &fallback } else { &doc };
    let elements: Vec<(&str, &str)> = source
        .iter()
        .map(|(k, t)| (k.as_str(), t.as_str()))
        .collect();
    let Some(anchor_ei) = elements.iter().position(|(k, _)| *k == anchor_key) else {
        return false;
    };
    let Some(head_ei) = elements.iter().position(|(k, _)| *k == head_key) else {
        return false;
    };
    update_spans(resolve_span_ranges(
        &elements,
        (anchor_ei, anchor_range),
        (head_ei, head_range),
    ))
}

/// Pixels to scroll this frame so a drag-select can grow past the viewport.
/// Negative = toward earlier content. Zero when the pointer is inside the
/// reading band; speed ramps as the pointer enters (or passes) the edge zone.
pub fn autoscroll_delta(pointer_y: f32, band_top: f32, band_bottom: f32) -> f32 {
    const ZONE: f32 = 72.0;
    const MAX: f32 = 18.0;
    if !pointer_y.is_finite() || band_bottom <= band_top + 1.0 {
        return 0.0;
    }
    if pointer_y < band_top + ZONE {
        let t = ((band_top + ZONE - pointer_y) / ZONE).clamp(0.0, 2.0);
        -MAX * t
    } else if pointer_y > band_bottom - ZONE {
        let t = ((pointer_y - (band_bottom - ZONE)) / ZONE).clamp(0.0, 2.0);
        MAX * t
    } else {
        0.0
    }
}

/// Replace the resolved spans (drag update). Returns true if they changed.
pub fn update_spans(spans: Vec<Span>) -> bool {
    let mut guard = state().lock().unwrap();
    let Some(sel) = guard.as_mut() else {
        return false;
    };
    if sel.spans == spans {
        return false;
    }
    sel.spans = spans;
    true
}

/// End the drag for `key`'s claim; returns the joined text if non-empty.
pub fn end_drag(key: &str) -> Option<String> {
    let mut guard = state().lock().unwrap();
    let sel = guard.as_mut()?;
    if sel.anchor_key != key || !sel.dragging {
        return None;
    }
    finish_drag(&mut guard)
}

/// End whichever drag is live (window-level mouse-up).
pub fn end_any_drag() -> Option<String> {
    let mut guard = state().lock().unwrap();
    let sel = guard.as_mut()?;
    if !sel.dragging {
        return None;
    }
    finish_drag(&mut guard)
}

fn finish_drag(guard: &mut Option<MdSelection>) -> Option<String> {
    let Some(sel) = guard.as_mut() else {
        return None;
    };
    sel.dragging = false;
    if sel.spans.iter().all(|s| s.range.is_empty()) {
        *guard = None;
        return None;
    }
    Some(join_spans(&sel.spans))
}

/// Clear if `key` owns a settled selection (a mouse-down landed outside the
/// owner; the element the down landed IN claims right after). True if cleared.
pub fn clear_if_owner(key: &str) -> bool {
    let mut guard = state().lock().unwrap();
    if guard
        .as_ref()
        .is_some_and(|s| s.anchor_key == key && !s.dragging)
    {
        *guard = None;
        return true;
    }
    false
}

/// The wash range for `key` this frame (empty ⇒ nothing to paint).
pub fn wash_range(key: &str) -> Option<Range<usize>> {
    let guard = state().lock().unwrap();
    let sel = guard.as_ref()?;
    sel.spans
        .iter()
        .find(|s| s.key == key && !s.range.is_empty())
        .map(|s| s.range.clone())
}

/// The full selected text (Cmd+C), spans joined in document order.
pub fn selected_text() -> Option<String> {
    let guard = state().lock().unwrap();
    let sel = guard.as_ref()?;
    if sel.spans.iter().all(|s| s.range.is_empty()) {
        return None;
    }
    Some(join_spans(&sel.spans))
}

fn join_spans(spans: &[Span]) -> String {
    spans
        .iter()
        .filter(|s| !s.range.is_empty())
        .map(|s| &s.text[s.range.clone()])
        .collect::<Vec<_>>()
        .join("\n")
}

/// Word range around `ix` for double-click selection: an alphanumeric/`_`
/// run, or the single non-space char under the cursor, or empty at spaces.
pub fn word_range(text: &str, ix: usize) -> Range<usize> {
    let mut ix = ix.min(text.len());
    // Snap into a char boundary (mouse indices should already be on one;
    // defensive against mid-char byte offsets).
    while ix > 0 && !text.is_char_boundary(ix) {
        ix -= 1;
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let before = text[..ix].chars().next_back();
    let at = text[ix..].chars().next();
    // Off a word boundary entirely: select the single char (or nothing).
    if !at.is_some_and(is_word) && !before.is_some_and(is_word) {
        return match at {
            Some(c) if !c.is_whitespace() => ix..ix + c.len_utf8(),
            _ => ix..ix,
        };
    }
    let start = text[..ix]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map(|(i, _)| i)
        .unwrap_or(ix);
    let end = text[ix..]
        .char_indices()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map(|(i, c)| ix + i + c.len_utf8())
        .unwrap_or(ix);
    start..end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elems<'a>() -> Vec<(&'a str, &'a str)> {
        vec![
            ("p1", "first paragraph"),
            ("p2", "second"),
            ("p3", "third one"),
        ]
    }

    #[test]
    fn spans_within_one_element() {
        let spans = resolve_spans(&elems(), (0, 6), (0, 15));
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].key, "p1");
        assert_eq!(&spans[0].text[spans[0].range.clone()], "paragraph");
        // Reversed direction normalizes.
        assert_eq!(resolve_spans(&elems(), (0, 15), (0, 6)), spans);
    }

    #[test]
    fn spans_across_elements_cover_middles_whole() {
        let spans = resolve_spans(&elems(), (0, 6), (2, 5));
        assert_eq!(spans.len(), 3);
        assert_eq!(&spans[0].text[spans[0].range.clone()], "paragraph");
        assert_eq!(&spans[1].text[spans[1].range.clone()], "second");
        assert_eq!(&spans[2].text[spans[2].range.clone()], "third");
        // Reversed drag (bottom-up) resolves identically.
        assert_eq!(resolve_spans(&elems(), (2, 5), (0, 6)), spans);
    }

    #[test]
    fn word_drag_keeps_the_clicked_word_when_moving_left() {
        let elems = vec![("p1", "hello world today")];
        // Double-clicked "world" (6..11), dragged into "hello" (0..5).
        let spans = resolve_span_ranges(&elems, (0, 6..11), (0, 0..5));
        assert_eq!(spans.len(), 1);
        assert_eq!(&spans[0].text[spans[0].range.clone()], "hello world");
    }

    #[test]
    fn word_drag_extends_right_by_whole_words() {
        let elems = vec![("p1", "hello world today")];
        let spans = resolve_span_ranges(&elems, (0, 6..11), (0, 12..17));
        assert_eq!(&spans[0].text[spans[0].range.clone()], "world today");
    }

    #[test]
    fn word_drag_across_elements_unions_the_units() {
        let spans = resolve_span_ranges(&elems(), (0, 6..15), (2, 0..5));
        assert_eq!(spans.len(), 3);
        assert_eq!(&spans[0].text[spans[0].range.clone()], "paragraph");
        assert_eq!(&spans[1].text[spans[1].range.clone()], "second");
        assert_eq!(&spans[2].text[spans[2].range.clone()], "third");
    }

    #[test]
    fn document_resolve_covers_unpainted_middles() {
        let _state = state_lock();
        bind_document(vec![
            ("p1".into(), "first paragraph".into()),
            ("p2".into(), "second".into()),
            ("p3".into(), "third one".into()),
            ("p4".into(), "fourth".into()),
        ]);
        begin("p1", 6);
        // Only p1 and p4 are "on screen" — p2/p3 were virtualized through.
        let visible = [("p1", "first paragraph"), ("p4", "fourth")];
        assert!(resolve_against_document(&visible, "p4", 6..6));
        assert_eq!(
            selected_text().as_deref(),
            Some("paragraph\nsecond\nthird one\nfourth")
        );
        end_any_drag();
        bind_document(Vec::new());
    }

    #[test]
    fn reversing_a_drag_keeps_the_original_anchor() {
        let _state = state_lock();
        bind_document(vec![
            ("p1".into(), "first paragraph".into()),
            ("p2".into(), "second".into()),
            ("p3".into(), "third one".into()),
        ]);
        begin("p3", 9);
        let visible = [
            ("p1", "first paragraph"),
            ("p2", "second"),
            ("p3", "third one"),
        ];
        assert!(resolve_against_document(&visible, "p1", 0..0));
        assert_eq!(
            selected_text().as_deref(),
            Some("first paragraph\nsecond\nthird one")
        );
        // Drag back toward the anchor: must shrink from the top, not flip.
        assert!(resolve_against_document(&visible, "p2", 0..0));
        assert_eq!(selected_text().as_deref(), Some("second\nthird one"));
        end_any_drag();
        bind_document(Vec::new());
    }

    /// The drag tests below mutate the process-global selection state —
    /// serialize them, or the parallel test runner interleaves their
    /// begin/end_drag calls (long-standing flake).
    fn state_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn drag_lifecycle_and_copy_joins() {
        let _state = state_lock();
        begin("p1", 6);
        assert_eq!(drag_anchor("p1"), Some(6));
        assert_eq!(drag_anchor("p2"), None);
        let spans = resolve_spans(&elems(), (0, 6), (1, 6));
        assert!(update_spans(spans.clone()));
        assert!(!update_spans(spans)); // unchanged ⇒ no repaint
        assert_eq!(wash_range("p1"), Some(6..15));
        assert_eq!(wash_range("p2"), Some(0..6));
        assert_eq!(wash_range("p3"), None);
        assert_eq!(end_drag("p1").as_deref(), Some("paragraph\nsecond"));
        assert_eq!(selected_text().as_deref(), Some("paragraph\nsecond"));
        // Settled: a down elsewhere clears via the owner's listener.
        assert!(!clear_if_owner("p2"));
        assert!(clear_if_owner("p1"));
        assert_eq!(selected_text(), None);
    }

    #[test]
    fn empty_click_clears_on_release() {
        let _state = state_lock();
        begin("p1", 3);
        assert_eq!(end_drag("p1"), None);
        assert_eq!(selected_text(), None);
    }

    #[test]
    fn double_click_span() {
        let _state = state_lock();
        begin_with_span("p1", "hello world", 6..11);
        assert_eq!(wash_range("p1"), Some(6..11));
        assert_eq!(end_drag("p1").as_deref(), Some("world"));
    }

    #[test]
    fn autoscroll_delta_is_zero_inside_the_band() {
        assert_eq!(autoscroll_delta(200.0, 100.0, 500.0), 0.0);
        assert_eq!(autoscroll_delta(172.0, 100.0, 500.0), 0.0);
        assert_eq!(autoscroll_delta(428.0, 100.0, 500.0), 0.0);
    }

    #[test]
    fn autoscroll_delta_ramps_at_the_edges() {
        // Mid-zone above the top: half speed upward.
        let up = autoscroll_delta(136.0, 100.0, 500.0);
        assert!((up - (-9.0)).abs() < 0.01, "{up}");
        // Mid-zone below the bottom: half speed downward.
        let down = autoscroll_delta(464.0, 100.0, 500.0);
        assert!((down - 9.0).abs() < 0.01, "{down}");
        // Past the band edge: still capped at 2×.
        assert!((autoscroll_delta(0.0, 100.0, 500.0) - (-36.0)).abs() < 0.01);
        assert!((autoscroll_delta(600.0, 100.0, 500.0) - 36.0).abs() < 0.01);
    }

    #[test]
    fn autoscroll_delta_rejects_a_collapsed_band() {
        assert_eq!(autoscroll_delta(10.0, 100.0, 100.0), 0.0);
    }

    #[test]
    fn word_ranges() {
        let t = "let foo_bar = 12;";
        assert_eq!(word_range(t, 5), 4..11); // inside foo_bar
        assert_eq!(word_range(t, 4), 4..11); // at word start
        assert_eq!(word_range(t, 11), 4..11); // at word end
        assert_eq!(word_range(t, 15), 14..16); // inside 12
        assert_eq!(&t[word_range(t, 12)], "="); // lone symbol
        assert_eq!(word_range(t, 3), 0..3); // boundary after "let"
        // Unicode-safe (mid-char byte offsets snap down).
        let u = "héllo wörld";
        assert_eq!(&u[word_range(u, 2)], "héllo");
    }
}
