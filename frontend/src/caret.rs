//! Turning a click on rendered markdown into a caret position in the textarea.
//!
//! The rendered body and the textarea show the same card in two different
//! shapes: one is HTML produced by `pulldown-cmark`, the other is the markdown
//! source. Clicking the rendered view used to drop the user at offset 0 of the
//! source, which on a long card means hunting for the place they just clicked.
//!
//! The bridge between the two shapes is the `data-src` attribute that
//! [`crate::components::markdown`] writes onto every run of body text: the
//! offset of that run's first character in the markdown source, counted in
//! UTF-16 code units (the unit `selectionStart` speaks). Given a click point
//! this module finds the run under it, adds the offset within that run, and
//! then places and scrolls the textarea.
//!
//! ## What "exact" means here
//!
//! The offset within a run is measured in *rendered* characters and added to
//! the run's *source* offset. Those agree character for character for plain
//! text, which is nearly all card prose. Where the source spends more
//! characters than it renders — a `\*` escape, an `&amp;` entity — the caret
//! lands a character or two **early**, never late, because the source of a run
//! is never shorter than what it renders. Card #382 accepted that in exchange
//! for not carrying a full character-level source map.

use wasm_bindgen::{JsCast, JsValue};
use web_sys::{Element, HtmlElement, HtmlTextAreaElement, Node};

use crate::components::markdown::SRC_POS_ATTR;

/// How close to the click the caret has to land before we stop scrolling.
/// Below a pixel there is nothing left to correct and browsers disagree about
/// sub-pixel rounding anyway.
const SCROLL_EPSILON: f64 = 1.0;

/// The markdown-source offset (UTF-16 code units) of the character at viewport
/// point (`x`, `y`) inside `container`.
///
/// `None` when nothing under the point can be resolved, which callers should
/// read as "leave the caret where it would have gone anyway".
pub fn source_offset_at(container: &Element, x: f64, y: f64) -> Option<u32> {
    if let Some((node, offset)) = caret_node_at(x, y)
        && container.contains(Some(&node))
        && node.node_type() == Node::TEXT_NODE
        && let Some(run) = closest_run(&node)
        && let Some(start) = run_offset(&run)
    {
        let within = text_offset_within(&run, &node, offset);
        return Some(start.saturating_add(within));
    }
    // The point did not land on annotated text: the container's padding, a list
    // bullet, the gap below the last paragraph. Fall back to the nearest run.
    nearest_run_offset(container, x, y)
}

/// Places the caret at `pos` and scrolls so it sits as close as possible to
/// `anchor_y` (the viewport Y of the click that started the edit).
///
/// The textarea absorbs as much of the scroll as it can; whatever is left over
/// is taken up by the nearest scrollable ancestor. That split is what makes one
/// code path serve both editors: the modal's textarea scrolls internally
/// (`height: 100%`), while the inline card's is `field-sizing: content;
/// overflow: hidden` and never scrolls, so its column scrolls instead.
pub fn place_caret(textarea: &HtmlTextAreaElement, pos: u32, anchor_y: Option<f64>) {
    let value = textarea.value();
    let pos = pos.min(utf16_len(&value));

    let Some(anchor_y) = anchor_y else {
        focus_without_scroll(textarea);
        let _ = textarea.set_selection_range(pos, pos);
        return;
    };

    // `anchor_y` is a screen coordinate from the click, so every measurement it
    // is compared against has to be taken in the same scroll state. Both steps
    // below can scroll an ancestor on their own, so snapshot first and put the
    // scroller back after each one:
    //
    //   * `focus()` scrolls the focused element into view, and the inline
    //     card's textarea is `field-sizing: content` — as tall as the whole
    //     card — so that alone hauls the column across.
    //   * restoring `value` after the measurement moves the caret to the *end*
    //     of the text, and a focused textarea gets its caret scrolled into
    //     view, which is what used to leave the column showing the card's end
    //     rather than the click.
    let scroller = nearest_scrollable(textarea);
    let scroller_top = scroller.as_ref().map(|el| el.scroll_top());
    let box_top = textarea.get_bounding_client_rect().top();

    // Measured before focusing: an unfocused textarea has no caret for the
    // browser to chase when the value is swapped.
    let caret_top = caret_top(textarea, &value, pos);
    restore_scroll(scroller.as_ref(), scroller_top);

    focus_without_scroll(textarea);
    let _ = textarea.set_selection_range(pos, pos);
    restore_scroll(scroller.as_ref(), scroller_top);

    let Some(caret_top) = caret_top else {
        return;
    };

    // Where the caret should sit inside the textarea's own box, so that it
    // lands back under the pointer.
    let wanted_within_box = anchor_y - box_top;

    let max_scroll = f64::from(textarea.scroll_height() - textarea.client_height()).max(0.0);
    let scroll = (caret_top - wanted_within_box).clamp(0.0, max_scroll);
    textarea.set_scroll_top(scroll as i32);

    // Whatever the textarea could not absorb — all of it, when the textarea
    // grows to fit its content instead of scrolling — is left for the scroller.
    // Scrolling it down by `remainder` moves the textarea's box up by the same
    // amount, which puts the caret at `anchor_y`.
    //
    // Applied as an absolute target measured from the snapshot rather than a
    // nudge from wherever the scroller happens to be, so a stray scroll from
    // focus or layout cannot compound into it.
    let remainder = (caret_top - scroll) - wanted_within_box;
    if remainder.abs() > SCROLL_EPSILON
        && let (Some(el), Some(from)) = (scroller.as_ref(), scroller_top)
    {
        let max = f64::from(el.scroll_height() - el.client_height()).max(0.0);
        el.set_scroll_top((f64::from(from) + remainder).clamp(0.0, max) as i32);
    }
}

/// Focus without the browser scrolling the element into view — this module
/// does its own scrolling, and the default behaviour fights it.
fn focus_without_scroll(textarea: &HtmlTextAreaElement) {
    let options = web_sys::FocusOptions::new();
    options.set_prevent_scroll(true);
    let _ = textarea.focus_with_options(&options);
}

/// Puts a scroller back where the snapshot found it.
fn restore_scroll(scroller: Option<&Element>, top: Option<i32>) {
    if let (Some(el), Some(top)) = (scroller, top) {
        el.set_scroll_top(top);
    }
}

// ── Resolving the click ────────────────────────────────────────────────────

/// The DOM (node, offset) caret position at a viewport point.
///
/// `caretPositionFromPoint` is the standard spelling; `caretRangeFromPoint` is
/// the older WebKit one, still what Safari below 17.4 offers.
fn caret_node_at(x: f64, y: f64) -> Option<(Node, u32)> {
    let document = web_sys::window()?.document()?;
    // The binding for the standard spelling is `structural` with no `catch`,
    // so calling it on an engine that does not have the method throws a
    // `TypeError` that unwinds out through wasm and kills the click handler —
    // taking click-to-edit with it, which is worse than not having the
    // fallback at all. `Reflect::has` walks the prototype chain, so this is the
    // `in` operator and costs nothing.
    if js_sys::Reflect::has(&document, &JsValue::from_str("caretPositionFromPoint"))
        .unwrap_or(false)
    {
        let caret = document.caret_position_from_point(x as f32, y as f32)?;
        return Some((caret.offset_node()?, caret.offset()));
    }
    // WebKit's older spelling. It never became standard, so `web-sys` generates
    // no binding for it and it has to be reached by name; Safari below 17.4
    // offers only this one.
    let method = js_sys::Reflect::get(&document, &JsValue::from_str("caretRangeFromPoint")).ok()?;
    let range = method
        .dyn_ref::<js_sys::Function>()?
        .call2(&document, &JsValue::from_f64(x), &JsValue::from_f64(y))
        .ok()?;
    let range = range.dyn_ref::<web_sys::Range>()?;
    Some((range.start_container().ok()?, range.start_offset().ok()?))
}

/// The nearest ancestor element (or `node` itself) carrying `data-src`.
fn closest_run(node: &Node) -> Option<Element> {
    let start = match node.dyn_ref::<Element>() {
        Some(el) => el.clone(),
        None => node.parent_element()?,
    };
    start.closest(&format!("[{SRC_POS_ATTR}]")).ok().flatten()
}

/// The source offset recorded on a run element.
fn run_offset(run: &Element) -> Option<u32> {
    run.get_attribute(SRC_POS_ATTR)?.parse().ok()
}

/// Characters (UTF-16) between the start of `run` and the caret at
/// (`target`, `offset`).
///
/// A run is usually a single text node, but a live search splits it around
/// `<mark>` elements, so the text nodes before the caret's own have to be
/// counted.
fn text_offset_within(run: &Element, target: &Node, offset: u32) -> u32 {
    let mut seen = 0u32;
    if walk_text(run.unchecked_ref::<Node>(), target, &mut seen) {
        seen.saturating_add(offset)
    } else {
        // The caret node is not inside this run after all; the run's own start
        // is the safest answer.
        0
    }
}

/// Depth-first walk that adds up text-node lengths until `target` is reached.
/// Returns whether `target` was found.
fn walk_text(node: &Node, target: &Node, seen: &mut u32) -> bool {
    if node.is_same_node(Some(target)) {
        return true;
    }
    if node.node_type() == Node::TEXT_NODE {
        *seen = seen.saturating_add(utf16_len(&node.text_content().unwrap_or_default()));
        return false;
    }
    let children = node.child_nodes();
    for i in 0..children.length() {
        if let Some(child) = children.item(i)
            && walk_text(&child, target, seen)
        {
            return true;
        }
    }
    false
}

/// The offset for a click that missed every annotated run: the end of the run
/// nearest the point, or its start if the point sits above or before it.
fn nearest_run_offset(container: &Element, x: f64, y: f64) -> Option<u32> {
    let runs = container
        .query_selector_all(&format!("[{SRC_POS_ATTR}]"))
        .ok()?;
    let mut best: Option<(f64, Element)> = None;
    for i in 0..runs.length() {
        let Some(run) = runs.item(i).and_then(|n| n.dyn_into::<Element>().ok()) else {
            continue;
        };
        let rect = run.get_bounding_client_rect();
        // Vertical distance dominates: on a wrapped paragraph the run on the
        // clicked line is the right answer even when the pointer is far to its
        // right, which is exactly where clicks past the end of a line land.
        let dy = (rect.top() - y).max(y - rect.bottom()).max(0.0);
        let dx = (rect.left() - x).max(x - rect.right()).max(0.0);
        let score = dy * 1000.0 + dx;
        if best.as_ref().is_none_or(|(b, _)| score < *b) {
            best = Some((score, run));
        }
    }
    let (_, run) = best?;
    let start = run_offset(&run)?;
    let rect = run.get_bounding_client_rect();
    // Past the end of the run in reading order → its end; before it → its start.
    if y > rect.bottom() || (y >= rect.top() && x > rect.right()) {
        let len = utf16_len(&run.text_content().unwrap_or_default());
        Some(start.saturating_add(len))
    } else {
        Some(start)
    }
}

// ── Placing and scrolling ──────────────────────────────────────────────────

/// Distance in pixels from the top of the textarea's border box to the top of
/// the line the caret sits on.
///
/// A textarea exposes no caret geometry, so this lays the text out a second
/// time in a mirror `<div>` that copies the textarea's text metrics and content
/// width, and reads the offset of a zero-width marker placed at the caret.
///
/// The cheaper trick — shorten the value, read `scrollHeight`, put it back —
/// looks right and silently returns a constant here. The inline card's textarea
/// shares a CSS grid cell with the rendered markdown and is `align-self:
/// stretch`, so shrinking its value does not shrink the cell: it is pulled
/// straight back to the sibling's height. The modal's is `height: 100%`, so any
/// prefix shorter than the visible box just reads as the box height. Measured
/// on card #382, that read 3751px for prefixes of 53, 416 and 7364 characters
/// alike — against true offsets of 12, 137 and 2508 — which scrolled the column
/// to the end of the card no matter where the user clicked.
fn caret_top(textarea: &HtmlTextAreaElement, value: &str, pos: u32) -> Option<f64> {
    let window = web_sys::window()?;
    let document = window.document()?;
    let body = document.body()?;
    let style = window
        .get_computed_style(textarea.unchecked_ref::<Element>())
        .ok()
        .flatten()?;

    let prop = |name: &str| style.get_property_value(name).unwrap_or_default();
    let px = |name: &str| {
        prop(name)
            .trim_end_matches("px")
            .parse::<f64>()
            .unwrap_or(0.0)
    };

    // `client_width` is the padding box, so take the padding back off to get the
    // width the text actually wraps at.
    let content_width =
        (f64::from(textarea.client_width()) - px("padding-left") - px("padding-right")).max(0.0);

    // Transparent borders of the real widths: `offset_top` is measured from the
    // border edge, which is also what `get_bounding_client_rect().top` returns
    // for the textarea, so the two agree without any correction term.
    let css = format!(
        "position:absolute;visibility:hidden;left:-9999px;top:0;\
         box-sizing:content-box;white-space:pre-wrap;overflow-wrap:break-word;\
         width:{content_width}px;\
         font-family:{};font-size:{};font-weight:{};font-style:{};\
         letter-spacing:{};line-height:{};text-transform:{};word-spacing:{};\
         text-indent:{};tab-size:{};\
         padding:{} {} {} {};\
         border-width:{} {} {} {};border-style:solid;border-color:transparent;",
        prop("font-family"),
        prop("font-size"),
        prop("font-weight"),
        prop("font-style"),
        prop("letter-spacing"),
        prop("line-height"),
        prop("text-transform"),
        prop("word-spacing"),
        prop("text-indent"),
        prop("tab-size"),
        prop("padding-top"),
        prop("padding-right"),
        prop("padding-bottom"),
        prop("padding-left"),
        prop("border-top-width"),
        prop("border-right-width"),
        prop("border-bottom-width"),
        prop("border-left-width"),
    );

    let mirror = document.create_element("div").ok()?;
    mirror.set_attribute("style", &css).ok()?;
    // `white-space: pre-wrap` keeps a trailing newline, so a caret on a fresh
    // empty line lands on that line rather than the end of the one above.
    mirror.set_text_content(Some(utf16_prefix(value, pos)));

    let marker = document.create_element("span").ok()?;
    // Zero-width space: joins the layout without drawing anything.
    marker.set_text_content(Some("\u{200b}"));
    mirror.append_child(&marker).ok()?;
    body.append_child(&mirror).ok()?;

    let top = marker
        .dyn_ref::<HtmlElement>()
        .map(|el| f64::from(el.offset_top()));
    let _ = body.remove_child(&mirror);
    top
}

/// The nearest ancestor of `el` that actually scrolls — the column's card list
/// for an inline card, and nothing at all in the modal, whose textarea scrolls
/// on its own.
fn nearest_scrollable(el: &HtmlTextAreaElement) -> Option<Element> {
    let window = web_sys::window()?;
    let mut current = el.parent_element();
    while let Some(candidate) = current {
        let scrollable = candidate.scroll_height() > candidate.client_height()
            && window
                .get_computed_style(&candidate)
                .ok()
                .flatten()
                .and_then(|s| s.get_property_value("overflow-y").ok())
                .is_some_and(|o| o == "auto" || o == "scroll" || o == "overlay");
        if scrollable {
            return Some(candidate);
        }
        current = candidate.parent_element();
    }
    None
}

/// The viewport Y a rendered-body click should anchor the caret to, or `None`
/// for a synthetic click that carries no coordinates (`element.click()`, and
/// the keyboard activation browsers report the same way).
pub fn anchor_of(ev: &web_sys::MouseEvent) -> Option<f64> {
    if ev.client_x() == 0 && ev.client_y() == 0 {
        None
    } else {
        Some(f64::from(ev.client_y()))
    }
}

/// The element a rendered-body click landed in, as the container to search.
pub fn event_container(ev: &web_sys::MouseEvent) -> Option<Element> {
    ev.current_target()?.dyn_into::<Element>().ok()
}

/// Focus without any caret placement — the behaviour before card #382, kept for
/// the paths that have no click to work from.
pub fn focus_only(textarea: &HtmlTextAreaElement) {
    let _ = textarea.focus();
}

// ── UTF-16 helpers ─────────────────────────────────────────────────────────
//
// Rust strings index by byte, `selectionStart` and DOM text offsets by UTF-16
// code unit. Everything crossing that boundary goes through these two.

/// Length of `s` in UTF-16 code units.
fn utf16_len(s: &str) -> u32 {
    s.encode_utf16().count() as u32
}

/// The first `units` UTF-16 code units of `s`, rounded down to a character
/// boundary if `units` would split a surrogate pair.
fn utf16_prefix(s: &str, units: u32) -> &str {
    let mut seen = 0u32;
    for (byte, ch) in s.char_indices() {
        // Stop *before* a character that would take the count past `units`, so
        // a cut inside a surrogate pair yields the character boundary below it
        // rather than a byte index that would panic the slice.
        if seen + ch.len_utf16() as u32 > units {
            return &s[..byte];
        }
        seen += ch.len_utf16() as u32;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::{utf16_len, utf16_prefix};

    #[test]
    fn utf16_len_counts_code_units_not_bytes_or_chars() {
        assert_eq!(utf16_len("abc"), 3);
        // 2 bytes, 1 unit.
        assert_eq!(utf16_len("é"), 1);
        // 4 bytes, 1 char, 2 units — the case that makes byte offsets wrong.
        assert_eq!(utf16_len("𝄞"), 2);
        assert_eq!(utf16_len("a𝄞é"), 4);
    }

    #[test]
    fn utf16_prefix_cuts_at_the_requested_unit() {
        assert_eq!(utf16_prefix("abcdef", 3), "abc");
        assert_eq!(utf16_prefix("abc", 0), "");
        assert_eq!(utf16_prefix("aéb", 2), "aé");
    }

    #[test]
    fn utf16_prefix_never_splits_a_surrogate_pair() {
        // Asking for one unit of a two-unit character must not slice mid-char
        // (which would panic on a byte index) — it rounds down to before it.
        assert_eq!(utf16_prefix("a𝄞b", 2), "a");
        assert_eq!(utf16_prefix("a𝄞b", 3), "a𝄞");
    }

    #[test]
    fn utf16_prefix_clamps_past_the_end() {
        assert_eq!(utf16_prefix("abc", 99), "abc");
        assert_eq!(utf16_prefix("", 5), "");
    }
}
