//! Drawing the board link graph (card #303).
//!
//! [`GraphView`] is mounted by `BoardView` below the column headers while the
//! board is in graph mode. It does three things:
//!
//! 1. **Measures the column headers.** Graph mode keeps the board's columns
//!    and their headers, so a node has to land under its own column's header.
//!    Rather than recompute the board's flex layout, the view reads where the
//!    browser actually put each header (`offsetLeft` / `offsetWidth`) and
//!    re-reads it whenever that can have changed: a `ResizeObserver` on the
//!    header row covers window resizes, and an effect covers columns being
//!    added, removed, reordered, collapsed or expanded.
//! 2. **Lays the graph out.** A `Memo` turns the measurements, the board's
//!    cards (through `BoardCardIndex`), its links (`BoardLinkIndex`) and its
//!    collapsed columns (`CollapsedColumns`) into a [`GraphModel`] via
//!    [`super::layout::layout`]. Every one of those is a signal the board keeps
//!    current over SSE, so any change anywhere — a card created, edited, moved
//!    or deleted, a link added or removed, a column collapsed — recomputes the
//!    picture. Nothing here fetches anything.
//! 3. **Draws it.** One scrolling body (the single scrollbar on the right)
//!    holds a canvas: column bands at the back, an SVG layer of lines with
//!    arrowheads, reason labels, and then the nodes — plain `<a>` links that
//!    open the card's modal over the graph.
//!
//! The graph is read-only: no drag, no inline edit, no link editing. Clicking a
//! node opens the ordinary card modal, which edits as normal, and whatever it
//! changes flows back here through the same signals.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use leptos::leptos_dom::helpers::request_animation_frame;
use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;

use super::BoardMode;
use super::layout::{ColumnInput, LinkInput, Params, layout};
use crate::collapsed::CollapsedColumns;
use crate::links::BoardLinkIndex;
use crate::search::BoardCardIndex;

/// Where the browser put the column headers, in px from the header row's left
/// edge: `(column id, left, width)` in on-screen order.
#[derive(Clone, Debug, Default, PartialEq)]
struct Measured {
    columns: Vec<(String, f64, f64)>,
}

/// One column's background stripe on the canvas. A collapsed column's stripe
/// is its 48px strip, carrying the column's name (its header has room only for
/// the expand button).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Band {
    column_id: String,
    name: String,
    left: i32,
    width: i32,
    collapsed: bool,
}

/// One card's node, ready to draw. Positions are whole pixels so the struct
/// can be `Eq + Hash` and serve as its own key in a `<For>`: a node whose
/// position, number or title changes is re-created, the rest are left alone.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct NodeView {
    card_id: String,
    number: u32,
    title: String,
    href: String,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// One link's line, ready to draw — keyed the same way as [`NodeView`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct EdgeView {
    link_id: String,
    /// Card numbers of the two ends, exposed as `data-from` / `data-to` for
    /// tests and for anyone inspecting the DOM.
    from_number: u32,
    to_number: u32,
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
    reason: Option<String>,
    label_x: i32,
    label_y: i32,
}

/// Everything the view draws, recomputed as one value. `PartialEq` lets the
/// `Memo` skip notifying the view when a change upstream (a card edit that
/// leaves its title alone, say) produces the very same picture.
#[derive(Clone, Debug, Default, PartialEq)]
struct GraphModel {
    width: i32,
    height: i32,
    bands: Vec<Band>,
    nodes: Vec<NodeView>,
    edges: Vec<EdgeView>,
}

/// A CSS pixel length.
fn px(value: i32) -> String {
    format!("{value}px")
}

/// Read the header row's column positions.
///
/// `offsetLeft` is measured from the nearest positioned ancestor, which graph
/// mode makes the header row itself (`position: relative` in `style.css`), and
/// it ignores the row's horizontal scroll — exactly the canvas coordinates the
/// layout wants.
fn measure_header(row: &web_sys::HtmlElement) -> Measured {
    let mut columns = Vec::new();
    // `:scope >` restricts the query to the row's direct children, so nothing
    // nested inside a column (there is nothing today) could be picked up.
    let Ok(list) = row.query_selector_all(":scope > .column-view") else {
        return Measured::default();
    };
    for i in 0..list.length() {
        // `item` returns a `Node`; `dyn_into` is the checked downcast to the
        // element type that has `offset_left`. Anything else is skipped.
        let Some(element) = list
            .item(i)
            .and_then(|node| node.dyn_into::<web_sys::HtmlElement>().ok())
        else {
            continue;
        };
        let Some(id) = element.get_attribute("data-column-id") else {
            continue;
        };
        columns.push((
            id,
            f64::from(element.offset_left()),
            f64::from(element.offset_width()),
        ));
    }
    Measured { columns }
}

/// The graph body: one scroll container under the column headers.
///
/// `header_row` is the `.columns-row` element `BoardView` renders the headers
/// in; `board_slug` is the board's slug, for the nodes' links.
#[component]
pub fn GraphView(header_row: NodeRef<leptos::html::Div>, board_slug: RwSignal<String>) -> AnyView {
    // caps monomorphization at this boundary — see CardVersionActions doc comment in history_panel.rs

    // ── Board state, from `BoardView`'s contexts ───────────────────────────
    // All of these are `Copy` handles into the reactive arena, so each closure
    // below can capture its own copy with `move` — no cloning needed.
    let columns = expect_context::<RwSignal<Vec<RwSignal<shared::Column>>>>();
    let card_index = expect_context::<BoardCardIndex>();
    let link_index = expect_context::<BoardLinkIndex>();
    let collapsed_columns = expect_context::<CollapsedColumns>();

    let measured: RwSignal<Measured> = RwSignal::new(Measured::default());
    let body_ref = NodeRef::<leptos::html::Div>::new();

    // Set false when this view unmounts. The animation-frame callbacks below
    // run *after* the current task, so one can fire once the view is gone;
    // they check this first. An `Arc<AtomicBool>` rather than an `Rc<Cell>`
    // because `on_cleanup` demands `Send + Sync`.
    let alive = Arc::new(AtomicBool::new(true));

    // ── 1. Measuring ──────────────────────────────────────────────────────
    //
    // `measure` captures only `Copy` handles, so the closure is `Copy` too and
    // can be handed to the observer and to every animation frame by value.
    let measure = move || {
        let Some(row) = header_row.try_get_untracked().flatten() else {
            return;
        };
        let now = measure_header(&row);
        // Write only on a real change, so an observer firing for a resize
        // that moved nothing does not recompute the layout.
        if measured.try_with_untracked(|old| *old != now) == Some(true) {
            let _ = measured.try_set(now);
        }
    };

    // Window resizes (and anything else that resizes the header row) arrive
    // through a `ResizeObserver`. Its JS callback is a wasm-bindgen
    // `Closure`, which must stay alive as long as the observer can call it, so
    // both are kept until cleanup — then the observer is disconnected *before*
    // the closure is dropped, and nothing can call into freed memory. The
    // browser passes the callback `(entries, observer)`; it needs neither, and
    // JS ignores extra arguments to a function that takes none.
    let on_resize = Closure::<dyn Fn()>::new(measure);
    match web_sys::ResizeObserver::new(on_resize.as_ref().unchecked_ref()) {
        Ok(observer) => {
            // The header row's element exists by the time effects run; the
            // effect re-runs (harmlessly — `observe` is idempotent) only if
            // the `NodeRef` is ever pointed at a new element. Observing fires
            // the callback once straight away, which takes the first
            // measurement.
            let observing = observer.clone();
            Effect::new(move |_| {
                if let Some(row) = header_row.get() {
                    observing.observe(&row);
                }
            });
            // `SendWrapper` because `on_cleanup` demands `Send + Sync` and
            // neither a JS object nor a `Closure` is; a wasm SPA has a single
            // thread, so the wrapper's "only on the creating thread" check can
            // never fail.
            let held = send_wrapper::SendWrapper::new((observer, on_resize));
            on_cleanup(move || {
                held.0.disconnect();
                drop(held);
            });
        }
        Err(err) => {
            // No `ResizeObserver` (a very old browser): the graph still draws,
            // from the measurements the effect below takes, but will not
            // follow a window resize until something else changes.
            crate::telemetry::error_detail(
                "graph view: ResizeObserver unavailable",
                &format!("{err:?}"),
                "graph_measure",
            );
            drop(on_resize);
        }
    }

    // Columns added, removed or reordered, and columns collapsed or expanded,
    // change the headers without necessarily resizing the row, so the
    // observer above would miss them. This effect subscribes to both lists
    // and re-measures — on the next animation frame, because the effect can
    // run before the headers' own DOM has been updated for the change.
    let alive_for_frames = alive.clone();
    Effect::new(move |_| {
        columns.track();
        collapsed_columns.ids.track();
        let alive = alive_for_frames.clone();
        request_animation_frame(move || {
            if alive.load(Ordering::Relaxed) {
                measure();
            }
        });
    });

    on_cleanup(move || {
        alive.store(false, Ordering::Relaxed);
        // The header row only scrolls sideways while the graph drives it (see
        // `sync_scroll`); hand it back to text mode unscrolled.
        if let Some(row) = header_row.try_get_untracked().flatten() {
            row.set_scroll_left(0);
        }
    });

    // ── 2. Laying out ─────────────────────────────────────────────────────
    //
    // A `Memo`: it re-runs whenever any signal it reads changes, and only
    // notifies the view when the resulting model is different. Every read is
    // tracked on purpose — that is what makes the graph live.
    let model = Memo::new(move |_| {
        let measured = measured.get();
        let collapsed = collapsed_columns.ids.get();
        let entries = card_index.0.get();
        let links = link_index.links.get();
        let slug = board_slug.get();

        let mut inputs: Vec<ColumnInput> = Vec::new();
        let mut bands: Vec<Band> = Vec::new();
        // Card id → (number, title), to dress the layout's bare ids.
        let mut card_info: std::collections::HashMap<String, (u32, String)> =
            std::collections::HashMap::new();

        for column_signal in columns.get() {
            // `try_get`: a column deleted mid-update has a disposed signal.
            let Some(column) = column_signal.try_get() else {
                continue;
            };
            // Not measured yet (just added): left out until the next frame's
            // measurement places it.
            let Some((_, left, width)) =
                measured.columns.iter().find(|(id, _, _)| *id == column.id)
            else {
                continue;
            };
            let is_collapsed = collapsed.contains(&column.id);
            // A collapsed column's cards are not read at all, so edits to
            // them do not recompute a picture they are not in.
            let cards: Vec<shared::Card> = if is_collapsed {
                Vec::new()
            } else {
                entries
                    .iter()
                    .find(|(id, _)| *id == column.id)
                    .and_then(|(_, cards)| cards.try_get())
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|card| card.try_get())
                    .collect()
            };
            for card in &cards {
                card_info.insert(
                    card.id.clone(),
                    (
                        card.number,
                        shared::history::summary::card_title_from_body(&card.body),
                    ),
                );
            }
            bands.push(Band {
                column_id: column.id.clone(),
                name: column.name.clone(),
                left: left.round() as i32,
                width: width.round() as i32,
                collapsed: is_collapsed,
            });
            inputs.push(ColumnInput {
                left: *left,
                width: *width,
                collapsed: is_collapsed,
                cards: cards.into_iter().map(|card| card.id).collect(),
            });
        }

        let link_inputs: Vec<LinkInput> = links
            .iter()
            .map(|link| LinkInput {
                id: link.id.clone(),
                predecessor: link.predecessor_id.clone(),
                successor: link.successor_id.clone(),
            })
            .collect();

        let laid_out = layout(&inputs, &link_inputs, &Params::default());

        let nodes = laid_out
            .nodes
            .iter()
            .filter_map(|node| {
                let (number, title) = card_info.get(&node.card_id)?.clone();
                Some(NodeView {
                    card_id: node.card_id.clone(),
                    number,
                    title,
                    href: BoardMode::Graph.card_path(&slug, number),
                    x: node.rect.x.round() as i32,
                    y: node.rect.y.round() as i32,
                    w: node.rect.w.round() as i32,
                    h: node.rect.h.round() as i32,
                })
            })
            .collect();

        let edges = laid_out
            .edges
            .iter()
            .filter_map(|edge| {
                let link = links.iter().find(|l| l.id == edge.link_id)?;
                let from_number = card_info.get(&edge.predecessor)?.0;
                let to_number = card_info.get(&edge.successor)?.0;
                Some(EdgeView {
                    link_id: edge.link_id.clone(),
                    from_number,
                    to_number,
                    x1: edge.from.0.round() as i32,
                    y1: edge.from.1.round() as i32,
                    x2: edge.to.0.round() as i32,
                    y2: edge.to.1.round() as i32,
                    // A blank reason is no reason.
                    reason: link
                        .reason
                        .as_deref()
                        .map(str::trim)
                        .filter(|r| !r.is_empty())
                        .map(str::to_string),
                    label_x: edge.label_at.0.round() as i32,
                    label_y: edge.label_at.1.round() as i32,
                })
            })
            .collect();

        let width = bands.iter().map(|b| b.left + b.width).max().unwrap_or(0);
        GraphModel {
            width,
            height: laid_out.height.ceil() as i32,
            bands,
            nodes,
            edges,
        }
    });

    // ── 3. Drawing ────────────────────────────────────────────────────────
    //
    // Every binding below reads the model with `try_with` rather than `get`:
    // the memo is disposed with this component, and a view closure can run
    // during the unmount — reading a disposed memo then would panic and wedge
    // the tab. `try_with` also borrows the model, so each binding clones only
    // the field it draws.

    // The graph body scrolls both ways; the header row above it does not
    // scroll by itself (its overflow is hidden), so mirror the body's
    // sideways scroll onto it to keep each header over its own column.
    let sync_scroll = move |_: web_sys::Event| {
        if let (Some(body), Some(row)) = (body_ref.get_untracked(), header_row.get_untracked()) {
            row.set_scroll_left(body.scroll_left());
        }
    };

    view! {
        <div class="graph-body" node_ref=body_ref on:scroll=sync_scroll>
            <div
                class="graph-canvas"
                style:width=move || px(model.try_with(|m| m.width).unwrap_or(0))
                style:height=move || px(model.try_with(|m| m.height).unwrap_or(0))
            >
                <For
                    each=move || model.try_with(|m| m.bands.clone()).unwrap_or_default()
                    key=|band| band.clone()
                    children=|band| {
                        view! {
                            <div
                                class="graph-band"
                                class:graph-band-collapsed=band.collapsed
                                data-column-id=band.column_id.clone()
                                style:left=px(band.left)
                                style:width=px(band.width)
                            >
                                {band.collapsed.then(|| view! {
                                    <span class="graph-band-name">{band.name.clone()}</span>
                                })}
                            </div>
                        }
                    }
                />
                <svg
                    class="graph-edges"
                    aria-hidden="true"
                    width=move || model.try_with(|m| m.width).unwrap_or(0)
                    height=move || model.try_with(|m| m.height).unwrap_or(0)
                >
                    <defs>
                        // The arrowhead, drawn at the *after* end of every
                        // line. `refX=10` puts its tip exactly on the line's
                        // end point, which the layout clips to the successor's
                        // border; `userSpaceOnUse` keeps it the same size
                        // whatever the line's stroke width.
                        <marker
                            id="graph-arrow"
                            viewBox="0 0 10 10"
                            refX="10"
                            refY="5"
                            markerWidth="10"
                            markerHeight="10"
                            markerUnits="userSpaceOnUse"
                            orient="auto"
                        >
                            <path class="graph-arrowhead" d="M0,0 L10,5 L0,10 z" />
                        </marker>
                    </defs>
                    <For
                        each=move || model.try_with(|m| m.edges.clone()).unwrap_or_default()
                        key=|edge| edge.clone()
                        children=|edge| {
                            view! {
                                <line
                                    class="graph-edge"
                                    data-link-id=edge.link_id.clone()
                                    data-from=edge.from_number
                                    data-to=edge.to_number
                                    x1=edge.x1
                                    y1=edge.y1
                                    x2=edge.x2
                                    y2=edge.y2
                                    marker-end="url(#graph-arrow)"
                                />
                            }
                        }
                    />
                </svg>
                // Reason labels: HTML rather than SVG text, so they can wrap
                // in a halo background and elide with CSS. Centred on the
                // middle of their line by the CSS transform.
                <For
                    each=move || {
                        model
                            .try_with(|m| {
                                m.edges.iter().filter(|e| e.reason.is_some()).cloned().collect::<Vec<_>>()
                            })
                            .unwrap_or_default()
                    }
                    key=|edge| edge.clone()
                    children=|edge| {
                        // Two owned copies: one for the tooltip (the label may
                        // be elided by CSS), one for the text.
                        let reason = edge.reason.clone().unwrap_or_default();
                        let tooltip = reason.clone();
                        view! {
                            <div
                                class="graph-edge-label"
                                data-link-id=edge.link_id.clone()
                                title=tooltip
                                style:left=px(edge.label_x)
                                style:top=px(edge.label_y)
                            >
                                {reason}
                            </div>
                        }
                    }
                />
                <For
                    each=move || model.try_with(|m| m.nodes.clone()).unwrap_or_default()
                    key=|node| node.clone()
                    children=|node| {
                        let label = format!("#{} {}", node.number, node.title);
                        view! {
                            // A plain link: the router turns the click into
                            // an in-app navigation to `?card=N`, which opens
                            // the card's modal over the graph.
                            <a
                                class="graph-node"
                                href=node.href.clone()
                                data-card-number=node.number
                                title=label
                                style:left=px(node.x)
                                style:top=px(node.y)
                                style:width=px(node.w)
                                style:height=px(node.h)
                            >
                                <span class="graph-node-number">{format!("#{}", node.number)}</span>
                                <span class="graph-node-title">{node.title.clone()}</span>
                            </a>
                        }
                    }
                />
            </div>
        </div>
    }
    .into_any()
}
