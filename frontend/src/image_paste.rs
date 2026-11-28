//! Pasting an image into a card body (card #67).
//!
//! When the clipboard holds an image *file*, a paste into either card editor
//! (the full-screen modal's textarea or the inline card's) does not insert
//! anything the browser would: it uploads the image to `POST /api/images` and
//! writes `![image](/api/images/{id})` into the body. Everything else, plain
//! text included, pastes exactly as before. That is also how image markdown
//! moves between cards: cut and paste of the text.
//!
//! # The placeholder
//!
//! An upload takes time, and the user keeps typing. So the paste first
//! inserts a placeholder at the caret, `![Uploading image…](#upload-<token>)`,
//! and the upload's answer later swaps *that exact text* for the real
//! markdown, wherever it has moved to by then. On failure the placeholder is
//! removed. Each paste gets its own token, so two uploads in flight each find
//! their own placeholder.
//!
//! The swap edits the body *signal*, not the textarea, so it still lands when
//! the editor has been closed in the meantime. If the editor's component is
//! gone altogether (the card collapsed off the board, the modal closed), the
//! swap is made against the server's copy of the card instead
//! ([`finish_upload`]).
//!
//! The text functions at the top are pure and host-tested. Caret positions
//! are **UTF-16 code units**, because that is what a textarea's
//! `selectionStart` counts. A Rust string's byte offsets differ as soon as the
//! text has an accented letter or an emoji.

use leptos::prelude::*;
use wasm_bindgen::JsCast as _;

/// The text inserted while an upload with this `token` is in flight.
pub fn placeholder(token: &str) -> String {
    format!("![Uploading image…](#upload-{token})")
}

/// The markdown an uploaded image is written as.
pub fn image_markdown(url: &str) -> String {
    format!("![image]({url})")
}

/// `body` with `text` in place of the UTF-16 range `start..end` (the
/// textarea's selection; `start == end` is a plain caret), and the caret
/// position just after the inserted text, also in UTF-16 units.
///
/// Out-of-range or reversed offsets are clamped rather than trusted: they come
/// from the DOM, and a paste must never panic.
pub fn insert_at(body: &str, start: u32, end: u32, text: &str) -> (String, u32) {
    let start_byte = utf16_to_byte(body, start);
    // `max` keeps `end` from landing before `start`.
    let end_byte = utf16_to_byte(body, end).max(start_byte);
    let mut out = String::with_capacity(body.len() + text.len());
    out.push_str(&body[..start_byte]);
    out.push_str(text);
    out.push_str(&body[end_byte..]);
    // The caret goes after the insertion: everything before it, plus the
    // inserted text, counted in UTF-16 units.
    let caret = utf16_len(&body[..start_byte]) + utf16_len(text);
    (out, caret)
}

/// `body` with the placeholder for `token` replaced by `replacement`, or
/// `None` when the placeholder is no longer there (the user deleted it, so
/// there is nothing to put back).
pub fn replace_placeholder(body: &str, token: &str, replacement: &str) -> Option<String> {
    let marker = placeholder(token);
    // `replacen(.., 1)`: only the first occurrence. There is only ever one for
    // a token, but this way a pasted copy of a placeholder is left alone.
    body.contains(&marker)
        .then(|| body.replacen(&marker, replacement, 1))
}

/// The byte offset in `text` of UTF-16 offset `units`, clamped to the end of
/// the text and never inside a character.
fn utf16_to_byte(text: &str, units: u32) -> usize {
    let mut seen = 0u32;
    // `char_indices` yields each character with its byte offset.
    for (byte, ch) in text.char_indices() {
        if seen >= units {
            return byte;
        }
        // `len_utf16` is 1 for most characters and 2 for those outside the
        // Basic Multilingual Plane (most emoji).
        seen += ch.len_utf16() as u32;
    }
    text.len()
}

/// How many UTF-16 code units `text` is.
fn utf16_len(text: &str) -> u32 {
    text.encode_utf16().count() as u32
}

/// What a failed upload says to the user, from its HTTP status (if it got
/// one). Fixed sentences: nothing from the server's answer is shown.
pub fn failure_message(status: Option<u16>) -> &'static str {
    match status {
        Some(413) => TOO_LARGE,
        Some(415) => "That file isn't a supported image (PNG, JPEG, GIF, WebP or SVG).",
        _ => "The image could not be uploaded.",
    }
}

/// Shown when the image is over the limit: checked in the browser before
/// anything is sent, and also what a 413 from the server says.
const TOO_LARGE: &str = "That image is too large: the limit is 10 MB.";

// ── The browser side ────────────────────────────────────────────────────────

/// What a paste handler needs from the editor it serves.
///
/// Both editors keep their body in a signal and save it with their own
/// `do_save`; these closures are how this module reaches them without
/// knowing which editor it is in. `Clone + 'static` because they are moved
/// into the upload's async task, which outlives the paste event.
pub struct Editor<R, W, E> {
    /// The card being edited, for the server-side fallback.
    pub card_id: String,
    /// The body as the editor currently holds it, or `None` when the editor
    /// is gone or now shows a different card.
    pub read: R,
    /// Put a new body into the editor *and save it*. Returns `false` when the
    /// editor is gone, so the swap must go to the server instead.
    pub write: W,
    /// Show (`Some`) or clear (`None`) the upload error next to the editor.
    pub report: E,
}

/// Handle a `paste` event on `textarea`. Returns without touching anything
/// unless the clipboard holds an image file.
pub fn handle_paste<R, W, E>(
    event: &web_sys::Event,
    textarea: &web_sys::HtmlTextAreaElement,
    editor: Editor<R, W, E>,
) where
    R: Fn() -> Option<String> + Clone + 'static,
    W: Fn(String) -> bool + Clone + 'static,
    E: Fn(Option<&'static str>) + Clone + 'static,
{
    let Some(file) = clipboard_image(event) else {
        // Not an image: the browser pastes as usual.
        return;
    };
    // From here on the paste is ours, so the browser must not also insert the
    // clipboard's text form (often the image's file name).
    event.prevent_default();

    // Too large to be accepted: say so at once rather than sending 10 MB to
    // be refused. `size()` is an `f64` in the DOM API.
    if file.size() > shared::MAX_IMAGE_BYTES as f64 {
        (editor.report)(Some(TOO_LARGE));
        return;
    }
    (editor.report)(None);

    let Some(current) = (editor.read)() else {
        return;
    };
    let token = new_token();
    // `selection_start()` is `Result<Option<u32>, _>`: `Err` if the element
    // has no selection API, `None` if it has no selection. Either way, insert
    // at the end.
    let end_of_text = utf16_len(&current);
    let start = textarea
        .selection_start()
        .ok()
        .flatten()
        .unwrap_or(end_of_text);
    let end = textarea.selection_end().ok().flatten().unwrap_or(start);
    let (with_placeholder, caret) = insert_at(&current, start, end, &placeholder(&token));

    // Update the textarea first and place the caret after the placeholder.
    // When the signal then sets the same value through `prop:value`, an
    // unchanged value does not move the caret.
    textarea.set_value(&with_placeholder);
    let _ = textarea.set_selection_range(caret, caret);
    (editor.write)(with_placeholder);

    // `spawn_local` runs the upload on the browser's event loop; the paste
    // handler returns now.
    wasm_bindgen_futures::spawn_local(async move {
        let outcome = crate::api::upload_image(&file).await;
        finish_upload(editor, &token, outcome).await;
    });
}

/// Swap the placeholder for the image (or remove it after a failure), in the
/// editor if it is still there, otherwise in the server's copy of the card.
async fn finish_upload<R, W, E>(
    editor: Editor<R, W, E>,
    token: &str,
    outcome: Result<shared::UploadedImage, crate::api::ApiError>,
) where
    R: Fn() -> Option<String>,
    W: Fn(String) -> bool,
    E: Fn(Option<&'static str>),
{
    // What to do to the body, as text in → `Some(text out)`, or `None` when
    // the placeholder is gone.
    let replacement = match &outcome {
        Ok(uploaded) => image_markdown(&uploaded.url),
        Err(error) => {
            crate::telemetry::error("image upload failed", error);
            (editor.report)(Some(failure_message(crate::telemetry::Reportable::status(
                error,
            ))));
            String::new()
        }
    };

    if let Some(current) = (editor.read)() {
        match replace_placeholder(&current, token, &replacement) {
            // The placeholder is in the editor: swap it there, which saves.
            Some(updated) => {
                if (editor.write)(updated) {
                    return;
                }
            }
            // The user deleted the placeholder. Respect that.
            None => return,
        }
    }

    // The editor has gone: make the same swap on the stored card.
    let card = match crate::api::fetch_card(&editor.card_id).await {
        Ok(card) => card,
        Err(error) => {
            crate::telemetry::error("image upload: card reload failed", &error);
            return;
        }
    };
    let Some(updated) = replace_placeholder(&card.body, token, &replacement) else {
        return;
    };
    let request = shared::UpdateCardRequest {
        body: Some(updated),
        ..Default::default()
    };
    if let Err(error) = crate::api::update_card(&editor.card_id, request).await {
        crate::telemetry::error("image upload: card save failed", &error);
    }
}

/// The first image file on the paste event's clipboard, if any.
///
/// `web_sys::ClipboardEvent` sits behind web-sys's unstable-API flag, so the
/// event's `clipboardData` is read with `Reflect` instead and cast to
/// `DataTransfer`, which is stable.
fn clipboard_image(event: &web_sys::Event) -> Option<web_sys::File> {
    let data = js_sys::Reflect::get(event, &"clipboardData".into()).ok()?;
    let data: web_sys::DataTransfer = data.dyn_into().ok()?;
    let items = data.items();
    // `items` is a DOM list, indexed from 0 to `length() - 1`.
    (0..items.length())
        .filter_map(|index| items.get(index))
        .filter(|item| item.kind() == "file" && item.type_().starts_with("image/"))
        // `get_as_file` is `Result<Option<File>>`; `.ok().flatten()` turns
        // both kinds of "no file" into `None`.
        .find_map(|item| item.get_as_file().ok().flatten())
}

/// A token no other paste in this tab (or, almost surely, any other tab) has:
/// eight random hex digits.
fn new_token() -> String {
    // `Math.random()` is plenty here: the token only has to be distinct
    // among the few uploads in flight, not unpredictable.
    let random = (js_sys::Math::random() * f64::from(u32::MAX)) as u32;
    format!("{random:08x}")
}

/// The `report` closure for an editor whose upload error lives in `slot`.
///
/// `Copy` (it captures only the signal, which is `Copy`), so the editors'
/// paste handlers stay `Copy` too, as Leptos's `view!` needs. `try_set`
/// because the upload can finish after the editor's component has been
/// disposed.
pub fn reporter(
    slot: RwSignal<Option<&'static str>>,
) -> impl Fn(Option<&'static str>) + Copy + 'static {
    move |message| {
        let _ = slot.try_set(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_placeholder_is_inserted_at_the_caret() {
        let (body, caret) = insert_at("ab", 1, 1, "XY");
        assert_eq!(body, "aXYb");
        assert_eq!(caret, 3);
    }

    #[test]
    fn a_selection_is_replaced_by_the_placeholder() {
        let (body, caret) = insert_at("hello world", 6, 11, "there");
        assert_eq!(body, "hello there");
        assert_eq!(caret, 11);
    }

    #[test]
    fn offsets_count_utf16_units_not_bytes() {
        // "é" is one UTF-16 unit but two UTF-8 bytes; "😀" is two units and
        // four bytes. Offset 3 is just after the emoji.
        let (body, caret) = insert_at("é😀z", 3, 3, "!");
        assert_eq!(body, "é😀!z");
        assert_eq!(caret, 4);
        // An offset inside the emoji's surrogate pair rounds forward to the
        // next character boundary rather than splitting it.
        let (body, _) = insert_at("😀z", 1, 1, "!");
        assert_eq!(body, "😀!z");
    }

    #[test]
    fn out_of_range_offsets_are_clamped() {
        assert_eq!(insert_at("ab", 99, 99, "!"), ("ab!".to_string(), 3));
        // A reversed selection is treated as a caret at its start.
        assert_eq!(insert_at("abc", 2, 1, "!"), ("ab!c".to_string(), 3));
        assert_eq!(insert_at("", 0, 0, "!"), ("!".to_string(), 1));
    }

    #[test]
    fn the_swap_finds_its_placeholder_after_more_typing() {
        let (body, _) = insert_at("start end", 5, 5, &placeholder("aa11"));
        // The user keeps typing on both sides while the upload runs.
        let edited = format!("new first line\n{body} and more");
        let swapped =
            replace_placeholder(&edited, "aa11", &image_markdown("/api/images/x")).expect("found");
        assert_eq!(
            swapped,
            "new first line\nstart![image](/api/images/x) end and more"
        );
    }

    #[test]
    fn a_failed_upload_removes_only_its_placeholder() {
        let body = format!("a {} b", placeholder("1"));
        // A failure replaces the placeholder with nothing (`finish_upload`).
        assert_eq!(
            replace_placeholder(&body, "1", ""),
            Some("a  b".to_string())
        );
    }

    #[test]
    fn two_uploads_each_resolve_their_own_placeholder() {
        let body = format!("{} | {}", placeholder("one"), placeholder("two"));
        // The second finishes first.
        let body = replace_placeholder(&body, "two", "[2]").expect("two");
        let body = replace_placeholder(&body, "one", "[1]").expect("one");
        assert_eq!(body, "[1] | [2]");
    }

    #[test]
    fn a_deleted_placeholder_is_not_put_back() {
        assert_eq!(replace_placeholder("no marker here", "abc", "[x]"), None);
    }

    #[test]
    fn the_generated_markdown_renders_as_an_img() {
        let html = crate::components::markdown::to_html(&image_markdown("/api/images/abc"));
        assert!(
            html.contains("<img src=\"/api/images/abc\" alt=\"image\""),
            "{html}"
        );
    }

    #[test]
    fn failures_get_a_fixed_sentence_by_status() {
        assert_eq!(failure_message(Some(413)), TOO_LARGE);
        assert!(failure_message(Some(415)).contains("supported image"));
        assert_eq!(
            failure_message(Some(500)),
            "The image could not be uploaded."
        );
        assert_eq!(failure_message(None), "The image could not be uploaded.");
    }
}
