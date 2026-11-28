//! Layout tests (card #303).
//!
//! Every expected position below is worked out by hand from [`params`], and
//! every property check (overlap, a line through a node) is written here,
//! independently of the layout code — never by calling the layout's own
//! helpers — so a mistake in the engine cannot also be a mistake in its oracle.
//!
//! With [`params`]: nodes are 100×40, the slot pitch is 100 + 20 = 120, so a
//! 130px column holds 1 node per row and a 250px column holds 2. The row pitch
//! is 40 + 30 = 70 and the first row starts at y = 10, so row `r` is at
//! `10 + 70r`. A lone node in a 250px column starting at `L` is centred at
//! `L + 75`; a full row of two starts at `L + 15` and `L + 135`.

use super::*;

fn params() -> Params {
    Params {
        node_w: 100.0,
        node_h: 40.0,
        h_gap: 20.0,
        row_gap: 30.0,
        pad_top: 10.0,
        pad_bottom: 10.0,
    }
}

fn column(left: f64, width: f64, cards: &[&str]) -> ColumnInput {
    ColumnInput {
        left,
        width,
        collapsed: false,
        cards: cards.iter().map(|c| c.to_string()).collect(),
    }
}

fn link(from: &str, to: &str) -> LinkInput {
    LinkInput {
        id: format!("{from}->{to}"),
        predecessor: from.to_string(),
        successor: to.to_string(),
    }
}

/// The node for `card`, which must be in the layout.
fn node<'a>(layout: &'a Layout, card: &str) -> &'a NodeBox {
    layout
        .nodes
        .iter()
        .find(|n| n.card_id == card)
        .unwrap_or_else(|| panic!("no node for {card}"))
}

/// `(x, y)` of `card`'s top-left corner.
fn at(layout: &Layout, card: &str) -> (f64, f64) {
    let r = node(layout, card).rect;
    (r.x, r.y)
}

// ── Independent oracles ─────────────────────────────────────────────────

/// Do two rectangles share any interior area? Plain interval overlap on both
/// axes; touching edges do not count.
fn overlaps(a: &Rect, b: &Rect) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

/// Does the segment pass through the interior of `r`? Brute force: walk the
/// segment in steps of at most a quarter pixel and test each point. Nothing
/// like the clipping the layout uses, which is the point.
fn passes_through(from: (f64, f64), to: (f64, f64), r: &Rect) -> bool {
    let length = ((to.0 - from.0).powi(2) + (to.1 - from.1).powi(2)).sqrt();
    let steps = (length * 4.0).ceil().max(1.0) as usize;
    (0..=steps).any(|i| {
        let t = i as f64 / steps as f64;
        let (x, y) = (from.0 + t * (to.0 - from.0), from.1 + t * (to.1 - from.1));
        x > r.x && x < r.x + r.w && y > r.y && y < r.y + r.h
    })
}

fn assert_no_overlaps(layout: &Layout) {
    for (i, a) in layout.nodes.iter().enumerate() {
        for b in &layout.nodes[i + 1..] {
            assert!(
                !overlaps(&a.rect, &b.rect),
                "{} and {} overlap: {:?} {:?}",
                a.card_id,
                b.card_id,
                a.rect,
                b.rect
            );
        }
    }
}

fn assert_no_line_through_a_node(layout: &Layout) {
    for edge in &layout.edges {
        for n in &layout.nodes {
            if n.card_id == edge.predecessor || n.card_id == edge.successor {
                continue;
            }
            assert!(
                !passes_through(edge.from, edge.to, &n.rect),
                "line {} passes through {}",
                edge.link_id,
                n.card_id
            );
        }
    }
}

/// Is `point` on the border of `r` (within a hair)?
fn on_border(point: (f64, f64), r: &Rect) -> bool {
    let eps = 1e-6;
    let inside_x = point.0 >= r.x - eps && point.0 <= r.x + r.w + eps;
    let inside_y = point.1 >= r.y - eps && point.1 <= r.y + r.h + eps;
    let on_vertical_side =
        ((point.0 - r.x).abs() < eps || (point.0 - (r.x + r.w)).abs() < eps) && inside_y;
    let on_horizontal_side =
        ((point.1 - r.y).abs() < eps || (point.1 - (r.y + r.h)).abs() < eps) && inside_x;
    on_vertical_side || on_horizontal_side
}

// ── Basic geometry ──────────────────────────────────────────────────────

#[test]
fn a_same_column_link_puts_the_after_card_below_the_before_card() {
    // Text mode lists a above b, but b comes first.
    let layout = layout(
        &[column(0.0, 130.0, &["a", "b"])],
        &[link("b", "a")],
        &params(),
    );
    assert_eq!(at(&layout, "b"), (15.0, 10.0));
    assert_eq!(at(&layout, "a"), (15.0, 80.0));
    // Straight down, from b's bottom edge to a's top edge.
    let edge = &layout.edges[0];
    assert_eq!(edge.from, (65.0, 50.0));
    assert_eq!(edge.to, (65.0, 80.0));
    assert_eq!(edge.label_at, (65.0, 65.0));
    // Two rows: 10 + 2·70 − 30 + 10.
    assert_eq!(layout.height, 130.0);
}

#[test]
fn a_cross_column_link_with_free_rows_runs_level() {
    let layout = layout(
        &[column(0.0, 130.0, &["a"]), column(130.0, 130.0, &["x"])],
        &[link("x", "a")],
        &params(),
    );
    assert_eq!(at(&layout, "a"), (15.0, 10.0));
    assert_eq!(at(&layout, "x"), (145.0, 10.0));
    // From x's left side to a's right side, at mid-height.
    assert_eq!(layout.edges[0].from, (145.0, 30.0));
    assert_eq!(layout.edges[0].to, (115.0, 30.0));
}

#[test]
fn a_fan_out_of_three_in_a_two_slot_column_wraps_to_a_second_row() {
    let layout = layout(
        &[column(0.0, 250.0, &["p", "c1", "c2", "c3"])],
        &[link("p", "c1"), link("p", "c2"), link("p", "c3")],
        &params(),
    );
    assert_eq!(at(&layout, "p"), (75.0, 10.0));
    assert_eq!(at(&layout, "c1"), (15.0, 80.0));
    assert_eq!(at(&layout, "c2"), (135.0, 80.0));
    // The third child wraps below, centred, so p's line to it runs down the
    // gap between its siblings.
    assert_eq!(at(&layout, "c3"), (75.0, 150.0));
    assert_no_line_through_a_node(&layout);
}

#[test]
fn unlinked_cards_keep_column_order_two_to_a_row() {
    let layout = layout(
        &[column(0.0, 250.0, &["a", "b", "c", "d", "e"])],
        &[],
        &params(),
    );
    assert_eq!(at(&layout, "a"), (15.0, 10.0));
    assert_eq!(at(&layout, "b"), (135.0, 10.0));
    assert_eq!(at(&layout, "c"), (15.0, 80.0));
    assert_eq!(at(&layout, "d"), (135.0, 80.0));
    assert_eq!(at(&layout, "e"), (75.0, 150.0));
    assert!(layout.edges.is_empty());
}

#[test]
fn a_component_starts_below_anything_in_the_columns_it_spans() {
    // t2 (todo) ← d1 (done) skips the middle column, so the component claims
    // all three columns and starts below m1, which is in the middle one —
    // and its level line has the middle column's row to itself.
    let columns = [
        column(0.0, 250.0, &["t1", "t2"]),
        column(250.0, 250.0, &["m1"]),
        column(500.0, 250.0, &["d1"]),
    ];
    let layout = layout(&columns, &[link("d1", "t2")], &params());
    assert_eq!(at(&layout, "t1"), (75.0, 10.0));
    assert_eq!(at(&layout, "m1"), (325.0, 10.0));
    // Each end of the one link takes the slot nearest the other — t2 the
    // right slot of its column (L + 135), d1 the left (L + 15) — which is
    // the shortest level line.
    assert_eq!(at(&layout, "t2"), (135.0, 80.0));
    assert_eq!(at(&layout, "d1"), (515.0, 80.0));
    assert_no_line_through_a_node(&layout);
}

#[test]
fn two_components_never_share_rows_over_overlapping_columns() {
    // Two separate chains spanning the same columns.
    let columns = [
        column(0.0, 250.0, &["a1", "b1"]),
        column(250.0, 250.0, &["a2", "b2"]),
    ];
    let layout = layout(&columns, &[link("a2", "a1"), link("b2", "b1")], &params());
    let rows = |cards: &[&str]| {
        let ys: Vec<f64> = cards.iter().map(|c| at(&layout, c).1).collect();
        let top = ys.iter().copied().fold(f64::INFINITY, f64::min);
        let bottom = ys.iter().copied().fold(f64::NEG_INFINITY, f64::max) + 40.0;
        (top, bottom)
    };
    let (a_top, a_bottom) = rows(&["a1", "a2"]);
    let (b_top, b_bottom) = rows(&["b1", "b2"]);
    assert!(
        a_bottom <= b_top || b_bottom <= a_top,
        "components share rows"
    );
    // And concretely: a's chain first, level on row 0; b's on row 1. Each
    // link's ends sit in the facing slots (L + 135 and L + 15), the
    // shortest level line.
    assert_eq!(at(&layout, "a1"), (135.0, 10.0));
    assert_eq!(at(&layout, "a2"), (265.0, 10.0));
    assert_eq!(at(&layout, "b1"), (135.0, 80.0));
    assert_eq!(at(&layout, "b2"), (265.0, 80.0));
}

#[test]
fn a_lone_predecessor_sits_level_with_the_card_it_leads_to() {
    // x leads only to c, the bottom of a three-card chain in the next column
    // over. Ranked from the top, x would sit on row 0 with a long diagonal;
    // pulled down, it sits beside c and its line runs level.
    let layout = layout(
        &[
            column(0.0, 130.0, &["a", "b", "c"]),
            column(130.0, 130.0, &["x"]),
        ],
        &[link("a", "b"), link("b", "c"), link("x", "c")],
        &params(),
    );
    assert_eq!(at(&layout, "a"), (15.0, 10.0));
    assert_eq!(at(&layout, "b"), (15.0, 80.0));
    assert_eq!(at(&layout, "c"), (15.0, 150.0));
    assert_eq!(at(&layout, "x"), (145.0, 150.0));
}

#[test]
fn nodes_in_a_row_are_ordered_towards_their_neighbours() {
    // Text mode lists q before p in the middle column, but p's link comes
    // from the left and q's from the right; swapping them keeps both lines
    // level and clear of each other. (Both lead on to s, which makes the
    // five cards one component and so puts p and q in one row.)
    let columns = [
        column(0.0, 250.0, &["l"]),
        column(250.0, 250.0, &["q", "p", "s"]),
        column(500.0, 250.0, &["r"]),
    ];
    let links = [
        link("l", "p"),
        link("r", "q"),
        link("p", "s"),
        link("q", "s"),
    ];
    let layout = layout(&columns, &links, &params());
    // l and r take the slots facing the middle column (L + 135, L + 15);
    // s sits centred below p and q — its two lines then total ~79px of
    // drawn line against ~90px under either of them.
    assert_eq!(at(&layout, "l"), (135.0, 10.0));
    assert_eq!(at(&layout, "p"), (265.0, 10.0));
    assert_eq!(at(&layout, "q"), (385.0, 10.0));
    assert_eq!(at(&layout, "r"), (515.0, 10.0));
    assert_eq!(at(&layout, "s"), (325.0, 80.0));
    assert_no_line_through_a_node(&layout);
}

// ── Clearing lines through nodes ────────────────────────────────────────

#[test]
fn a_line_skipping_a_column_tilts_past_the_card_in_between() {
    // d → t skips the middle column, where d's other successor m would sit
    // on the same row. Level, the line would run straight through m; so m
    // drops a row instead, into the slot nearest d (L + 135), and d → t runs
    // level and short between the facing slots (L + 135, L + 15) over an
    // empty middle row.
    let columns = [
        column(0.0, 250.0, &["t"]),
        column(250.0, 250.0, &["m"]),
        column(500.0, 250.0, &["d"]),
    ];
    let layout = layout(&columns, &[link("d", "m"), link("d", "t")], &params());
    assert_eq!(at(&layout, "d"), (515.0, 10.0));
    assert_eq!(at(&layout, "m"), (385.0, 80.0));
    assert_eq!(at(&layout, "t"), (135.0, 10.0));
    assert_no_line_through_a_node(&layout);
}

/// Todo | Doing | Done, one card each, 250px columns.
fn three_columns_t_m_d() -> [ColumnInput; 3] {
    [
        column(0.0, 250.0, &["t"]),
        column(250.0, 250.0, &["m"]),
        column(500.0, 250.0, &["d"]),
    ]
}

#[test]
fn a_skipping_line_into_a_card_the_middle_card_also_leads_to_tilts_clear() {
    // d (Done) and m (Doing) both lead into t (Todo). Level, the line d → t
    // would lie on top of m → t and read as d → m → t. Instead m moves up a
    // row, so d → t runs level over an empty middle row and m → t comes
    // down into t from above. Hand-worked: rows at y = 10 and 80; every card
    // in the slot nearest the cards it links to — t right (L + 135), m left
    // (L + 15), d left (L + 15).
    let layout = layout(
        &three_columns_t_m_d(),
        &[link("m", "t"), link("d", "t")],
        &params(),
    );
    assert_eq!(at(&layout, "d"), (515.0, 80.0));
    assert_eq!(at(&layout, "m"), (265.0, 10.0));
    assert_eq!(at(&layout, "t"), (135.0, 80.0));
    assert_no_line_through_a_node(&layout);
}

#[test]
fn a_transitive_link_across_columns_tilts_clear_of_the_chain() {
    // The e2e fixture's Done → Doing → Todo chain plus the transitive
    // Done → Todo link. Level, d → t would hide m → t and invent a d → m → t
    // reading. Hand-worked: d on row 0, m and t level on row 1; t right
    // (L + 135) and d left (L + 15), facing each other. m centred (L + 75):
    // in its left slot d → t would cut through it, and centred its two lines
    // total ~186px of drawn line against ~213px in its right slot.
    let layout = layout(
        &three_columns_t_m_d(),
        &[link("d", "m"), link("m", "t"), link("d", "t")],
        &params(),
    );
    assert_eq!(at(&layout, "d"), (515.0, 10.0));
    assert_eq!(at(&layout, "m"), (325.0, 80.0));
    assert_eq!(at(&layout, "t"), (135.0, 80.0));
    assert_no_line_through_a_node(&layout);
}

#[test]
fn a_transitive_link_in_one_column_nudges_the_middle_card_aside() {
    // a → b → c and a → c: centred, a's line to c would run through b.
    let layout = layout(
        &[column(0.0, 250.0, &["a", "b", "c"])],
        &[link("a", "b"), link("b", "c"), link("a", "c")],
        &params(),
    );
    assert_eq!(at(&layout, "a"), (75.0, 10.0));
    assert_eq!(at(&layout, "b"), (15.0, 80.0));
    assert_eq!(at(&layout, "c"), (75.0, 150.0));
    assert_no_line_through_a_node(&layout);
}

#[test]
fn the_sketch_board_has_no_overlaps_and_no_line_through_a_node() {
    // The card's sketch: two todo cards lead into a hub, which fans out to
    // three, one of which also leads to another; an in-progress card leads
    // into the hub and a done card into the in-progress card; a second done
    // card leads, skipping a column, to the last todo card. Default sizes,
    // three 370px columns.
    let columns = [
        column(
            0.0,
            370.0,
            &["top1", "top2", "hub", "left", "right", "bottom", "last"],
        ),
        column(370.0, 370.0, &["ip"]),
        column(740.0, 370.0, &["done1", "done2"]),
    ];
    let links = [
        link("top1", "hub"),
        link("top2", "hub"),
        link("hub", "left"),
        link("hub", "right"),
        link("hub", "bottom"),
        link("right", "bottom"),
        link("ip", "hub"),
        link("done1", "ip"),
        link("done2", "last"),
    ];
    let layout = layout(&columns, &links, &Params::default());
    assert_eq!(layout.nodes.len(), 10);
    assert_eq!(layout.edges.len(), 9);
    assert_no_overlaps(&layout);
    assert_no_line_through_a_node(&layout);
    // Inside a column, every link reads downward.
    for edge in &layout.edges {
        let (from, to) = (
            node(&layout, &edge.predecessor),
            node(&layout, &edge.successor),
        );
        if from.column == to.column {
            assert!(to.rect.y > from.rect.y, "{} reads upward", edge.link_id);
        }
    }
    // done1 → ip runs level: nothing pushes ip down past done1.
    assert_eq!(at(&layout, "done1").1, at(&layout, "ip").1);
}

// ── The user's board (card #303 review) ─────────────────────────────────

/// Do two segments cross at a point inside both? Solved directly: the two
/// lines `a + t(b − a)` and `c + u(d − c)` meet where a 2×2 linear system
/// says (Cramer's rule); they cross if both `t` and `u` lie strictly inside
/// (0, 1). Parallel segments never count. A different method from the
/// engine's orientation test, on purpose.
fn crosses(a: (f64, f64), b: (f64, f64), c: (f64, f64), d: (f64, f64)) -> bool {
    let (r, s) = ((b.0 - a.0, b.1 - a.1), (d.0 - c.0, d.1 - c.1));
    let denominator = r.0 * s.1 - r.1 * s.0;
    if denominator.abs() < 1e-12 {
        return false;
    }
    let (qx, qy) = (c.0 - a.0, c.1 - a.1);
    let t = (qx * s.1 - qy * s.0) / denominator;
    let u = (qx * r.1 - qy * r.0) / denominator;
    let inside = |v: f64| v > 1e-9 && v < 1.0 - 1e-9;
    inside(t) && inside(u)
}

/// Every pair of drawn lines that share no card and cross.
fn crossing_pairs(layout: &Layout) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for (i, e) in layout.edges.iter().enumerate() {
        for f in &layout.edges[i + 1..] {
            let ends = [&e.predecessor, &e.successor];
            if ends.contains(&&f.predecessor) || ends.contains(&&f.successor) {
                continue;
            }
            if crosses(e.from, e.to, f.from, f.to) {
                pairs.push((e.link_id.clone(), f.link_id.clone()));
            }
        }
    }
    pairs
}

/// The dev board `manual-testing`, where the user could see avoidable
/// crossings and lines through cards: three columns (cards in text-mode
/// order, card numbers as ids) of `width` px.
fn manual_testing_board(width: f64) -> Layout {
    let cards: [&[&str]; 3] = [
        &["135", "120", "130", "122", "121", "119", "11", "10"],
        &["131", "118", "13"],
        &["132", "15"],
    ];
    let columns: Vec<ColumnInput> = cards
        .iter()
        .enumerate()
        .map(|(i, ids)| column(i as f64 * width, width, ids))
        .collect();
    let links = [
        link("11", "10"),
        link("13", "11"),
        link("15", "11"),
        link("15", "135"),
        link("118", "11"),
        link("118", "122"),
        link("120", "119"),
        link("121", "119"),
        link("121", "120"),
        link("122", "119"),
        link("122", "121"),
    ];
    layout(&columns, &links, &Params::default())
}

#[test]
fn the_manual_testing_board_has_no_line_through_a_card_at_any_width() {
    // 474px: three columns at a 1440px window; 420px: a narrower window;
    // 370px: the minimum column width; 330px: narrower still, one slot per
    // row.
    for width in [474.0, 420.0, 370.0, 330.0] {
        let layout = manual_testing_board(width);
        assert_eq!(layout.nodes.len(), 13, "at {width}px");
        assert_eq!(layout.edges.len(), 11, "at {width}px");
        assert_no_overlaps(&layout);
        assert_no_line_through_a_node(&layout);

        let crossings = crossing_pairs(&layout);
        // The user's two named links cross nothing.
        for named in ["15->135", "11->10"] {
            assert!(
                !crossings.iter().any(|(a, b)| a == named || b == named),
                "{named} crosses a line at {width}px: {crossings:?}"
            );
        }
        // Inside a column, every link reads downward.
        for edge in &layout.edges {
            let (from, to) = (
                node(&layout, &edge.predecessor),
                node(&layout, &edge.successor),
            );
            if from.column == to.column {
                assert!(to.rect.y > from.rect.y, "{} reads upward", edge.link_id);
            }
        }
        // At 1440px (474px columns) the board had four crossings before card
        // #303's review. The search now reaches none there, nor at the
        // minimum column width; at 330px (one slot per row) one is left,
        // inside the 122 → 121 → 120 → 119 chain.
        let allowed = if width == 330.0 { 1 } else { 0 };
        assert!(crossings.len() <= allowed, "at {width}px: {crossings:?}");

        // Compact: at most one empty row between the top and the last card.
        // (The search keeps a gap only where closing it would cost more —
        // put a line through a card, say; at 420px and 330px it keeps one.)
        // Rows are worked out from the default sizes: the first at y = 20,
        // then one every 58 + 44 = 102px.
        let mut rows: Vec<usize> = layout
            .nodes
            .iter()
            .map(|n| ((n.rect.y - 20.0) / 102.0).round() as usize)
            .collect();
        rows.sort_unstable();
        rows.dedup();
        let span = rows.last().map_or(0, |last| last + 1);
        assert!(span - rows.len() <= 1, "at {width}px, rows {rows:?}");
        // And the board starts at the top: something is on the first row.
        assert_eq!(rows.first(), Some(&0), "at {width}px");
    }
}

// ── Collapsed columns and empty boards ──────────────────────────────────

#[test]
fn a_collapsed_columns_cards_and_their_links_are_absent() {
    let mut hidden = column(130.0, 130.0, &["x"]);
    hidden.collapsed = true;
    let columns = [
        column(0.0, 130.0, &["a"]),
        hidden,
        column(260.0, 130.0, &["y"]),
    ];
    let layout = layout(&columns, &[link("x", "a"), link("y", "a")], &params());
    let cards: Vec<&str> = layout.nodes.iter().map(|n| n.card_id.as_str()).collect();
    assert_eq!(cards, ["a", "y"]);
    let edges: Vec<&str> = layout.edges.iter().map(|e| e.link_id.as_str()).collect();
    assert_eq!(edges, ["y->a"]);
}

#[test]
fn an_empty_board_lays_out_to_nothing() {
    assert_eq!(layout(&[], &[], &params()), Layout::default());
    let empty_column = layout(&[column(0.0, 250.0, &[])], &[], &params());
    assert!(empty_column.nodes.is_empty());
    assert_eq!(empty_column.height, 0.0);
}

#[test]
fn links_to_cards_not_on_the_board_are_ignored() {
    // A cross-board link (its other end is on no visible column) and a
    // self-link draw nothing and do not disturb the order.
    let layout = layout(
        &[column(0.0, 130.0, &["a", "b"])],
        &[link("elsewhere", "a"), link("a", "a")],
        &params(),
    );
    assert!(layout.edges.is_empty());
    assert_eq!(at(&layout, "a"), (15.0, 10.0));
    assert_eq!(at(&layout, "b"), (15.0, 80.0));
}

#[test]
fn a_one_card_row_never_vanishes_in_a_narrow_column() {
    // Narrower than one slot: still one node per row.
    let layout = layout(&[column(0.0, 50.0, &["a", "b"])], &[], &params());
    assert_eq!(layout.nodes.len(), 2);
    assert_ne!(at(&layout, "a").1, at(&layout, "b").1);
}

// ── Scale ───────────────────────────────────────────────────────────────

#[test]
fn a_two_hundred_card_board_lays_out_without_overlaps() {
    // A deterministic pseudo-random board: 200 cards over three columns and
    // links only from an earlier-created card to a later one, so the links
    // cannot form a cycle (as the server guarantees).
    let mut seed: u64 = 0x5eed;
    let mut next = move || {
        // A 64-bit linear congruential generator (Knuth's MMIX constants).
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) as usize
    };
    let names: Vec<String> = (0..200).map(|i| format!("c{i}")).collect();
    let mut by_column: Vec<Vec<&str>> = vec![Vec::new(); 3];
    for name in &names {
        by_column[next() % 3].push(name.as_str());
    }
    let columns: Vec<ColumnInput> = by_column
        .iter()
        .enumerate()
        .map(|(i, cards)| column(i as f64 * 370.0, 370.0, cards))
        .collect();
    let mut links = Vec::new();
    for _ in 0..150 {
        let (a, b) = (next() % 200, next() % 200);
        if a < b {
            links.push(link(&names[a], &names[b]));
        }
    }

    let layout = layout(&columns, &links, &Params::default());

    assert_eq!(layout.nodes.len(), 200);
    assert_no_overlaps(&layout);
    for edge in &layout.edges {
        let (from, to) = (
            node(&layout, &edge.predecessor),
            node(&layout, &edge.successor),
        );
        assert!(on_border(edge.from, &from.rect), "{} start", edge.link_id);
        assert!(on_border(edge.to, &to.rect), "{} end", edge.link_id);
        if from.column == to.column {
            assert!(to.rect.y > from.rect.y, "{} reads upward", edge.link_id);
        }
    }
    // Every node inside its own column.
    for n in &layout.nodes {
        let c = &columns[n.column];
        assert!(n.rect.x >= c.left && n.rect.x + n.rect.w <= c.left + c.width);
        assert!(n.rect.y + n.rect.h <= layout.height);
    }
}
