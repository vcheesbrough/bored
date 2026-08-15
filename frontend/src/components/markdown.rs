use leptos::prelude::*;
use pulldown_cmark::{html, CowStr, Event, Options, Parser, Tag, TagEnd};

use crate::search::highlight_spans;

fn to_html(md: &str) -> String {
    to_html_with(md, "")
}

/// Renders `md` to HTML, wrapping every span of body text that `query` matched
/// in `<mark class="search-hit">`.
///
/// An empty `query` renders exactly what [`to_html`] renders.
fn to_html_with(md: &str, query: &str) -> String {
    // Strip raw HTML and dangerous URI schemes to prevent stored XSS via `inner_html`.
    // `skip_depth` counts how many nested dangerous link/image Starts are open; we
    // suppress each End only while the counter is non-zero. A counter (not a bool) is
    // needed because images can nest inside links — e.g. [![alt](js:src)](js:href)
    // produces two Start events before the first End.
    let mut skip_depth: u32 = 0;
    let parser = Parser::new_ext(md, Options::ENABLE_TABLES).filter(|event| match event {
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
        Event::End(TagEnd::Link | TagEnd::Image) => {
            if skip_depth > 0 {
                skip_depth -= 1;
                false
            } else {
                true
            }
        }
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
    let parser = parser.map(|event| match event {
        Event::Start(Tag::CodeBlock(_)) => {
            code_depth += 1;
            event
        }
        Event::End(TagEnd::CodeBlock) => {
            code_depth = code_depth.saturating_sub(1);
            event
        }
        Event::Text(text) if highlight && code_depth == 0 => {
            let spans = highlight_spans(&text, query);
            if spans.is_empty() {
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
    use super::{to_html, to_html_with};

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
        let out = to_html_with("Deploy the preview", "deploy");
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
            assert_eq!(to_html_with(md, ""), to_html(md), "for: {md}");
            assert_eq!(to_html_with(md, "   "), to_html(md), "for: {md}");
        }
    }

    #[test]
    fn highlighted_text_is_still_escaped() {
        // Highlighting bypasses the parser's escaper, so the escaping in
        // `mark_spans` is what prevents stored XSS here.
        let out = to_html_with("alert <script>evil()</script> deploy", "deploy alert");
        assert!(!out.contains("<script>"), "raw script tag leaked: {out}");
        assert!(out.contains(r#"<mark class="search-hit">deploy</mark>"#));

        // `<` that the parser keeps as text must come out escaped in the
        // highlighted output, exactly as it does without highlighting.
        let angles = to_html_with("deploy 1 < 2 > 0", "deploy");
        assert!(angles.contains("&lt;"), "expected escaped '<': {angles}");
        assert!(angles.contains("&gt;"), "expected escaped '>': {angles}");
    }

    #[test]
    fn highlight_escapes_ampersands_and_quotes() {
        let out = to_html_with(r#"deploy "A&B" now"#, "deploy");
        assert!(out.contains("&amp;"), "ampersand must be escaped: {out}");
        assert!(!out.contains(r#""A&B""#), "raw entity text leaked: {out}");
    }

    #[test]
    fn highlight_keeps_markdown_structure() {
        let out = to_html_with("# Deploy\n\n- deploy item", "deploy");
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
        let out = to_html_with("```\nlet deploy = 1;\n```", "deploy");
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
        let out = to_html_with("[deploy me](javascript:alert(1))", "deploy");
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
) -> impl IntoView {
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
                to_html_with(&body, &query)
            }
        ></div>
    }
}

/// Renders markdown that does not change for the lifetime of the component.
#[component]
pub fn StaticMarkdownPreview(body: String, #[prop(optional)] class: &'static str) -> impl IntoView {
    let rendered = to_html(&body);
    view! {
        <div class=class inner_html=rendered></div>
    }
}
