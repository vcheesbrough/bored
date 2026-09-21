//! Make a WASM panic *visible* instead of a silently dead tab (card #368).
//!
//! A panic in this app is fatal to the tab. The module is built with
//! `panic = "abort"` semantics on wasm32: the panic hook runs, then the module
//! hits an `unreachable` trap. When that happens inside a Leptos reactive read
//! it also wedges the `wasm-bindgen-futures` task queue, so no effect ever runs
//! again, in-flight saves never finish, and every later click is inert. Before
//! this banner the user saw a board that simply stopped responding — the report
//! behind #304 was "it doesn't work and from that point on the UI doesn't work
//! properly".
//!
//! This does not prevent panics. It makes them loud.
//!
//! # Why plain DOM calls
//!
//! The banner is built with raw `web_sys` calls on `<body>`, **outside** the
//! Leptos tree, because the reactive runtime is exactly what has just died:
//! anything rendered through a signal or a `view!` would never appear.
//!
//! The Reload control is an ordinary `<a href>` to the current page for the
//! same reason. A click handler would be a Rust closure, and calling back into
//! a module that has trapped — possibly half way through an allocation or with
//! a `RefCell` still borrowed — is not something to rely on. A link reloads
//! with no Rust of ours involved; it is marked so the router's global click
//! interception leaves it to the browser (see `show`).

use wasm_bindgen::JsCast;

/// The banner's element id. Also how a second panic finds the first banner
/// rather than stacking another on top of it.
const BANNER_ID: &str = "panic-banner";

/// The custom window event that panics on purpose, for the e2e suite.
const TEST_PANIC_EVENT: &str = "bored:test-panic";

/// What the banner says, whatever the build.
const HEADLINE: &str = "Something went wrong and this page has stopped working. \
                        Reload to continue.";

/// Install the panic hook. Call once, first thing in `main`.
pub fn install() {
    std::panic::set_hook(Box::new(|info| {
        // Unchanged from before this card, and first: the e2e suite's liveness
        // checks look for exactly this console line, and the log must land even
        // if building the banner itself goes wrong.
        leptos::logging::error!("wasm panic: {info}");
        show(detail(cfg!(debug_assertions), &info.to_string()).as_deref());
    }));
}

/// The extra line shown under the headline: the panic message and location in
/// a debug build, where the reader is a developer hunting a trap; nothing in a
/// release build, where the reader is a user and the console still has it all.
///
/// Pure, so it is unit tested on the host.
fn detail(debug_build: bool, panic_info: &str) -> Option<String> {
    debug_build.then(|| panic_info.to_string())
}

/// Where the Reload link points: the current page, minus any `#fragment`.
///
/// The fragment matters. A link that differs from the current URL only by its
/// fragment is an in-page scroll, not a navigation, so it would not reload
/// anything.
fn reload_href(current: &str) -> &str {
    current.split('#').next().unwrap_or(current)
}

/// Put the banner on the page. Every step is fallible and every failure is
/// ignored: this runs inside a panic hook, where a second panic would abort
/// without even the console line, and the banner is a courtesy on top of that
/// line — not something worth failing over.
fn show(detail: Option<&str>) {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    // Idempotent: one banner, however many panics follow the first.
    if document.get_element_by_id(BANNER_ID).is_some() {
        return;
    }
    let Some(body) = document.body() else {
        return;
    };
    let Ok(banner) = document.create_element("div") else {
        return;
    };
    banner.set_id(BANNER_ID);
    banner.set_class_name("panic-banner");
    // An alert, so a screen reader announces it at once.
    let _ = banner.set_attribute("role", "alert");

    if let Ok(headline) = document.create_element("p") {
        headline.set_class_name("panic-banner-text");
        headline.set_text_content(Some(HEADLINE));
        let _ = banner.append_child(&headline);
    }

    if let Some(detail) = detail
        && let Ok(pre) = document.create_element("pre")
    {
        pre.set_class_name("panic-banner-detail");
        pre.set_text_content(Some(detail));
        let _ = banner.append_child(&pre);
    }

    if let Ok(reload) = document.create_element("a") {
        reload.set_class_name("panic-banner-reload");
        let href = document
            .location()
            .and_then(|l| l.href().ok())
            .unwrap_or_default();
        let _ = reload.set_attribute("href", reload_href(&href));
        // leptos_router listens for clicks on every same-origin `<a>`, cancels
        // them and navigates client-side — through the very runtime that has
        // just died, so without these the click silently does nothing. Its
        // handler steps aside for a link with a `target` or `rel="external"`;
        // either alone suffices, both say what is meant: a real page load.
        let _ = reload.set_attribute("target", "_self");
        let _ = reload.set_attribute("rel", "external");
        reload.set_text_content(Some("Reload"));
        let _ = banner.append_child(&reload);
    }

    let _ = body.append_child(&banner);
}

/// Listen for [`TEST_PANIC_EVENT`] on `window` and panic when it fires.
///
/// Production code on purpose, because the e2e suite runs the exact image that
/// deploys: there is no test-only build to hide it in. It is harmless there —
/// only a script already running in the page can dispatch the event, and such
/// a script can break its own tab in any number of ways without it.
pub fn install_test_trigger() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let on_event = wasm_bindgen::closure::Closure::<dyn Fn()>::new(|| {
        panic!("deliberate panic from the `{TEST_PANIC_EVENT}` event");
    });
    let _ = window
        .add_event_listener_with_callback(TEST_PANIC_EVENT, on_event.as_ref().unchecked_ref());
    // The listener lives as long as the page; hand the closure to JS for good.
    on_event.forget();
}

#[cfg(test)]
mod tests {
    use super::{detail, reload_href};

    #[test]
    fn a_debug_build_shows_the_panic() {
        assert_eq!(
            detail(true, "panicked at src/x.rs:1:1: boom"),
            Some("panicked at src/x.rs:1:1: boom".to_string())
        );
    }

    #[test]
    fn a_release_build_keeps_the_banner_short() {
        assert_eq!(detail(false, "panicked at src/x.rs:1:1: boom"), None);
    }

    #[test]
    fn reload_keeps_the_path_and_query() {
        assert_eq!(
            reload_href("https://bored.example/boards/b?card=7"),
            "https://bored.example/boards/b?card=7"
        );
    }

    #[test]
    fn reload_drops_a_fragment_so_the_link_navigates() {
        assert_eq!(
            reload_href("https://bored.example/boards/b?card=7#top"),
            "https://bored.example/boards/b?card=7"
        );
    }
}
