use leptos::prelude::*;
use pulldown_cmark::{html, Event, Parser};

fn to_html(md: &str) -> String {
    // Strip raw HTML events before rendering to prevent stored XSS: a card body
    // containing `<script>` or inline event handlers would otherwise be injected
    // verbatim into the DOM via `inner_html`.
    let parser = Parser::new(md).filter(|event| {
        !matches!(event, Event::Html(_) | Event::InlineHtml(_))
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
