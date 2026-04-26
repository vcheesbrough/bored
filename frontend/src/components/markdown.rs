use leptos::prelude::*;
use pulldown_cmark::{html, Event, Parser, TagEnd};

fn to_html(md: &str) -> String {
    // Strip raw HTML and dangerous URI schemes to prevent stored XSS via `inner_html`.
    // `skip_depth` counts how many nested dangerous link/image Starts are open; we
    // suppress each End only while the counter is non-zero. A counter (not a bool) is
    // needed because images can nest inside links — e.g. [![alt](js:src)](js:href)
    // produces two Start events before the first End.
    let mut skip_depth: u32 = 0;
    let parser = Parser::new(md).filter(|event| match event {
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
    let mut html_output = String::new();
    html::push_html(&mut html_output, parser);
    html_output
}

#[cfg(test)]
mod tests {
    use super::to_html;

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
}

/// Renders a markdown string as HTML inside a `<div>`.
///
/// `inner_html` is Leptos 0.7's built-in special attribute that calls
/// `set_inner_html()` on the underlying DOM element reactively.
#[component]
pub fn MarkdownPreview(
    #[prop(into)] body: Signal<String>,
    #[prop(optional)] class: &'static str,
) -> impl IntoView {
    view! {
        <div class=class inner_html=move || to_html(&body.get())></div>
    }
}
