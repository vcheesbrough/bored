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

use leptos::leptos_dom::helpers::{request_animation_frame, window_event_listener};
use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;

use super::BoardMode;
use super::layout::{ColumnInput, Layout, LinkInput, Params, layout};
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
    /// The middle of the line, where a reason's badge is centred.
    label_x: i32,
    label_y: i32,
    /// Which way the reason's tooltip opens — see [`tooltip_placement`].
    tip_align: TipAlign,
    tip_below: bool,
}

/// How a reason tooltip lines up with its badge, horizontally.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum TipAlign {
    /// Starts at the badge and runs right — for a badge near the left edge.
    Start,
    /// Centred over the badge — the usual case.
    Center,
    /// Ends at the badge and runs left — for a badge near the right edge.
    End,
}

impl TipAlign {
    /// The CSS class that applies this alignment (`style.css`).
    fn class(self) -> &'static str {
        match self {
            TipAlign::Start => "graph-tip-start",
            TipAlign::Center => "graph-tip-center",
            TipAlign::End => "graph-tip-end",
        }
    }
}

/// The widest a reason tooltip gets (`.graph-edge-tooltip`'s `max-width` in
/// `style.css`), plus a little room to spare at the canvas edge.
const TIP_MAX_WIDTH: i32 = 260;
const TIP_EDGE_MARGIN: i32 = 8;
/// Room a tooltip needs above its badge: the longest reason (200 characters)
/// wraps to about five lines at the tooltip's width, ~90px with padding, plus
/// the gap to the badge.
const TIP_ROOM_ABOVE: i32 = 110;

/// Which way a reason tooltip for a badge at `(x, y)` should open so that it
/// stays inside a canvas `canvas_width` wide — and so inside the graph body,
/// which scrolls over exactly that canvas and would otherwise clip it (or
/// grow a scrollbar for it).
///
/// It opens above the badge, centred, unless that would cross the canvas's
/// top or a side; then it flips below, or lines up with the badge from the
/// side it has room on. Returns `(alignment, opens_below)`.
fn tooltip_placement(x: i32, y: i32, canvas_width: i32) -> (TipAlign, bool) {
    let half = TIP_MAX_WIDTH / 2 + TIP_EDGE_MARGIN;
    let align = if x < half {
        TipAlign::Start
    } else if x > canvas_width - half {
        TipAlign::End
    } else {
        TipAlign::Center
    };
    (align, y < TIP_ROOM_ABOVE)
}

/// Everything the view draws, recomputed as one value. `PartialEq` lets the
/// `Memo` skip notifying the view when a change upstream (a card edit that
/// leaves its title alone, say) produces the very same picture.
#[derive(Clone, Debug, Default, PartialEq)]
struct GraphModel {
    width: i32,
    height: i32,
    bands: Vec<Band>,
    /// The nodes and the reason badges, in keyboard (tab) order — see
    /// [`tab_order`].
    items: Vec<Focusable>,
    edges: Vec<EdgeView>,
}

/// Something in the graph a keyboard can reach: a card's node, or a link's
/// reason badge.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Focusable {
    Node(NodeView),
    Reason(EdgeView),
}

/// The nodes and reason badges in the order Tab should visit them: the
/// nodes in reading order (column by column, top to bottom as text mode
/// lists them), each reason badge straight after the later of its link's two
/// cards — so a keyboard user meets a reason having just passed the cards it
/// explains, not in a block after every card on the board. A badge whose
/// cards are not drawn (cannot happen: the layout draws only links between
/// drawn cards) would go at the end.
fn tab_order(nodes: &[NodeView], edges: &[EdgeView]) -> Vec<Focusable> {
    let index_of = |number: u32| nodes.iter().position(|n| n.number == number);
    // For each node, the badges that follow it, in link order.
    let mut after: Vec<Vec<&EdgeView>> = vec![Vec::new(); nodes.len()];
    let mut orphans = Vec::new();
    for edge in edges.iter().filter(|e| e.reason.is_some()) {
        match (index_of(edge.from_number), index_of(edge.to_number)) {
            (Some(a), Some(b)) => after[a.max(b)].push(edge),
            _ => orphans.push(edge),
        }
    }
    let mut items = Vec::new();
    for (node, badges) in nodes.iter().zip(after) {
        items.push(Focusable::Node(node.clone()));
        items.extend(badges.into_iter().map(|e| Focusable::Reason(e.clone())));
    }
    items.extend(orphans.into_iter().map(|e| Focusable::Reason(e.clone())));
    items
}

/// The last layout and the inputs it was computed from.
///
/// The layout runs a search bounded by a work budget (up to ~15 ms natively
/// for a tangled component, perhaps twice that in the browser — see
/// `layout::COMPONENT_WORK`), and most changes the graph's memo sees leave
/// every position where it was: a card's body edited, a reason reworded, a
/// card renumbered. The layout's inputs hold only geometry, card ids and
/// link ends — never text — so those changes produce equal inputs, and the
/// cache hands back the previous layout without searching again.
#[derive(Default)]
struct LayoutCache {
    last: Option<(Vec<ColumnInput>, Vec<LinkInput>, Layout)>,
}

impl LayoutCache {
    /// The layout for these inputs: the cached one if they equal the last
    /// inputs (`true` — a hit), else `compute`'s, which is then cached
    /// (`false`).
    fn layout_for(
        &mut self,
        columns: Vec<ColumnInput>,
        links: Vec<LinkInput>,
        compute: impl FnOnce(&[ColumnInput], &[LinkInput]) -> Layout,
    ) -> (Layout, bool) {
        if let Some((c, l, laid_out)) = &self.last
            && *c == columns
            && *l == links
        {
            return (laid_out.clone(), true);
        }
        let laid_out = compute(&columns, &links);
        self.last = Some((columns, links, laid_out.clone()));
        (laid_out, false)
    }
}

/// One column's layout input: where it is, whether it is collapsed, and its
/// cards' ids in text-mode order. Card text is deliberately not part of it
/// (see [`LayoutCache`]).
fn column_input(left: f64, width: f64, collapsed: bool, cards: &[shared::Card]) -> ColumnInput {
    ColumnInput {
        left,
        width,
        collapsed,
        cards: cards.iter().map(|card| card.id.clone()).collect(),
    }
}

/// The layout's view of the board's links: ids and ends only, not reasons.
fn link_inputs(links: &[shared::CardLink]) -> Vec<LinkInput> {
    links
        .iter()
        .map(|link| LinkInput {
            id: link.id.clone(),
            predecessor: link.predecessor_id.clone(),
            successor: link.successor_id.clone(),
        })
        .collect()
}

/// How long the header row must stop resizing before the graph re-measures
/// it, in ms. A window drag fires the `ResizeObserver` every frame, and
/// laying the graph out on every one would spend the search's budget sixty
/// times a second; meanwhile the previous layout stays on screen.
const RESIZE_SETTLE_MS: u32 = 100;

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
    //
    // The very first callback (observing fires one at once) measures
    // straight away, so the graph appears without a delay. After that each
    // callback only (re)starts a [`RESIZE_SETTLE_MS`] timer, and the
    // measurement runs when it fires: a drag re-lays the graph out once, at
    // its end. `pending` holds the timer; a `gloo_timers` `Timeout` cancels
    // itself when dropped, so replacing it cancels the previous one, and
    // dropping the closure at cleanup (which drops `pending` with it) cancels
    // any still waiting. `Rc<RefCell<…>>`: shared by every call of the
    // closure, mutable through a shared reference, single-threaded.
    let pending: std::rc::Rc<std::cell::RefCell<Option<gloo_timers::callback::Timeout>>> =
        std::rc::Rc::default();
    let on_resize = Closure::<dyn Fn()>::new(move || {
        let first = measured
            .try_with_untracked(|m| m.columns.is_empty())
            .unwrap_or(false);
        if first {
            measure();
            return;
        }
        let timeout = gloo_timers::callback::Timeout::new(RESIZE_SETTLE_MS, measure);
        if let Ok(mut slot) = pending.try_borrow_mut() {
            *slot = Some(timeout);
        }
    });
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
    // tracked on purpose — that is what makes the graph live. The layout
    // itself is reused while its inputs are unchanged (see [`LayoutCache`]);
    // a `StoredValue` rather than a signal, since nothing should re-run
    // because the cache changed.
    let layout_cache: StoredValue<LayoutCache> = StoredValue::new(LayoutCache::default());

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
            inputs.push(column_input(*left, *width, is_collapsed, &cards));
        }

        // `try_update_value` (the view may be unmounting) runs the closure on
        // the stored cache, which needs `&mut` to remember a new layout.
        let laid_out = layout_cache
            .try_update_value(|cache| {
                cache
                    .layout_for(inputs, link_inputs(&links), |columns, links| {
                        layout(columns, links, &Params::default())
                    })
                    .0
            })
            .unwrap_or_default();

        let nodes: Vec<NodeView> = laid_out
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

        // Needed before the edges: a tooltip's placement depends on it.
        let width = bands.iter().map(|b| b.left + b.width).max().unwrap_or(0);

        let edges: Vec<EdgeView> = laid_out
            .edges
            .iter()
            .filter_map(|edge| {
                let link = links.iter().find(|l| l.id == edge.link_id)?;
                let from_number = card_info.get(&edge.predecessor)?.0;
                let to_number = card_info.get(&edge.successor)?.0;
                // A blank reason is no reason.
                let reason = link
                    .reason
                    .as_deref()
                    .map(str::trim)
                    .filter(|r| !r.is_empty())
                    .map(str::to_string);
                let (label_x, label_y) = (
                    edge.label_at.0.round() as i32,
                    edge.label_at.1.round() as i32,
                );
                let (tip_align, tip_below) = tooltip_placement(label_x, label_y, width);
                Some(EdgeView {
                    link_id: edge.link_id.clone(),
                    from_number,
                    to_number,
                    x1: edge.from.0.round() as i32,
                    y1: edge.from.1.round() as i32,
                    x2: edge.to.0.round() as i32,
                    y2: edge.to.1.round() as i32,
                    reason,
                    label_x,
                    label_y,
                    tip_align,
                    tip_below,
                })
            })
            .collect();

        GraphModel {
            width,
            height: laid_out.height.ceil() as i32,
            bands,
            items: tab_order(&nodes, &edges),
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

    // Escape hides an open reason tooltip without moving focus or the
    // pointer (WCAG 2.1 SC 1.4.13, "dismissible"). One flag for the whole
    // graph: Escape sets it and CSS hides every tooltip; entering or focusing
    // a badge clears it, so the next one opens as usual. A window listener,
    // because Escape must work while the pointer merely hovers a badge, when
    // the key goes to whatever has focus; it is removed on cleanup.
    let tips_dismissed = RwSignal::new(false);
    let escape_listener = StoredValue::new(Some(window_event_listener(
        leptos::ev::keydown,
        move |ev| {
            if ev.key() == "Escape" {
                let _ = tips_dismissed.try_set(true);
            }
        },
    )));
    on_cleanup(move || {
        escape_listener.update_value(|listener| {
            if let Some(listener) = listener.take() {
                listener.remove();
            }
        });
    });

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
                class:graph-tips-dismissed=move || tips_dismissed.try_get().unwrap_or(false)
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
                // The nodes and the reason badges, in one list so the DOM —
                // and therefore Tab — visits each badge straight after its
                // link's cards (see `tab_order`). Badges still draw above
                // nodes: their CSS `z-index` puts them there whatever the
                // DOM order.
                <For
                    each=move || model.try_with(|m| m.items.clone()).unwrap_or_default()
                    key=|item| item.clone()
                    children=move |item| match item {
                        Focusable::Node(node) => graph_node(node),
                        Focusable::Reason(edge) => reason_badge(edge, tips_dismissed),
                    }
                />
            </div>
        </div>
    }
    .into_any()
}

/// One card's node: a plain link, which the router turns into an in-app
/// navigation to `?card=N` — opening the card's modal over the graph.
fn graph_node(node: NodeView) -> AnyView {
    let label = format!("#{} {}", node.number, node.title);
    view! {
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
    .into_any()
}

/// A link's reason: a small "i" badge at the middle of its line.
///
/// The reason itself is in a tooltip shown on hover *and* on keyboard focus
/// — the badge is focusable for exactly that. The tooltip is a child of the
/// badge, so CSS alone can show it (`:hover`, `:focus`), and moving the
/// pointer from the badge onto the tooltip keeps it open (`style.css`
/// bridges the gap between them). Its side and direction come from
/// [`tooltip_placement`].
///
/// For a screen reader the badge is named after its link ("Reason for the
/// link from #118 to #11" — each badge's name is different) and described
/// by the tooltip, so focusing it announces which link and why.
fn reason_badge(edge: EdgeView, tips_dismissed: RwSignal<bool>) -> AnyView {
    let reason = edge.reason.clone().unwrap_or_default();
    // Link ids are ULIDs — letters and digits — so they make a valid,
    // page-unique element id. Two owned copies: the badge points at the
    // tooltip by id, and the tooltip carries it.
    let tip_id = format!("graph-reason-{}", edge.link_id);
    let described_by = tip_id.clone();
    let name = format!(
        "Reason for the link from #{} to #{}",
        edge.from_number, edge.to_number
    );
    let class = format!(
        "graph-edge-reason {}{}",
        edge.tip_align.class(),
        if edge.tip_below {
            " graph-tip-below"
        } else {
            ""
        }
    );
    // Entering or focusing a badge undoes an earlier Escape.
    let reopen = move || {
        let _ = tips_dismissed.try_set(false);
    };
    view! {
        <span
            class=class
            role="img"
            tabindex="0"
            aria-label=name
            aria-describedby=described_by
            data-link-id=edge.link_id.clone()
            style:left=px(edge.label_x)
            style:top=px(edge.label_y)
            on:mouseenter=move |_| reopen()
            on:focus=move |_| reopen()
        >
            <span class="graph-edge-reason-glyph" aria-hidden="true">"i"</span>
            <span class="graph-edge-tooltip" role="tooltip" id=tip_id>
                {reason}
            </span>
        </span>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::{
        EdgeView, Focusable, LayoutCache, NodeView, TipAlign, column_input, link_inputs, tab_order,
        tooltip_placement,
    };
    use crate::graph::layout::{ColumnInput, Layout, LinkInput};

    // ── Reason tooltip placement ───────────────────────────────────────────
    //
    // A 1000px canvas. The tooltip is up to 260px wide, so a centred one
    // needs 130px either side of its badge, plus an 8px margin: 138px.

    #[test]
    fn a_tooltip_in_open_space_opens_above_centred() {
        assert_eq!(tooltip_placement(500, 300, 1000), (TipAlign::Center, false));
    }

    #[test]
    fn a_tooltip_near_a_side_lines_up_from_the_side_with_room() {
        assert_eq!(tooltip_placement(137, 300, 1000), (TipAlign::Start, false));
        assert_eq!(tooltip_placement(138, 300, 1000), (TipAlign::Center, false));
        assert_eq!(tooltip_placement(862, 300, 1000), (TipAlign::Center, false));
        assert_eq!(tooltip_placement(863, 300, 1000), (TipAlign::End, false));
    }

    #[test]
    fn a_tooltip_near_the_top_opens_below() {
        assert_eq!(tooltip_placement(500, 109, 1000), (TipAlign::Center, true));
        assert_eq!(tooltip_placement(500, 110, 1000), (TipAlign::Center, false));
        // A corner flips both ways.
        assert_eq!(tooltip_placement(20, 40, 1000), (TipAlign::Start, true));
    }

    // ── The layout cache ───────────────────────────────────────────────────

    fn card(id: &str, number: u32, body: &str) -> shared::Card {
        shared::Card {
            id: id.to_string(),
            column_id: "col".to_string(),
            body: body.to_string(),
            position: 0,
            number,
            tags: Vec::new(),
            last_edited_by: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn card_link(id: &str, from: &str, to: &str, reason: Option<&str>) -> shared::CardLink {
        shared::CardLink {
            id: id.to_string(),
            predecessor_id: from.to_string(),
            successor_id: to.to_string(),
            predecessor_number: 0,
            successor_number: 0,
            predecessor_board_slug: String::new(),
            successor_board_slug: String::new(),
            reason: reason.map(str::to_string),
            last_edited_by: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    /// Lay out through `cache` and report whether it hit. The stand-in
    /// layout records that it ran by returning a non-default height.
    fn through(cache: &mut LayoutCache, columns: Vec<ColumnInput>, links: Vec<LinkInput>) -> bool {
        let mut ran = false;
        let (_, hit) = cache.layout_for(columns, links, |_, _| {
            ran = true;
            Layout {
                height: 1.0,
                ..Layout::default()
            }
        });
        assert_eq!(hit, !ran, "a hit must not run the layout, a miss must");
        hit
    }

    #[test]
    fn editing_a_cards_text_or_a_reason_reuses_the_layout() {
        let mut cache = LayoutCache::default();
        let before = [card("a", 1, "# First"), card("b", 2, "# Second")];
        let links = [card_link("l", "a", "b", Some("why"))];
        assert!(!through(
            &mut cache,
            vec![column_input(0.0, 474.0, false, &before)],
            link_inputs(&links),
        ));
        // The same cards with new bodies and a new number, the same link
        // with a new reason: nothing the layout reads has changed.
        let after = [card("a", 7, "# First, renamed"), card("b", 2, "Rewritten")];
        let reworded = [card_link("l", "a", "b", Some("a better reason"))];
        assert!(through(
            &mut cache,
            vec![column_input(0.0, 474.0, false, &after)],
            link_inputs(&reworded),
        ));
    }

    #[test]
    fn a_new_link_a_new_width_or_a_collapse_lays_out_again() {
        let mut cache = LayoutCache::default();
        let cards = [card("a", 1, "A"), card("b", 2, "B")];
        let column =
            |width: f64, collapsed: bool| vec![column_input(0.0, width, collapsed, &cards)];
        assert!(!through(&mut cache, column(474.0, false), Vec::new()));
        // A link appears.
        let linked = link_inputs(&[card_link("l", "a", "b", None)]);
        assert!(!through(&mut cache, column(474.0, false), linked.clone()));
        // The window is resized.
        assert!(!through(&mut cache, column(500.0, false), linked.clone()));
        // The column is collapsed.
        assert!(!through(&mut cache, column(500.0, true), linked.clone()));
        // And nothing changes: a hit again.
        assert!(through(&mut cache, column(500.0, true), linked));
    }

    // ── Keyboard order ─────────────────────────────────────────────────────

    fn node_view(number: u32) -> NodeView {
        NodeView {
            card_id: format!("c{number}"),
            number,
            title: String::new(),
            href: String::new(),
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        }
    }

    fn edge_view(from: u32, to: u32, reason: Option<&str>) -> EdgeView {
        EdgeView {
            link_id: format!("{from}->{to}"),
            from_number: from,
            to_number: to,
            x1: 0,
            y1: 0,
            x2: 0,
            y2: 0,
            reason: reason.map(str::to_string),
            label_x: 0,
            label_y: 0,
            tip_align: TipAlign::Center,
            tip_below: false,
        }
    }

    /// Short names for an order, to compare against: `#N` for a node,
    /// `from->to` for a badge.
    fn names(items: &[Focusable]) -> Vec<String> {
        items
            .iter()
            .map(|item| match item {
                Focusable::Node(n) => format!("#{}", n.number),
                Focusable::Reason(e) => e.link_id.clone(),
            })
            .collect()
    }

    #[test]
    fn each_reason_badge_follows_the_later_of_its_links_cards() {
        // Nodes in reading order #1, #2, #3. 3 → 1 has a reason: its later
        // card in that order is #3, so its badge comes after #3. 1 → 2 has
        // one too: after #2. 2 → 3 has none: no badge at all.
        let nodes = [node_view(1), node_view(2), node_view(3)];
        let edges = [
            edge_view(3, 1, Some("why")),
            edge_view(1, 2, Some("because")),
            edge_view(2, 3, None),
        ];
        assert_eq!(
            names(&tab_order(&nodes, &edges)),
            ["#1", "#2", "1->2", "#3", "3->1"]
        );
    }
}
