use leptos::prelude::*;
use pulldown_cmark::{html, Event, Parser};

fn to_html(md: &str) -> String {
    // Strip raw HTML events and block `javascript:`/`vbscript:` URIs in link
    // and image destinations to prevent stored XSS via `inner_html`.
    let parser = Parser::new(md).filter(|event| match event {
        // Raw HTML blocks / inline HTML inject verbatim into the DOM.
        Event::Html(_) | Event::InlineHtml(_) => false,
        // `javascript:` and `vbscript:` href/src values execute on click/load.
        Event::Start(pulldown_cmark::Tag::Link { dest_url, .. })
        | Event::Start(pulldown_cmark::Tag::Image { dest_url, .. }) => {
            let lower = dest_url.to_lowercase();
            !lower.starts_with("javascript:") && !lower.starts_with("vbscript:")
        }
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
    #[prop(into)]
    body: Signal<String>,
    #[prop(optional)]
    class: &'static str,
) -> impl IntoView {
    view! {
        <div class=class inner_html=move || to_html(&body.get())></div>
    }
}
