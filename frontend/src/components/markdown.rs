use leptos::prelude::*;
use pulldown_cmark::{html, Event, Parser, TagEnd};

fn to_html(md: &str) -> String {
    // Strip raw HTML and dangerous URI schemes to prevent stored XSS via `inner_html`.
    let parser = Parser::new(md).filter(|event| match event {
        // Raw HTML blocks and inline HTML inject verbatim into the DOM.
        Event::Html(_) | Event::InlineHtml(_) => false,
        // Block dangerous URI schemes in link/image destinations.
        // Also drop the matching End events so push_html doesn't emit stray
        // </a> or </img> closing tags for every filtered Start — safe because
        // Markdown doesn't allow nested links or images.
        Event::Start(pulldown_cmark::Tag::Link { dest_url, .. })
        | Event::Start(pulldown_cmark::Tag::Image { dest_url, .. }) => {
            let lower = dest_url.to_lowercase();
            !lower.starts_with("javascript:")
                && !lower.starts_with("vbscript:")
                && !lower.starts_with("data:")
        }
        Event::End(TagEnd::Link | TagEnd::Image) => false,
        _ => true,
    });
    let mut html_output = String::new();
    html::push_html(&mut html_output, parser);
    html_output
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
