use leptos::prelude::*;
use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd, html};

use crate::search::highlight_spans;

fn to_html(md: &str) -> String {
    to_html_with(md, "", false)
}

/// The `data-*` attribute that carries a text run's offset in the markdown
/// source. Shared with `crate::caret`, which reads it back off the DOM.
pub const SRC_POS_ATTR: &str = "data-src";

/// Renders `md` to HTML, wrapping every span of body text that `query` matched
/// in `<mark class="search-hit">`.
///
/// An empty `query` renders without highlighting.
///
/// `source_positions` wraps every run of body text in
/// `<span data-src="N">…</span>`, where `N` is the offset of that run's first
/// character in `md`, counted in **UTF-16 code units**. Click-to-edit reads
/// those offsets back off the DOM to place the textarea caret
/// (`crate::caret`); UTF-16 is the unit `HTMLTextAreaElement.selectionStart`
/// works in, so the conversion happens here rather than in the browser.
///
/// The spans are inline and carry no styling, so the rendered output looks
/// identical either way — but they are one extra DOM node per text run, which
/// is why callers that are never edited (collapsed card previews, the history
/// panel) leave this off.
fn to_html_with(md: &str, query: &str, source_positions: bool) -> String {
    // Strip raw HTML and dangerous URI schemes to prevent stored XSS via `inner_html`.
    // `skip_depth` counts how many nested dangerous link/image Starts are open; we
    // suppress each End only while the counter is non-zero. A counter (not a bool) is
    // needed because images can nest inside links — e.g. [![alt](js:src)](js:href)
    // produces two Start events before the first End.
    let mut skip_depth: u32 = 0;
    // `into_offset_iter` yields `(Event, Range<usize>)` — the *byte* range of
    // `md` that produced each event. The range rides along through the filter
    // and map below untouched and is dropped just before `push_html`.
    let parser = Parser::new_ext(md, Options::ENABLE_TABLES)
        .into_offset_iter()
        .filter(|(event, _)| match event {
            // Raw HTML blocks and inline HTML inject verbatim into the DOM.
            Event::Html(_) | Event::InlineHtml(_) => false,
            // Block dangerous URI schemes in link/image destinations.
            Event::Start(pulldown_cmark::Tag::Link { dest_url, .. })
            | Event::Start(pulldown_cmark::Tag::Image { dest_url, .. }) => {
                let lower = dest_url.to_lowercase();
                let allowed = !lower.starts_with("javascript:")
                    && !lower.starts_with("vbscript:")
                    && !lower.starts_with("data:");
                if !allowed {
                    skip_depth += 1;
                }
                allowed
            }
            Event::End(TagEnd::Link | TagEnd::Image) if skip_depth > 0 => {
                skip_depth -= 1;
                false
            }
            Event::End(TagEnd::Link | TagEnd::Image) => true,
            _ => true,
        });
    // Highlighting runs *after* the sanitising filter above, so injected markup
    // can never come from the card body — only from this function.  `InlineHtml`
    // is written to the output verbatim, which is why the text is escaped here
    // rather than left to `push_html`.
    let highlight = !query.trim().is_empty();
    // Depth of open code blocks: their text is kept verbatim so highlighting
    // never rewrites code the user typed.
    let mut code_depth: u32 = 0;
    // Converts the byte offsets the parser reports into the UTF-16 offsets the
    // DOM wants. Kept outside the closure so one cursor walks `md` once across
    // every run, rather than rescanning the prefix per run.
    let mut utf16 = Utf16Cursor::new(md);
    let parser = parser.map(|(event, range)| match event {
        Event::Start(Tag::CodeBlock(_)) => {
            code_depth += 1;
            event
        }
        Event::End(TagEnd::CodeBlock) => {
            code_depth = code_depth.saturating_sub(1);
            event
        }
        // Inline code is a single event whose range spans the backticks too, so
        // the run's own start has to be located inside that slice. `push_html`
        // escapes the payload of `Event::Code`, so the annotated form has to be
        // emitted as `InlineHtml` — `<code>` wrapper and all.
        Event::Code(text) if source_positions => {
            let start = locate_run(md, &range, &text);
            let mut out = String::with_capacity(text.len() + 48);
            out.push_str("<code>");
            // Inline code is never highlighted, for the same reason a code
            // block is not: it is text the user typed verbatim.
            out.push_str(&wrap_run(&text, utf16.offset_of(start), &[]));
            out.push_str("</code>");
            Event::InlineHtml(CowStr::from(out))
        }
        Event::Text(text) => {
            let spans = if highlight && code_depth == 0 {
                highlight_spans(&text, query)
            } else {
                Vec::new()
            };
            if source_positions {
                let start = locate_run(md, &range, &text);
                Event::InlineHtml(CowStr::from(wrap_run(
                    &text,
                    utf16.offset_of(start),
                    &spans,
                )))
            } else if spans.is_empty() {
                Event::Text(text)
            } else {
                Event::InlineHtml(CowStr::from(mark_spans(&text, &spans)))
            }
        }
        _ => event,
    });

    let mut html_output = String::new();
    html::push_html(&mut html_output, parser);
    html_output
}

/// Byte offset in `md` where the rendered run `text` starts.
///
/// The event's range brackets the syntax that produced the run, which for plain
/// text is the run itself but for inline code also covers the backticks (and
/// any padding space) — ``` `code` ``` has a range three bytes wider than its
/// text. Searching the slice finds the run inside it; a run that has been
/// rewritten by the parser (an entity, an escape, smart punctuation) will not
/// be found verbatim, and the range start is the right answer for those anyway.
fn locate_run(md: &str, range: &std::ops::Range<usize>, text: &str) -> usize {
    md.get(range.clone())
        .and_then(|slice| slice.find(text))
        .map_or(range.start, |i| range.start + i)
}

/// Walks `md` once, converting byte offsets to UTF-16 code-unit offsets.
///
/// Callers must ask for offsets in non-decreasing order — which the parser's
/// text runs are, since they follow the source. An out-of-order request would
/// be wrong rather than merely slow, so it is rejected by falling back to a
/// full rescan.
struct Utf16Cursor<'a> {
    md: &'a str,
    byte: usize,
    utf16: usize,
}

impl<'a> Utf16Cursor<'a> {
    fn new(md: &'a str) -> Self {
        Self {
            md,
            byte: 0,
            utf16: 0,
        }
    }

    fn offset_of(&mut self, byte: usize) -> usize {
        let byte = byte.min(self.md.len());
        if byte < self.byte {
            // Out of order: recompute from the start rather than return a stale
            // count. Not expected, but cheap insurance against a parser quirk.
            self.byte = 0;
            self.utf16 = 0;
        }
        if let Some(slice) = self.md.get(self.byte..byte) {
            self.utf16 += slice.encode_utf16().count();
            self.byte = byte;
        }
        self.utf16
    }
}

/// Renders one text run as `<span data-src="start">…</span>`, with `spans`
/// (byte ranges within `text`) wrapped in `<mark>` *inside* the span.
///
/// The mark nests inside rather than around so that the caret resolver always
/// finds the offset-carrying element by walking up from the clicked text node,
/// whether or not a search is live.
fn wrap_run(text: &str, start: usize, spans: &[(usize, usize)]) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    out.push_str("<span ");
    out.push_str(SRC_POS_ATTR);
    out.push_str("=\"");
    out.push_str(&start.to_string());
    out.push_str("\">");
    if spans.is_empty() {
        push_escaped(&mut out, text);
    } else {
        out.push_str(&mark_spans(text, spans));
    }
    out.push_str("</span>");
    out
}

/// Escapes `text` for HTML and wraps each byte range in `spans` in a `<mark>`.
///
/// `spans` must be sorted, non-overlapping, and land on character boundaries —
/// which is what [`highlight_spans`] guarantees.
fn mark_spans(text: &str, spans: &[(usize, usize)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for &(start, end) in spans {
        push_escaped(&mut out, &text[cursor..start]);
        out.push_str("<mark class=\"search-hit\">");
        push_escaped(&mut out, &text[start..end]);
        out.push_str("</mark>");
        cursor = end;
    }
    push_escaped(&mut out, &text[cursor..]);
    out
}

/// Escapes the characters that could break out of HTML text content.
///
/// The parser's own escaping is bypassed once text becomes `InlineHtml`, so this
/// is the only thing standing between card text and the DOM.
fn push_escaped(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SRC_POS_ATTR, to_html, to_html_with};

    /// `to_html_with` with highlighting but no source annotation — the shape
    /// every pre-existing test here was written against.
    fn highlighted(md: &str, query: &str) -> String {
        to_html_with(md, query, false)
    }

    /// `to_html_with` with source annotation and no highlighting.
    fn annotated(md: &str) -> String {
        to_html_with(md, "", true)
    }

    /// The `data-src` value of the span whose text is exactly `text`.
    ///
    /// Panics if there is no such span, which is what makes the assertions
    /// below read as "this run is annotated, and here is its offset".
    fn src_of(html: &str, text: &str) -> usize {
        let needle = format!(">{text}</span>");
        let end = html
            .find(&needle)
            .unwrap_or_else(|| panic!("no span with text {text:?} in: {html}"));
        let open = html[..end]
            .rfind("<span ")
            .unwrap_or_else(|| panic!("span text {text:?} has no opening tag in: {html}"));
        let attr = format!("{SRC_POS_ATTR}=\"");
        let value_start = html[open..end]
            .find(&attr)
            .map(|i| open + i + attr.len())
            .unwrap_or_else(|| panic!("span for {text:?} carries no {SRC_POS_ATTR}: {html}"));
        let value_end = value_start
            + html[value_start..]
                .find('"')
                .expect("unterminated attribute");
        html[value_start..value_end]
            .parse()
            .expect("offset is a number")
    }

    #[test]
    fn dangerous_link_is_stripped_entirely() {
        // The <a> start and its matching end must both be suppressed — no
        // stray </a> that would capture following text.
        let out = to_html("[click me](javascript:alert(1))");
        assert!(!out.contains("<a"), "should produce no opening <a>");
        assert!(!out.contains("</a>"), "should produce no stray </a>");
        // The link text itself is still rendered as plain text.
        assert!(out.contains("click me"));
    }

    #[test]
    fn safe_link_round_trips() {
        let out = to_html("[Google](https://www.google.com) after");
        assert!(out.contains(r#"href="https://www.google.com""#));
        assert!(out.contains("</a>"), "closing tag must be present");
        // Text following the link must not be inside the <a>.
        let a_close = out.find("</a>").unwrap();
        let after_pos = out.find("after").unwrap();
        assert!(after_pos > a_close, "\"after\" must appear after </a>");
    }

    #[test]
    fn dangerous_image_nested_inside_dangerous_link_no_stray_close() {
        // [![alt](javascript:src)](javascript:href) — two filtered Starts before any End.
        // The bool-based implementation would reset on the image End and leak a </a>.
        let out = to_html("[![alt](javascript:img)](javascript:href)");
        assert!(!out.contains("<a"), "no opening <a>");
        assert!(
            !out.contains("</a>"),
            "no stray </a> from the outer link End"
        );
    }

    #[test]
    fn consecutive_links_one_filtered_one_safe() {
        // The filtered link's End must not suppress the safe link's End.
        let out = to_html("[bad](javascript:evil()) [good](https://example.com)");
        assert!(
            !out.contains("javascript:"),
            "dangerous scheme must not appear in output"
        );
        assert!(
            out.contains(r#"href="https://example.com""#),
            "safe link must survive"
        );
        assert_eq!(
            out.matches("</a>").count(),
            1,
            "exactly one </a> for the one safe link"
        );
    }

    #[test]
    fn markdown_tables_render_as_tables() {
        let out = to_html("| Name | State |\n| --- | --- |\n| Search | Done |");

        assert!(out.contains("<table>"));
        assert!(out.contains("<thead>"));
        assert!(out.contains("<tbody>"));
        assert!(out.contains("<th>Name</th>"));
        assert!(out.contains("<td>Search</td>"));
    }
    #[test]
    fn highlight_wraps_matches_in_mark() {
        let out = highlighted("Deploy the preview", "deploy");
        assert!(
            out.contains(r#"<mark class="search-hit">Deploy</mark>"#),
            "expected a mark around the match, got: {out}"
        );
        assert!(out.contains("the preview"), "surrounding text must survive");
    }

    #[test]
    fn empty_query_renders_identically_to_plain() {
        for md in [
            "# Heading\n\nSome *emphasised* text with a [link](https://example.com).",
            "| A | B |\n| --- | --- |\n| 1 | 2 |",
            "Text with <script> and & entities",
        ] {
            assert_eq!(highlighted(md, ""), to_html(md), "for: {md}");
            assert_eq!(highlighted(md, "   "), to_html(md), "for: {md}");
        }
    }

    #[test]
    fn highlighted_text_is_still_escaped() {
        // Highlighting bypasses the parser's escaper, so the escaping in
        // `mark_spans` is what prevents stored XSS here.
        let out = highlighted("alert <script>evil()</script> deploy", "deploy alert");
        assert!(!out.contains("<script>"), "raw script tag leaked: {out}");
        assert!(out.contains(r#"<mark class="search-hit">deploy</mark>"#));

        // `<` that the parser keeps as text must come out escaped in the
        // highlighted output, exactly as it does without highlighting.
        let angles = highlighted("deploy 1 < 2 > 0", "deploy");
        assert!(angles.contains("&lt;"), "expected escaped '<': {angles}");
        assert!(angles.contains("&gt;"), "expected escaped '>': {angles}");
    }

    #[test]
    fn highlight_escapes_ampersands_and_quotes() {
        let out = highlighted(r#"deploy "A&B" now"#, "deploy");
        assert!(out.contains("&amp;"), "ampersand must be escaped: {out}");
        assert!(!out.contains(r#""A&B""#), "raw entity text leaked: {out}");
    }

    #[test]
    fn highlight_keeps_markdown_structure() {
        let out = highlighted("# Deploy\n\n- deploy item", "deploy");
        assert!(out.contains("<h1>"), "heading must survive: {out}");
        assert!(out.contains("<li>"), "list item must survive: {out}");
        assert_eq!(
            out.matches("<mark").count(),
            2,
            "both matches marked: {out}"
        );
    }

    #[test]
    fn highlight_leaves_code_blocks_verbatim() {
        let out = highlighted("```\nlet deploy = 1;\n```", "deploy");
        assert!(
            !out.contains("<mark"),
            "code block must not be marked: {out}"
        );
        assert!(out.contains("let deploy = 1;"));
    }

    #[test]
    fn highlight_does_not_reintroduce_dangerous_links() {
        // The sanitising filter runs before highlighting, so a filtered link
        // stays filtered even when its text matches the query.
        let out = highlighted("[deploy me](javascript:alert(1))", "deploy");
        assert!(
            !out.contains("<a"),
            "dangerous link must stay stripped: {out}"
        );
        assert!(!out.contains("javascript:"));
        assert!(
            out.contains("<mark"),
            "link text is still highlighted: {out}"
        );
    }

    // ── Source-position annotation (click-to-edit caret placement) ──────────

    #[test]
    fn source_positions_are_off_by_default() {
        let md = "# Heading\n\nSome text.";
        assert!(
            !to_html(md).contains(SRC_POS_ATTR),
            "plain render must carry no offsets: {}",
            to_html(md)
        );
        assert!(
            !highlighted(md, "text").contains(SRC_POS_ATTR),
            "highlighting alone must not turn offsets on"
        );
        assert!(annotated(md).contains(SRC_POS_ATTR), "opt-in must work");
    }

    #[test]
    fn offsets_point_at_the_run_in_the_source() {
        let md = "# Heading\n\nSome text.";
        let out = annotated(md);
        // Each offset must be exactly where that run starts in `md`.
        assert_eq!(src_of(&out, "Heading"), md.find("Heading").unwrap());
        assert_eq!(src_of(&out, "Some text."), md.find("Some text.").unwrap());
    }

    #[test]
    fn offsets_skip_inline_markup() {
        // The run after `**bold**` starts past the closing asterisks, not at
        // the paragraph start — this is the whole point of the offset iterator.
        let md = "a **bold** tail";
        let out = annotated(md);
        assert_eq!(src_of(&out, "a "), 0);
        assert_eq!(src_of(&out, "bold"), md.find("bold").unwrap());
        assert_eq!(src_of(&out, " tail"), md.find(" tail").unwrap());
    }

    #[test]
    fn offsets_are_utf16_units_not_bytes() {
        // "𝄞" is 4 bytes but 2 UTF-16 units, and "é" is 2 bytes but 1 unit.
        // A byte offset here would land the caret past the intended character.
        let md = "𝄞é then";
        let out = annotated(md);
        // One text run for the whole paragraph, starting at 0 either way, so
        // check the conversion on a run that follows the multi-byte characters.
        let md2 = "𝄞é *x* then";
        let out2 = annotated(md2);
        assert_eq!(src_of(&out, "𝄞é then"), 0);
        // "𝄞"=2 + "é"=1 + " "=1 + "*"=1 → 5 UTF-16 units before "x".
        assert_eq!(src_of(&out2, "x"), 5, "in: {out2}");
        // Byte offset would have been 4+2+1+1 = 8, so this genuinely
        // distinguishes the two units.
        assert_ne!(src_of(&out2, "x"), md2.find('x').unwrap());
    }

    #[test]
    fn mark_nests_inside_the_offset_span() {
        // The caret resolver walks *up* from the clicked text node to the
        // nearest `data-src`, so the mark must not be the outer element.
        let out = to_html_with("Deploy the preview", "deploy", true);
        let span = out.find("<span ").expect("span present");
        let mark = out.find("<mark").expect("mark present");
        assert!(span < mark, "span must open before the mark: {out}");
        assert!(
            out.contains(r#"<mark class="search-hit">Deploy</mark>"#),
            "match still marked: {out}"
        );
    }

    #[test]
    fn code_block_text_is_annotated() {
        let md = "before\n\n```\nlet deploy = 1;\n```";
        let out = annotated(md);
        assert_eq!(
            src_of(&out, "let deploy = 1;\n"),
            md.find("let deploy").unwrap(),
            "code block text must be clickable: {out}"
        );
        assert!(out.contains("<pre><code>"), "still a code block: {out}");
    }

    #[test]
    fn inline_code_offset_skips_the_backticks() {
        let md = "run `cargo test` now";
        let out = annotated(md);
        assert!(out.contains("<code>"), "still rendered as code: {out}");
        assert_eq!(
            src_of(&out, "cargo test"),
            md.find("cargo test").unwrap(),
            "offset must land inside the backticks: {out}"
        );
    }

    #[test]
    fn annotation_escapes_text_and_keeps_links_sanitised() {
        // The annotated path writes `InlineHtml`, bypassing the parser's
        // escaper exactly as the highlighting path does — so the same
        // guarantees have to hold here.
        let out = annotated("a < b & c \"d\" <script>evil()</script>");
        assert!(!out.contains("<script>"), "raw script leaked: {out}");
        assert!(out.contains("&lt;"), "'<' escaped: {out}");
        assert!(out.contains("&amp;"), "'&' escaped: {out}");

        let link = annotated("[deploy me](javascript:alert(1))");
        assert!(
            !link.contains("<a"),
            "dangerous link still stripped: {link}"
        );
        assert!(!link.contains("javascript:"));
    }

    #[test]
    fn annotation_keeps_markdown_structure() {
        let out = annotated("# H\n\n- item\n\n| A | B |\n| --- | --- |\n| 1 | 2 |");
        for tag in ["<h1>", "<li>", "<table>", "<th>", "<td>"] {
            assert!(out.contains(tag), "{tag} must survive: {out}");
        }
    }

    #[test]
    fn offsets_are_non_decreasing_through_a_realistic_body() {
        // The UTF-16 cursor is monotonic; if the parser ever handed runs back
        // out of order the offsets would silently drift, so pin the ordering.
        let md = "# Title\n\nPara with `code`, **bold**, a [link](https://e.com), and é.\n\n- one\n- two\n\n```\nfenced 𝄞\n```\n\nTail.";
        let out = annotated(md);
        let attr = format!("{SRC_POS_ATTR}=\"");
        let mut last = 0usize;
        let mut count = 0usize;
        let mut rest = out.as_str();
        while let Some(i) = rest.find(&attr) {
            rest = &rest[i + attr.len()..];
            let end = rest.find('"').expect("unterminated attribute");
            let value: usize = rest[..end].parse().expect("offset is a number");
            assert!(
                value >= last,
                "offset {value} went backwards after {last}: {out}"
            );
            assert!(
                value <= md.encode_utf16().count(),
                "offset {value} past end of source"
            );
            last = value;
            count += 1;
        }
        assert!(count >= 8, "expected several annotated runs, got {count}");
    }
}

/// Renders a markdown string as HTML inside a `<div>`.
///
/// `inner_html` is Leptos 0.7's built-in special attribute that calls
/// `set_inner_html()` on the underlying DOM element reactively.
#[component]
pub fn MarkdownPreview(
    #[prop(into)] body: Signal<String>,
    #[prop(optional)] class: &'static str,
    /// Live search query; matches in the rendered text are wrapped in `<mark>`.
    /// Omit it (or feed it an empty string) to render without highlighting.
    #[prop(optional, into)]
    highlight: Option<Signal<String>>,
    /// Annotate each text run with its markdown-source offset so that a click
    /// on the rendered body can be turned into a caret position in the
    /// textarea. Only worth paying for on views the user can click into edit.
    #[prop(optional)]
    source_positions: bool,
) -> AnyView {
    // caps monomorphization at this boundary — see CardVersionActions doc comment in history_panel.rs
    view! {
        <div
            class=class
            inner_html=move || {
                // Reading the signals inside the closure keeps the render
                // reactive: typing in the search box re-renders the body.
                // `try_get` keeps a re-render of an already-disposed card (one
                // the search just filtered out) from trapping — see `CardItem`.
                let query = highlight.and_then(|q| q.try_get()).unwrap_or_default();
                let body = body.try_get().unwrap_or_default();
                to_html_with(&body, &query, source_positions)
            }
        ></div>
    }
    .into_any()
}

/// Renders markdown that does not change for the lifetime of the component.
#[component]
pub fn StaticMarkdownPreview(body: String, #[prop(optional)] class: &'static str) -> impl IntoView {
    let rendered = to_html(&body);
    view! {
        <div class=class inner_html=rendered></div>
    }
}
