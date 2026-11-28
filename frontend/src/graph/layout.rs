//! The board link graph's layout engine (card #303) — pure Rust, no DOM.
//!
//! Given the board's visible columns (where they sit on screen and which cards
//! they hold, in text-mode order) and the links between those cards, work out
//! where every card's node goes and where every link's line runs. Everything
//! here is plain data in, plain data out, so it is unit-tested on the host
//! target (`cargo test -p frontend`) with no browser and no reactive runtime;
//! `graph::view` is what turns the result into SVG and HTML.
//!
//! # The shape of the picture
//!
//! The graph keeps the board's columns: a card's node always sits inside its
//! own column. Rows are shared across every column, so two linked cards in
//! different columns can sit side by side at the same height. Within a column
//! a row holds as many fixed-width nodes as fit (two at the 370px minimum
//! column width).
//!
//! # The algorithm, step by step
//!
//! 1. **Visible cards and links.** Cards in collapsed columns are left out,
//!    and so is every link with an end outside the visible cards (a collapsed
//!    column, another board, a card not loaded yet).
//! 2. **Components.** Cards joined by links, ignoring direction, form a
//!    *component* (union–find). A card with no visible link is a component of
//!    one — a *singleton*.
//! 3. **Ranks** (per component). A rank is a logical row. A link inside one
//!    column must read downward, so its successor ranks at least one below its
//!    predecessor; a link across columns only needs the successor *no higher*,
//!    which lets it run horizontally when nothing else pushes it down. Ranks
//!    are the longest path under those weights, then every card with
//!    successors is pulled down as far as they allow, so a lone predecessor
//!    sits beside or just above what it leads to instead of at the very top.
//! 4. **Rows and slots.** Each (rank, column) group of nodes wraps into as
//!    many sub-rows as it needs at that column's slots per row; a rank is as
//!    tall as its tallest column. Nodes within a group are ordered by a few
//!    barycentre sweeps (each node moves towards the mean x of its
//!    neighbours), ties broken by text-mode order, which removes most
//!    crossings.
//! 5. **Clearing lines through nodes.** A straight line can still run through
//!    a third node of its own component. Each such conflict is resolved and
//!    the component laid out again, a bounded number of times: a node blocking
//!    a line between two cards of its own column is *nudged* to the left of
//!    its row (lines between centred nodes run down the middle), and anything
//!    else lengthens that one link by a rank — its successor must then sit a
//!    rank further below its predecessor — so the line tilts past the
//!    blocker.
//!    Then a **geometric search** polishes the component against a cost that
//!    is what the user sees — lines through cards, crossings, lines pointing
//!    up between columns, rows used, and total line length — by moving cards
//!    between the rows and slots of their own column (greedy descent, plus
//!    for small components a few seeded runs of simulated annealing and an
//!    exhaustive re-slotting of each column). Every search is bounded and
//!    seeded, so the same board always gives the same picture. Crossings
//!    are minimised, not guaranteed away.
//! 6. **Placing components.** Components go down the page in the order of
//!    their earliest card (text-mode index, then column). A component claims
//!    every column from its leftmost to its rightmost card and starts below
//!    whatever already occupies any of them (a per-column "skyline"), so no
//!    line can pass through another component's nodes. Consecutive singletons
//!    in one column share rows, as many per row as fit, in column order.
//! 7. **Lines.** Each link is a straight segment between the two nodes'
//!    centres, clipped to their borders, with its label point at the middle of
//!    what remains.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Sizes the layout works in, in CSS pixels. `graph::view` uses
/// [`Params::default`]; tests pass round numbers so expected positions can be
/// worked out by hand.
#[derive(Clone, Debug, PartialEq)]
pub struct Params {
    /// Width of every node.
    pub node_w: f64,
    /// Height of every node.
    pub node_h: f64,
    /// Horizontal space between two nodes in the same row of a column. Also
    /// part of the slots-per-row arithmetic — see [`slots_per_row`].
    pub h_gap: f64,
    /// Vertical space between rows: room for an arrowhead and a reason label.
    pub row_gap: f64,
    /// Space above the first row.
    pub pad_top: f64,
    /// Space below the last row.
    pub pad_bottom: f64,
}

impl Default for Params {
    /// 160 + 20 = 180px per slot, so a 370px column (the board's minimum
    /// width) holds two nodes per row, as the card decided.
    fn default() -> Self {
        Self {
            node_w: 160.0,
            node_h: 58.0,
            h_gap: 20.0,
            row_gap: 44.0,
            pad_top: 20.0,
            pad_bottom: 24.0,
        }
    }
}

/// One board column, as the layout needs it.
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnInput {
    /// Left edge of the column, in px from the left of the graph canvas.
    pub left: f64,
    /// Column width in px.
    pub width: f64,
    /// A collapsed column shows no nodes, and links touching its cards are
    /// not drawn.
    pub collapsed: bool,
    /// The column's card ids, top to bottom as text mode shows them.
    pub cards: Vec<String>,
}

/// One link, as the layout needs it: its id and its two ends' card ids.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkInput {
    pub id: String,
    /// The card that comes first — where the arrow starts.
    pub predecessor: String,
    /// The card that comes after — where the arrow points.
    pub successor: String,
}

/// An axis-aligned rectangle: top-left corner plus size, in px.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn right(&self) -> f64 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f64 {
        self.y + self.h
    }

    pub fn center(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    /// The same rectangle grown by `by` on every side.
    fn inflate(&self, by: f64) -> Rect {
        Rect {
            x: self.x - by,
            y: self.y - by,
            w: self.w + 2.0 * by,
            h: self.h + 2.0 * by,
        }
    }
}

/// Where one card's node goes.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeBox {
    pub card_id: String,
    /// Index into the `columns` slice the layout was given.
    pub column: usize,
    pub rect: Rect,
}

/// Where one link's line runs: from the predecessor's border to the
/// successor's border (the arrowhead end).
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeLine {
    pub link_id: String,
    pub predecessor: String,
    pub successor: String,
    pub from: (f64, f64),
    pub to: (f64, f64),
    /// The middle of the drawn segment — where a reason label is centred.
    pub label_at: (f64, f64),
}

/// The whole picture.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layout {
    /// One per visible card, in column order then text-mode order.
    pub nodes: Vec<NodeBox>,
    /// One per drawn link, in the order the links were given.
    pub edges: Vec<EdgeLine>,
    /// Height of the canvas needed to hold every row; 0 with no nodes.
    pub height: f64,
}

/// How many nodes fit side by side in a column `width` px wide: one slot is a
/// node plus a gap. Never zero — a column narrower than one slot still holds
/// one node per row (it overhangs the column, which is better than vanishing).
pub fn slots_per_row(width: f64, p: &Params) -> usize {
    let slot = p.node_w + p.h_gap;
    if slot <= 0.0 || !width.is_finite() {
        return 1;
    }
    // `as usize` on a float truncates towards zero (and saturates negatives to
    // 0), which is exactly the floor the card's formula asks for.
    ((width / slot) as usize).max(1)
}

/// How many rounds of "find a line through a node, fix it, lay out again" a
/// component gets. Each round costs one full component layout; the bound is
/// what keeps a pathological component from spinning.
const MAX_ROUNDS: usize = 12;
/// How many times one link may be lengthened by a rank to clear its line.
const MAX_BUMPS: u8 = 3;
/// How many barycentre ordering sweeps each layout round runs.
const SWEEPS: usize = 4;
/// A line passing this close to a node counts as passing through it, so lines
/// never graze a node's border either.
const CLEARANCE: f64 = 4.0;

/// One visible card, as the layout tracks it internally.
#[derive(Clone, Copy, Debug)]
struct Card {
    /// Index into the `columns` slice.
    col: usize,
    /// Position within its column in text mode (0 = top).
    text: usize,
}

/// One drawable link between two visible cards, by card index.
#[derive(Clone, Copy, Debug)]
struct Edge {
    from: usize,
    to: usize,
    /// Index into the `links` slice, to get the id back out.
    link: usize,
}

/// Lay the graph out. See the module docs for the steps.
pub fn layout(columns: &[ColumnInput], links: &[LinkInput], p: &Params) -> Layout {
    layout_counting_work(columns, links, p).0
}

/// [`layout`], also returning the work units its search spent (bounded by
/// [`LAYOUT_WORK`]) — for the tests that hold the search to its budget.
pub fn layout_counting_work(
    columns: &[ColumnInput],
    links: &[LinkInput],
    p: &Params,
) -> (Layout, u64) {
    // Work units every component's search has spent so far.
    let mut work = 0u64;
    // ── Step 1: visible cards and the links between them ────────────────
    //
    // Cards are numbered in column order, then text-mode order, and that
    // number is how everything below refers to a card. The map borrows the
    // ids (`&str`) from `columns` rather than cloning them: it only lives as
    // long as this function, and `columns` outlives it.
    let mut cards: Vec<Card> = Vec::new();
    let mut ids: Vec<&str> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (col, column) in columns.iter().enumerate() {
        if column.collapsed {
            continue;
        }
        for (text, id) in column.cards.iter().enumerate() {
            // A card is in one column; a duplicate would be a transient
            // mid-move state, and drawing it twice would be wrong, so the
            // first sighting wins.
            if index.contains_key(id.as_str()) {
                continue;
            }
            index.insert(id.as_str(), cards.len());
            ids.push(id.as_str());
            cards.push(Card { col, text });
        }
    }
    if cards.is_empty() {
        return (Layout::default(), 0);
    }

    let slots: Vec<usize> = columns.iter().map(|c| slots_per_row(c.width, p)).collect();

    // Only links with both ends visible, never a self-link, and one line per
    // ordered pair even if the data somehow held two.
    let mut edges: Vec<Edge> = Vec::new();
    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    for (i, link) in links.iter().enumerate() {
        let (Some(&from), Some(&to)) = (
            index.get(link.predecessor.as_str()),
            index.get(link.successor.as_str()),
        ) else {
            continue;
        };
        if from != to && seen.insert((from, to)) {
            edges.push(Edge { from, to, link: i });
        }
    }

    // ── Step 2: components ──────────────────────────────────────────────
    let mut sets = UnionFind::new(cards.len());
    for edge in &edges {
        sets.union(edge.from, edge.to);
    }
    // Group the cards by their set's root. A `BTreeMap` keeps the grouping
    // deterministic, and members come out in card-index order.
    let mut by_root: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for card in 0..cards.len() {
        by_root.entry(sets.find(card)).or_default().push(card);
    }
    // Each edge's component, worked out once rather than per component.
    let edge_root: Vec<usize> = edges.iter().map(|e| sets.find(e.from)).collect();
    let mut components: Vec<(usize, Vec<usize>)> = by_root.into_iter().collect();
    // Earliest card first: the lowest text-mode index any member has, then
    // the leftmost column at that index.
    components.sort_by_key(|(_, members)| {
        members
            .iter()
            .map(|&m| (cards[m].text, cards[m].col))
            .min()
            .unwrap_or((usize::MAX, usize::MAX))
    });

    // ── Step 6: place the components down the page ──────────────────────
    //
    // `skyline[c]` is the first free row in column `c`. `open_row[c]` is the
    // row the last singleton in column `c` went into, which a following
    // singleton may share while nothing else has been placed in `c` since.
    let mut skyline = vec![0usize; columns.len()];
    let mut open_row: Vec<Option<usize>> = vec![None; columns.len()];
    // (column, row) → the singletons in that row, left to right.
    let mut singleton_rows: BTreeMap<(usize, usize), Vec<usize>> = BTreeMap::new();
    // Final (row, x) of every card, filled in below.
    let mut placed: Vec<(usize, f64)> = vec![(0, 0.0); cards.len()];

    for (root, members) in &components {
        if members.len() == 1 {
            let card = members[0];
            let col = cards[card].col;
            // Join the open row if it is still the bottom of this column and
            // has a free slot; otherwise start a new row.
            let joined = match open_row[col] {
                Some(row) if row + 1 == skyline[col] => {
                    let row_cards = singleton_rows.entry((col, row)).or_default();
                    if row_cards.len() < slots[col] {
                        row_cards.push(card);
                        true
                    } else {
                        false
                    }
                }
                _ => false,
            };
            if !joined {
                let row = skyline[col];
                skyline[col] += 1;
                open_row[col] = Some(row);
                singleton_rows.insert((col, row), vec![card]);
            }
            continue;
        }

        let member_edges: Vec<Edge> = edges
            .iter()
            .zip(&edge_root)
            .filter(|(_, edge_root)| *edge_root == root)
            .map(|(edge, _)| *edge)
            .collect();
        // This component's search budget: its own cap, or what the board
        // has left if that is less.
        let budget = COMPONENT_WORK.min(LAYOUT_WORK.saturating_sub(work));
        let local = layout_component(members, &member_edges, &cards, columns, &slots, p, budget);
        work += local.work;

        // The component claims every column between its outermost cards, so
        // it starts below the lowest thing in any of them.
        let first_col = members.iter().map(|&m| cards[m].col).min().unwrap_or(0);
        let last_col = members.iter().map(|&m| cards[m].col).max().unwrap_or(0);
        let top = skyline[first_col..=last_col]
            .iter()
            .copied()
            .max()
            .unwrap_or(0);
        for (card, row, x) in local.placements {
            placed[card] = (top + row, x);
        }
        for floor in &mut skyline[first_col..=last_col] {
            *floor = top + local.rows;
        }
    }

    // Singleton rows are centred once their final size is known.
    for (&(col, row), row_cards) in &singleton_rows {
        let count = row_cards.len();
        for (slot, &card) in row_cards.iter().enumerate() {
            placed[card] = (row, slot_x(&columns[col], count, slot, p));
        }
    }

    // ── Rectangles, lines and height ────────────────────────────────────
    let rects: Vec<Rect> = placed
        .iter()
        .map(|&(row, x)| Rect {
            x,
            y: row_y(row, p),
            w: p.node_w,
            h: p.node_h,
        })
        .collect();

    let nodes = cards
        .iter()
        .enumerate()
        .map(|(i, card)| NodeBox {
            card_id: ids[i].to_string(),
            column: card.col,
            rect: rects[i],
        })
        .collect();

    // Edges in the order the links were given, which keeps the output stable
    // for the view's keyed list.
    let edge_lines = edges
        .iter()
        .map(|edge| {
            let (from, to) = clipped_segment(&rects[edge.from], &rects[edge.to]);
            let link = &links[edge.link];
            EdgeLine {
                link_id: link.id.clone(),
                predecessor: link.predecessor.clone(),
                successor: link.successor.clone(),
                from,
                to,
                label_at: ((from.0 + to.0) / 2.0, (from.1 + to.1) / 2.0),
            }
        })
        .collect();

    let rows = skyline.iter().copied().max().unwrap_or(0);
    let height = if rows == 0 {
        0.0
    } else {
        p.pad_top + rows as f64 * (p.node_h + p.row_gap) - p.row_gap + p.pad_bottom
    };

    (
        Layout {
            nodes,
            edges: edge_lines,
            height,
        },
        work,
    )
}

/// Top of row `row`.
fn row_y(row: usize, p: &Params) -> f64 {
    p.pad_top + row as f64 * (p.node_h + p.row_gap)
}

/// Left edge of the node in slot `slot` of a row of `count` nodes, centred in
/// the column.
fn slot_x(column: &ColumnInput, count: usize, slot: usize, p: &Params) -> f64 {
    let count = count.max(1) as f64;
    let row_width = count * p.node_w + (count - 1.0) * p.h_gap;
    column.left + (column.width - row_width) / 2.0 + slot as f64 * (p.node_w + p.h_gap)
}

/// One component's layout, before it is placed on the page: rows counted from
/// the component's own top.
struct LocalLayout {
    /// How many rows the component spans.
    rows: usize,
    /// `(card, row, x)` for every member.
    placements: Vec<(usize, usize, f64)>,
    /// Work units the component's search spent.
    work: u64,
}

/// One arrangement of a component: everything a layout round produces.
struct Arrangement {
    /// Per local node: its rank (logical row before wrapping).
    rank: Vec<usize>,
    /// Per local node: its row within the component.
    row: Vec<usize>,
    /// Per local node: its left edge.
    x: Vec<f64>,
    /// Per local node: whether its row in its column still has a free slot —
    /// the condition for nudging it aside.
    has_free_slot: Vec<bool>,
}

/// Steps 3–5 for one component of two or more cards.
///
/// `members` are global card indices; inside this function every node is
/// referred to by its position in `members` (a *local* index), which keeps
/// the per-node vectors small and dense.
fn layout_component(
    members: &[usize],
    edges: &[Edge],
    cards: &[Card],
    columns: &[ColumnInput],
    slots: &[usize],
    p: &Params,
    budget: u64,
) -> LocalLayout {
    let n = members.len();
    let local: HashMap<usize, usize> = members.iter().enumerate().map(|(l, &g)| (g, l)).collect();
    let col: Vec<usize> = members.iter().map(|&g| cards[g].col).collect();
    let text: Vec<usize> = members.iter().map(|&g| cards[g].text).collect();

    // Local edges with their rank weight: 1 inside a column (must read
    // down), 0 across columns (may run level). `mut` because a conflict round
    // may lengthen a link (raise its weight) to tilt its line past a card.
    let mut local_edges: Vec<(usize, usize, usize)> = edges
        .iter()
        .map(|e| {
            let (u, v) = (local[&e.from], local[&e.to]);
            (u, v, usize::from(col[u] == col[v]))
        })
        .collect();

    // Undirected neighbour lists, for the barycentre sweeps.
    let mut neighbours: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(u, v, _) in &local_edges {
        neighbours[u].push(v);
        neighbours[v].push(u);
    }

    // What the conflict rounds have learned so far: how often each link
    // (by its index in `local_edges`) has been lengthened, and which cards
    // have been nudged aside.
    let mut bumps = vec![0u8; local_edges.len()];
    let mut nudged = vec![false; n];

    let mut arrangement = arrange(
        &local_edges,
        &neighbours,
        &nudged,
        &col,
        &text,
        columns,
        slots,
        p,
    );
    // These rounds count against the component's budget too: each one tests
    // every line against every card (and arranges the component again), and
    // on a huge component they would otherwise run unbounded. They may use
    // a quarter of it; the search below gets the rest.
    let round_work = (local_edges.len() * n) as u64 * 2 + 1;
    let mut rounds_work = 0u64;
    for _ in 0..MAX_ROUNDS {
        if rounds_work + round_work > budget / 4 {
            break;
        }
        rounds_work += round_work;
        let conflicts = find_conflicts(&arrangement, &local_edges, p);
        if conflicts.is_empty() {
            break;
        }
        let mut changed = false;
        for (u, v, blocker) in conflicts {
            let vertical_in_blockers_column = col[u] == col[blocker] && col[v] == col[blocker];
            if vertical_in_blockers_column && !nudged[blocker] && arrangement.has_free_slot[blocker]
            {
                nudged[blocker] = true;
                changed = true;
                continue;
            }
            // Lengthen the link itself: its weight becomes the gap it spans
            // now plus one, so its successor must sit at least one rank
            // further below its predecessor than it does.
            //
            // This is recorded on the *link*, not as a floor on the
            // successor's rank. A floor does not survive `rank_nodes`: the
            // pull-down lifts the successor's predecessors down beside it and
            // the normalisation then shifts the whole component back up to
            // rank 0 — the same picture again. A weight is a constraint
            // *between* the two cards, which both of those steps keep.
            //
            // The links were de-duplicated in `layout`, so `(u, v)` names
            // exactly one of them.
            let Some(e) = local_edges.iter().position(|&(a, b, _)| a == u && b == v) else {
                continue;
            };
            if bumps[e] < MAX_BUMPS {
                let gap = arrangement.rank[v].saturating_sub(arrangement.rank[u]) + 1;
                local_edges[e].2 = local_edges[e].2.max(gap);
                bumps[e] += 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
        arrangement = arrange(
            &local_edges,
            &neighbours,
            &nudged,
            &col,
            &text,
            columns,
            slots,
            p,
        );
    }

    // Step 5b: polish the arrangement against what the user actually sees.
    let mut row = arrangement.row;
    let mut x = arrangement.x;
    let work = rounds_work
        + refine(
            &col,
            &local_edges,
            columns,
            slots,
            p,
            budget - rounds_work,
            &mut row,
            &mut x,
        );
    let rows = row.iter().copied().max().map_or(0, |r| r + 1);

    LocalLayout {
        rows,
        placements: (0..n).map(|l| (members[l], row[l], x[l])).collect(),
        work,
    }
}

// ── Step 5b: geometric local search ─────────────────────────────────────
//
// Ranking and barycentre ordering work on an abstract picture — ranks and
// slot order — and cannot see what the drawn lines will do. A chain with
// transitive links (122 → 121 → 120 → 119 plus 122 → 119) or a card fed from
// two columns away is exactly where that shows: lines through cards, and
// crossings a person can see how to avoid. So, once a component is placed,
// it is polished against a cost that *is* the picture:
//
//   cost = W_THROUGH × (lines passing through a card)
//        + W_CROSS   × (pairs of lines that cross)
//        + W_UP      × (links between columns that point up the page)
//        + W_ROW     × (rows the component spans)
//        + total line length in px
//
// The search moves one card at a time — to another row and/or slot of its
// own column, or by swapping places with another card of its column — and
// keeps a move only if it strictly lowers the cost. A same-column link must
// still read downward after every move (a hard rule, never traded off).
// Every candidate is tried in a fixed order and the number of candidates
// tried is capped, so the result is deterministic and the time bounded.

/// A line through a card is the worst thing the picture can show: it reads
/// as a link that is not there and hides one that is.
const W_THROUGH: f64 = 100_000.0;
/// A crossing costs as much as 1000px of extra line,
/// so a card will travel a long way to uncross two lines.
const W_CROSS: f64 = 10_000.0;
/// A link between columns that points up the page reads against the flow.
const W_UP: f64 = 300.0;
/// Each row the component spans: keeps the picture compact, so a card is
/// not parked in an empty row that a slightly longer line would avoid.
const W_ROW: f64 = 60.0;
/// How many rows above and below its current row a card of a big
/// component may be tried in (a small one's cards may try every row).
const MOVE_ROWS: usize = 2;
/// Components up to this many cards let a card try every row.
const FULL_REACH_MAX_NODES: usize = 40;
/// The most full passes over the component's cards in one greedy descent.
const MAX_PASSES: usize = 12;

/// The search's budget, in *work units*: one unit is one geometric test — a
/// line against a card, or two lines against each other — and the scans
/// that check a move is allowed count a unit per handful of cards or links
/// they look at (see [`Search::spend`]). Every phase draws on one counter
/// per component, checked before every candidate, so the search stops at
/// the budget whatever the board looks like, and stops at the same place on
/// every run (it counts work, not time).
///
/// Measured natively (release build, card #303 review) at 30–40 units per
/// microsecond: a dense 40-card, 60-link component spends its whole budget
/// in about 13–15 ms, and a 200-card, 300-link board lays out in about
/// 12–13 ms. WebAssembly in a browser is perhaps twice as slow. The budget
/// is the smallest that, with [`ANNEAL_PERCENT`], gets the user's
/// `manual-testing` board to no crossings at the minimum column width; the
/// tests hold the search to it in units, not time, so they are
/// deterministic.
pub const COMPONENT_WORK: u64 = 500_000;
/// The budget for every component on the board together. A board of many
/// tangled components gives the later ones less (they still get their
/// initial placement, which never has overlapping cards).
pub const LAYOUT_WORK: u64 = 1_500_000;
/// How a component's budget is shared out, in percent: the first greedy
/// descent, then the annealing runs together; whatever is left goes to the
/// final re-slotting and descent.
const GREEDY_PERCENT: u64 = 20;
const ANNEAL_PERCENT: u64 = 70;
/// Annealing temperature, in cost units: at the start a move that costs a
/// crossing is often taken; by the end only improvements are.
const ANNEAL_T_START: f64 = 3_000.0;
const ANNEAL_T_END: f64 = 5.0;
/// The most slot assignments [`slot_columns`] tries for one column (it also
/// stops at the budget).
const MAX_SLOT_COMBOS: usize = 4_096;
/// The whole-picture phases run only when this many whole-picture scores
/// fit in the component's budget.
const WHOLE_PICTURES: u64 = 20;
/// The most annealing runs (each with its own seed) a component gets.
const ANNEAL_MAX_RUNS: u64 = 6;

/// The x positions a node can take in a column with `k` slots per row: the
/// slots of a centred row of every length from 1 to `k` (for `k = 2`: left,
/// centre and right), plus hard against either side of the column (half a
/// gap in) when that is noticeably further out than the outermost slot. The
/// side positions are what let a one-slot column still offset a card from a
/// line running down its middle. Sorted, duplicates removed.
fn candidate_xs(column: &ColumnInput, k: usize, p: &Params) -> Vec<f64> {
    let mut xs: Vec<f64> = (1..=k)
        .flat_map(|count| (0..count).map(move |slot| (count, slot)))
        .map(|(count, slot)| slot_x(column, count, slot, p))
        .collect();
    let leftmost = slot_x(column, k, 0, p);
    let rightmost = slot_x(column, k, k - 1, p);
    let hard_left = column.left + p.h_gap / 2.0;
    let hard_right = column.left + column.width - p.h_gap / 2.0 - p.node_w;
    // Only when the column is wide enough to hold a node there at all.
    if hard_right >= hard_left {
        if leftmost - hard_left > p.h_gap {
            xs.push(hard_left);
        }
        if hard_right - rightmost > p.h_gap {
            xs.push(hard_right);
        }
    }
    xs.sort_by(f64::total_cmp);
    xs.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
    xs
}

/// Do segments `a1–a2` and `b1–b2` cross at a single interior point? Two
/// segments that merely touch, or overlap along a line, do not count (the
/// "through a card" term catches lines lying on top of each other, since
/// such lines run through a card at one end).
fn segments_cross(a1: (f64, f64), a2: (f64, f64), b1: (f64, f64), b2: (f64, f64)) -> bool {
    // The sign of a cross product says which side of a line a point is on.
    let side = |p: (f64, f64), q: (f64, f64), r: (f64, f64)| {
        (q.0 - p.0) * (r.1 - p.1) - (q.1 - p.1) * (r.0 - p.0)
    };
    let eps = 1e-9;
    let d1 = side(b1, b2, a1);
    let d2 = side(b1, b2, a2);
    let d3 = side(a1, a2, b1);
    let d4 = side(a1, a2, b2);
    d1 * d2 < -eps && d3 * d4 < -eps
}

/// The current geometry of one component during the search.
struct Search<'a> {
    col: &'a [usize],
    edges: &'a [(usize, usize, usize)],
    p: &'a Params,
    row: Vec<usize>,
    x: Vec<f64>,
    /// Work units spent so far. A `Cell` because the cost functions take
    /// `&self` (they only read the picture) but must still count what they
    /// do; `Cell` gives that one counter interior mutability, and `u64`
    /// is `Copy`, so `get`/`set` need no borrow.
    work: std::cell::Cell<u64>,
    /// Where the current phase must stop ([`Self::exhausted`]).
    limit: u64,
}

impl Search<'_> {
    /// Count `units` of work.
    fn spend(&self, units: u64) {
        self.work.set(self.work.get() + units);
    }

    /// Has the current phase used up its budget?
    fn exhausted(&self) -> bool {
        self.work.get() >= self.limit
    }

    fn rect(&self, l: usize) -> Rect {
        Rect {
            x: self.x[l],
            y: row_y(self.row[l], self.p),
            w: self.p.node_w,
            h: self.p.node_h,
        }
    }

    /// The drawn segment of edge `e`.
    fn segment(&self, e: usize) -> ((f64, f64), (f64, f64)) {
        let (u, v, _) = self.edges[e];
        clipped_segment(&self.rect(u), &self.rect(v))
    }

    /// What edge `e`'s line costs for passing through card `l` (not one of
    /// its ends): nothing if it misses, else [`W_THROUGH`].
    fn through_cost(&self, e: usize, l: usize) -> f64 {
        let (u, v, _) = self.edges[e];
        if l == u || l == v {
            return 0.0;
        }
        self.spend(1);
        let (a, b) = self.segment(e);
        if segment_hits_rect(a, b, &self.rect(l).inflate(CLEARANCE)) {
            W_THROUGH
        } else {
            0.0
        }
    }

    /// Do edges `e` and `f` cross? Edges sharing a card meet at it, which
    /// is not a crossing.
    fn cross(&self, e: usize, f: usize) -> bool {
        let (u1, v1, _) = self.edges[e];
        let (u2, v2, _) = self.edges[f];
        if u1 == u2 || u1 == v2 || v1 == u2 || v1 == v2 {
            return false;
        }
        self.spend(1);
        let (a1, a2) = self.segment(e);
        let (b1, b2) = self.segment(f);
        segments_cross(a1, a2, b1, b2)
    }

    /// Edge `e`'s own cost: its length, an upward penalty, and every card
    /// its line runs through.
    fn edge_cost(&self, e: usize) -> f64 {
        let (u, v, _) = self.edges[e];
        let (a, b) = self.segment(e);
        let mut cost = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
        if self.col[u] != self.col[v] && self.row[v] < self.row[u] {
            cost += W_UP;
        }
        cost + (0..self.row.len())
            .map(|l| self.through_cost(e, l))
            .sum::<f64>()
    }

    /// The part of the total cost that moving the cards in `moved` can
    /// change: the edges touching them (whole), the other edges' lines
    /// through them, and every crossing involving a touching edge.
    fn local_cost(&self, moved: &[usize]) -> f64 {
        let touches = |e: usize| {
            let (u, v, _) = self.edges[e];
            moved.contains(&u) || moved.contains(&v)
        };
        // The row count can change with any move, so it is always part of
        // the changeable cost.
        let mut cost = self.rows_cost();
        for e in 0..self.edges.len() {
            if touches(e) {
                cost += self.edge_cost(e);
                for f in 0..self.edges.len() {
                    // Each crossing between two touching edges once.
                    if f != e && (!touches(f) || f > e) && self.cross(e, f) {
                        cost += W_CROSS;
                    }
                }
            } else {
                cost += moved.iter().map(|&l| self.through_cost(e, l)).sum::<f64>();
            }
        }
        cost
    }

    /// [`W_ROW`] for every row from the top of the component (row 0) to its
    /// lowest card.
    fn rows_cost(&self) -> f64 {
        let rows = self.row.iter().copied().max().map_or(0, |r| r + 1);
        W_ROW * rows as f64
    }

    /// `(lines through cards, crossings)` in the current picture.
    fn faults(&self) -> (usize, usize) {
        let m = self.edges.len();
        let through = (0..m)
            .map(|e| {
                (0..self.row.len())
                    .filter(|&l| self.through_cost(e, l) > 0.0)
                    .count()
            })
            .sum();
        let crossings = (0..m)
            .map(|e| (e + 1..m).filter(|&f| self.cross(e, f)).count())
            .sum();
        (through, crossings)
    }

    /// The whole picture's cost.
    fn total_cost(&self) -> f64 {
        let mut cost = self.rows_cost();
        for e in 0..self.edges.len() {
            cost += self.edge_cost(e);
            for f in e + 1..self.edges.len() {
                if self.cross(e, f) {
                    cost += W_CROSS;
                }
            }
        }
        cost
    }

    /// Would card `l` at `(row, x)` overlap another card of its column
    /// (other than those in `ignore`, which are moving too)?
    fn overlaps(&self, l: usize, row: usize, x: f64, ignore: &[usize]) -> bool {
        // A scan of every card: cheap comparisons, a unit per eight.
        self.spend(1 + self.row.len() as u64 / 8);
        let min_gap = self.p.node_w + self.p.h_gap - 1e-6;
        (0..self.row.len()).any(|o| {
            o != l
                && !ignore.contains(&o)
                && self.col[o] == self.col[l]
                && self.row[o] == row
                && (self.x[o] - x).abs() < min_gap
        })
    }

    /// Does every same-column link touching `l` still read downward?
    fn order_holds(&self, l: usize) -> bool {
        self.spend(1 + self.edges.len() as u64 / 8);
        self.edges.iter().all(|&(u, v, _)| {
            !(u == l || v == l) || self.col[u] != self.col[v] || self.row[v] > self.row[u]
        })
    }
}

/// Step 5b for one component: polish `row` and `x` in place (see above),
/// spending at most `budget` work units. Returns the units spent.
#[allow(clippy::too_many_arguments)]
fn refine(
    col: &[usize],
    edges: &[(usize, usize, usize)],
    columns: &[ColumnInput],
    slots: &[usize],
    p: &Params,
    budget: u64,
    row: &mut Vec<usize>,
    x: &mut Vec<f64>,
) -> u64 {
    let n = col.len();
    if edges.is_empty() || n < 2 || budget == 0 {
        return 0;
    }
    // Each column's candidate x positions, computed once.
    let xs: HashMap<usize, Vec<f64>> = col
        .iter()
        .map(|&c| (c, candidate_xs(&columns[c], slots[c], p)))
        .collect();
    // `std::mem::take` moves the vectors into the search (leaving empty ones
    // behind) so it can own and mutate them; they are moved back at the end.
    let mut s = Search {
        col,
        edges,
        p,
        row: std::mem::take(row),
        x: std::mem::take(x),
        work: std::cell::Cell::new(0),
        limit: budget * GREEDY_PERCENT / 100,
    };

    // Greedy first, on a share of the budget: cheap, and on most components
    // all that is needed.
    greedy(&mut s, &xs);

    // The phases below score whole pictures, and one such score tests every
    // line against every card and every other line. On a component so big
    // that a handful of those would eat the budget, greedy (which scores
    // moves locally) is all it gets.
    let m = edges.len() as u64;
    let whole_picture = m * (n as u64 + m);
    if whole_picture * WHOLE_PICTURES > budget {
        s.limit = budget;
        greedy(&mut s, &xs);
        return finish(s, row, x);
    }

    // Then anneal: greedy stops at the first arrangement no single move
    // improves, and chains of transitive links routinely need two or three
    // cards moved together. Each run starts from the greedy result with its
    // own fixed seed, and the cheapest outcome wins — one run can settle in
    // a different valley from another. Each run gets an equal share of what
    // is left, so the last run ends at the budget. Effort follows need:
    // another run is made only while the best picture so far still has a
    // line through a card or a crossing, so a clean component stops early.
    let (start_row, start_x) = (s.row.clone(), s.x.clone());
    let mut best = (s.total_cost(), start_row.clone(), start_x.clone());
    let anneal_end = s.work.get() + budget * ANNEAL_PERCENT / 100;
    for run in 0..ANNEAL_MAX_RUNS {
        if run > 0 {
            let (run_row, run_x) = (s.row.clone(), s.x.clone());
            s.row.clone_from(&best.1);
            s.x.clone_from(&best.2);
            let clean = s.faults() == (0, 0);
            s.row = run_row;
            s.x = run_x;
            if clean {
                break;
            }
        }
        let spent = s.work.get();
        if spent >= anneal_end {
            break;
        }
        s.limit = spent + (anneal_end - spent) / (ANNEAL_MAX_RUNS - run);
        s.row.clone_from(&start_row);
        s.x.clone_from(&start_x);
        anneal(&mut s, &xs, run);
        greedy(&mut s, &xs);
        let cost = s.total_cost();
        if cost < best.0 - 1e-6 {
            best = (cost, s.row.clone(), s.x.clone());
        }
    }
    s.row = best.1;
    s.x = best.2;

    // Last, on the best picture, the rest of the budget: re-slot each
    // column exhaustively (the move the other phases cannot make), then
    // descend once more from wherever that lands.
    if s.faults() != (0, 0) {
        s.limit = budget;
        slot_columns(&mut s, &xs);
        greedy(&mut s, &xs);
    }

    finish(s, row, x)
}

/// Hand a finished search's picture back through `row` and `x`, and return
/// the work it spent.
///
/// The component is first shifted up so its first card is on row 0. Moving
/// every card the same distance changes no line's shape, so this is always
/// safe — and no single move in the search can do it, which is how empty
/// rows at the top survive it. Empty rows further down are left alone:
/// closing one changes the slope of every line across it, and the search
/// (which pays per row) kept it because closing it costs more.
fn finish(mut s: Search, row: &mut Vec<usize>, x: &mut Vec<f64>) -> u64 {
    let top = s.row.iter().copied().min().unwrap_or(0);
    for r in &mut s.row {
        *r -= top;
    }
    let spent = s.work.get();
    *row = s.row;
    *x = s.x;
    spent
}

/// Greedy descent: for each card in turn, make the single relocation or
/// swap that lowers the cost most, until a full pass improves nothing (or
/// the phase's budget runs out).
fn greedy(s: &mut Search, xs: &HashMap<usize, Vec<f64>>) {
    let n = s.row.len();
    'passes: for _ in 0..MAX_PASSES {
        let mut improved = false;
        for l in 0..n {
            // The best move found for this card: (gain, kind).
            let mut best: Option<(f64, Move)> = None;
            let before = s.local_cost(&[l]);
            let (home_row, home_x) = (s.row[l], s.x[l]);
            let max_row = s.row.iter().copied().max().unwrap_or(0) + 1;
            // Every row of a small component; nearby rows of a big one.
            let reach = if n <= FULL_REACH_MAX_NODES {
                max_row
            } else {
                MOVE_ROWS
            };

            // Relocations within its own column.
            for r in home_row.saturating_sub(reach)..=(home_row + reach).min(max_row) {
                for &cx in &xs[&s.col[l]] {
                    if r == home_row && (cx - home_x).abs() < 1e-6 {
                        continue;
                    }
                    if s.overlaps(l, r, cx, &[]) {
                        continue;
                    }
                    if s.exhausted() {
                        break 'passes;
                    }
                    s.row[l] = r;
                    s.x[l] = cx;
                    if s.order_holds(l) {
                        let gain = before - s.local_cost(&[l]);
                        if gain > 1e-6 && best.is_none_or(|(g, _)| gain > g) {
                            best = Some((gain, Move::To(r, cx)));
                        }
                    }
                    s.row[l] = home_row;
                    s.x[l] = home_x;
                }
            }

            // Swaps with another card of its column.
            for o in 0..n {
                if o == l || s.col[o] != s.col[l] {
                    continue;
                }
                if s.exhausted() {
                    break 'passes;
                }
                let pair_before = s.local_cost(&[l, o]);
                swap(s, l, o);
                let fits = !s.overlaps(l, s.row[l], s.x[l], &[o])
                    && !s.overlaps(o, s.row[o], s.x[o], &[l])
                    && s.order_holds(l)
                    && s.order_holds(o);
                if fits {
                    let gain = pair_before - s.local_cost(&[l, o]);
                    if gain > 1e-6 && best.is_none_or(|(g, _)| gain > g) {
                        best = Some((gain, Move::Swap(o)));
                    }
                }
                swap(s, l, o);
            }

            match best {
                Some((_, Move::To(r, cx))) => {
                    s.row[l] = r;
                    s.x[l] = cx;
                    improved = true;
                }
                Some((_, Move::Swap(o))) => {
                    swap(s, l, o);
                    improved = true;
                }
                None => {}
            }
        }
        if !improved {
            break;
        }
    }
}

/// For each column of the component, try every way of putting its cards in
/// that column's x positions — rows unchanged — and keep the cheapest
/// (if it beats what is there). A chain with transitive links
/// (122 → 121 → 120 → 119 plus 122 → 119 and 121 → 119) needs its cards
/// zig-zagged across the column so every line has a clear corridor, which
/// means moving several cards at once — a move neither [`greedy`] nor
/// [`anneal`] makes. Skipped for a column with more combinations than
/// [`MAX_SLOT_COMBOS`]; stops at the phase's budget.
fn slot_columns(s: &mut Search, xs: &HashMap<usize, Vec<f64>>) {
    let n = s.row.len();
    let mut columns: Vec<usize> = s.col.to_vec();
    columns.sort_unstable();
    columns.dedup();
    for c in columns {
        let cards: Vec<usize> = (0..n).filter(|&l| s.col[l] == c).collect();
        let options = &xs[&c];
        // `checked_pow` guards the count itself against overflow.
        let combos = u32::try_from(cards.len())
            .ok()
            .and_then(|k| options.len().checked_pow(k));
        let Some(combos) = combos.filter(|&k| k <= MAX_SLOT_COMBOS) else {
            continue;
        };
        let original: Vec<f64> = cards.iter().map(|&l| s.x[l]).collect();
        let mut best = (s.total_cost(), original.clone());
        for combo in 0..combos {
            if s.exhausted() {
                break;
            }
            // Read `combo` as a number in base `options.len()`: digit i is
            // the option for card i.
            let mut rest = combo;
            for &l in &cards {
                s.x[l] = options[rest % options.len()];
                rest /= options.len();
            }
            let fits = cards.iter().all(|&l| !s.overlaps(l, s.row[l], s.x[l], &[]));
            if fits {
                let cost = s.total_cost();
                if cost < best.0 - 1e-6 {
                    best = (cost, cards.iter().map(|&l| s.x[l]).collect());
                }
            }
        }
        for (&l, &x) in cards.iter().zip(&best.1) {
            s.x[l] = x;
        }
    }
}

/// A tiny deterministic pseudo-random generator (a 64-bit linear
/// congruential generator with Knuth's MMIX constants). The search must give
/// the same picture on every run and every machine, so it never touches a
/// real source of randomness — the "random" moves are a fixed sequence.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    /// A number in `0..n` (`n > 0`).
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// A number in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next() % 1_000_000) as f64 / 1_000_000.0
    }
}

/// Simulated annealing over the same moves as [`greedy`]: a random card is
/// relocated or swapped, and the move is kept if it lowers the cost — or,
/// with a probability that shrinks as the "temperature" falls, even if it
/// raises it. Taking the occasional worse move is what lets the search climb
/// out of an arrangement no single move improves. The best arrangement seen
/// is what the search ends with.
fn anneal(s: &mut Search, xs: &HashMap<usize, Vec<f64>>, run: u64) {
    let n = s.row.len();
    let e = s.edges.len();
    let mut rng = Lcg(0x5eed_6a7a + n as u64 * 31 + e as u64 + run * 7919);
    // Roughly what one step costs (two local costs over a card's links), to
    // turn the run's share of the budget into a number of steps; the
    // temperature schedule needs to know it. The loop also stops at the
    // budget, should the estimate be low.
    let degree = (2 * e).div_ceil(n).max(1) as u64;
    let per_step = 2 * (degree * (n + e) as u64 + e as u64) + 4;
    let share = s.limit.saturating_sub(s.work.get());
    let steps = (share / per_step).max(1);
    // Each step multiplies the temperature by `cool`, so it falls
    // geometrically from the start value to the end value.
    let cool = (ANNEAL_T_END / ANNEAL_T_START).powf(1.0 / steps as f64);
    let mut temperature = ANNEAL_T_START;

    // The best starts as where the search begins, so annealing can never
    // hand back something worse.
    let mut cost = s.total_cost();
    let (mut best_cost, mut best_row, mut best_x) = (cost, s.row.clone(), s.x.clone());

    for _ in 0..steps {
        if s.exhausted() {
            break;
        }
        temperature *= cool;
        let l = rng.below(n);
        let (home_row, home_x) = (s.row[l], s.x[l]);
        let max_row = s.row.iter().copied().max().unwrap_or(0) + 1;

        // Propose: a relocation (three times in four) or a swap.
        let partner = if rng.below(4) == 0 {
            let same: Vec<usize> = (0..n).filter(|&o| o != l && s.col[o] == s.col[l]).collect();
            if same.is_empty() {
                None
            } else {
                Some(same[rng.below(same.len())])
            }
        } else {
            None
        };
        let delta = match partner {
            Some(o) => {
                let before = s.local_cost(&[l, o]);
                swap(s, l, o);
                let fits = !s.overlaps(l, s.row[l], s.x[l], &[o])
                    && !s.overlaps(o, s.row[o], s.x[o], &[l])
                    && s.order_holds(l)
                    && s.order_holds(o);
                if !fits {
                    swap(s, l, o);
                    continue;
                }
                s.local_cost(&[l, o]) - before
            }
            None => {
                let column_xs = &xs[&s.col[l]];
                let r = rng.below(max_row + 1);
                let cx = column_xs[rng.below(column_xs.len())];
                if (r == home_row && (cx - home_x).abs() < 1e-6) || s.overlaps(l, r, cx, &[]) {
                    continue;
                }
                let before = s.local_cost(&[l]);
                s.row[l] = r;
                s.x[l] = cx;
                if !s.order_holds(l) {
                    s.row[l] = home_row;
                    s.x[l] = home_x;
                    continue;
                }
                s.local_cost(&[l]) - before
            }
        };

        // Metropolis rule: always keep an improvement; keep a worsening
        // with probability e^(−delta / temperature).
        let keep = delta < 0.0 || rng.unit() < (-delta / temperature).exp();
        if keep {
            cost += delta;
            if cost < best_cost - 1e-6 {
                best_cost = cost;
                best_row.clone_from(&s.row);
                best_x.clone_from(&s.x);
            }
        } else {
            match partner {
                Some(o) => swap(s, l, o),
                None => {
                    s.row[l] = home_row;
                    s.x[l] = home_x;
                }
            }
        }
    }

    s.row = best_row;
    s.x = best_x;
}

/// A candidate move in [`refine`].
#[derive(Clone, Copy)]
enum Move {
    /// To this row and x in its own column.
    To(usize, f64),
    /// Trade places with this card.
    Swap(usize),
}

/// Exchange two cards' places (row and x).
fn swap(s: &mut Search, a: usize, b: usize) {
    s.row.swap(a, b);
    s.x.swap(a, b);
}

/// Every `(predecessor, successor, blocker)` where the line from predecessor
/// to successor passes through (or within [`CLEARANCE`] of) a third node, in
/// edge order, at most one blocker per line.
fn find_conflicts(
    arrangement: &Arrangement,
    edges: &[(usize, usize, usize)],
    p: &Params,
) -> Vec<(usize, usize, usize)> {
    let rects: Vec<Rect> = (0..arrangement.x.len())
        .map(|l| Rect {
            x: arrangement.x[l],
            y: row_y(arrangement.row[l], p),
            w: p.node_w,
            h: p.node_h,
        })
        .collect();
    let mut conflicts = Vec::new();
    for &(u, v, _) in edges {
        let (a, b) = clipped_segment(&rects[u], &rects[v]);
        let blocker = (0..rects.len())
            .filter(|&other| other != u && other != v)
            .find(|&other| segment_hits_rect(a, b, &rects[other].inflate(CLEARANCE)));
        if let Some(blocker) = blocker {
            conflicts.push((u, v, blocker));
        }
    }
    conflicts
}

/// Steps 3 and 4 for one component: rank, group, order and position.
#[allow(clippy::too_many_arguments)]
fn arrange(
    edges: &[(usize, usize, usize)],
    neighbours: &[Vec<usize>],
    nudged: &[bool],
    col: &[usize],
    text: &[usize],
    columns: &[ColumnInput],
    slots: &[usize],
    p: &Params,
) -> Arrangement {
    let n = col.len();
    let rank = rank_nodes(n, edges);

    // Group by (rank, column). Each group starts in text-mode order; a
    // `BTreeMap` iterates groups in rank order, which `position` relies on.
    let mut groups: BTreeMap<(usize, usize), Vec<usize>> = BTreeMap::new();
    for l in 0..n {
        groups.entry((rank[l], col[l])).or_default().push(l);
    }
    for group in groups.values_mut() {
        group.sort_by_key(|&l| text[l]);
    }

    // Barycentre sweeps: sort each group by the mean centre-x of each
    // node's neighbours (a node with none keeps its own x as its key), ties
    // broken by text-mode order. `total_cmp` gives floats a total order so
    // the sort never panics on a NaN.
    let mut placement = position(&groups, nudged, columns, slots, n, p);
    for _ in 0..SWEEPS {
        let centre = |l: usize, placement: &Placement| placement.x[l] + p.node_w / 2.0;
        let key: Vec<f64> = (0..n)
            .map(|l| {
                if neighbours[l].is_empty() {
                    centre(l, &placement)
                } else {
                    let sum: f64 = neighbours[l].iter().map(|&m| centre(m, &placement)).sum();
                    sum / neighbours[l].len() as f64
                }
            })
            .collect();
        for group in groups.values_mut() {
            group.sort_by(|&a, &b| key[a].total_cmp(&key[b]).then(text[a].cmp(&text[b])));
        }
        placement = position(&groups, nudged, columns, slots, n, p);
    }

    Arrangement {
        rank,
        row: placement.row,
        x: placement.x,
        has_free_slot: placement.has_free_slot,
    }
}

/// The geometric half of an arrangement.
struct Placement {
    row: Vec<usize>,
    x: Vec<f64>,
    has_free_slot: Vec<bool>,
}

/// Turn ordered (rank, column) groups into rows and x positions.
///
/// A rank is as many rows tall as its most crowded column needs: a group of
/// `len` nodes in a column with `K` slots wraps into `ceil(len / K)` rows.
/// Each row of a group is centred in its column — unless it holds a nudged
/// node and has a free slot, in which case it is left-aligned so the column's
/// centre line is clear.
fn position(
    groups: &BTreeMap<(usize, usize), Vec<usize>>,
    nudged: &[bool],
    columns: &[ColumnInput],
    slots: &[usize],
    n: usize,
    p: &Params,
) -> Placement {
    // Rows each rank needs.
    let mut rank_rows: BTreeMap<usize, usize> = BTreeMap::new();
    for (&(rank, column), group) in groups {
        let need = group.len().div_ceil(slots[column]);
        let entry = rank_rows.entry(rank).or_insert(0);
        *entry = (*entry).max(need);
    }
    // First row of each rank: the running total of the ranks above it. Ranks
    // with no nodes simply do not appear, so they take no space.
    let mut rank_base: BTreeMap<usize, usize> = BTreeMap::new();
    let mut total = 0;
    for (&rank, &rows) in &rank_rows {
        rank_base.insert(rank, total);
        total += rows;
    }

    let mut row = vec![0usize; n];
    let mut x = vec![0.0f64; n];
    let mut has_free_slot = vec![false; n];
    for (&(rank, column), group) in groups {
        let k = slots[column];
        // `chunks(k)` splits the group into its wrapped rows of at most `k`.
        for (sub, chunk) in group.chunks(k).enumerate() {
            let count = chunk.len();
            let free = count < k;
            let left_align = free && chunk.iter().any(|&l| nudged[l]);
            for (slot, &l) in chunk.iter().enumerate() {
                row[l] = rank_base[&rank] + sub;
                // Left-aligned is "centred as if the row were full".
                x[l] = slot_x(
                    &columns[column],
                    if left_align { k } else { count },
                    slot,
                    p,
                );
                has_free_slot[l] = free;
            }
        }
    }
    Placement {
        row,
        x,
        has_free_slot,
    }
}

/// Step 3: rank every node of a component.
///
/// `edges` are `(predecessor, successor, weight)`; the result satisfies
/// `rank[successor] >= rank[predecessor] + weight` for every link, and the
/// lowest rank is 0, provided the links form no cycle (the server refuses
/// cycles; rows written before it did are still laid out, just without the
/// guarantee). Every constraint is *relative* — between two cards — which is
/// why the conflict rounds lengthen links rather than set a floor on a rank:
/// the pull-down and the normalisation below keep every gap but move ranks.
fn rank_nodes(n: usize, edges: &[(usize, usize, usize)]) -> Vec<usize> {
    let mut out: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];
    let mut indegree = vec![0usize; n];
    for &(u, v, w) in edges {
        out[u].push((v, w));
        indegree[v] += 1;
    }

    // Topological order (Kahn's algorithm). The ready set is a `BTreeSet`
    // so the lowest index is always taken next, which keeps the result
    // deterministic.
    let mut order = Vec::with_capacity(n);
    let mut ready: BTreeSet<usize> = (0..n).filter(|&l| indegree[l] == 0).collect();
    while let Some(l) = ready.pop_first() {
        order.push(l);
        for &(v, _) in &out[l] {
            indegree[v] -= 1;
            if indegree[v] == 0 {
                ready.insert(v);
            }
        }
    }
    // Anything left is on a cycle; append it so it is still ranked.
    if order.len() < n {
        let in_order: HashSet<usize> = order.iter().copied().collect();
        order.extend((0..n).filter(|l| !in_order.contains(l)));
    }

    // Longest path: walking in topological order, every predecessor's rank
    // is final before its successors are visited.
    let mut rank = vec![0usize; n];
    for &u in &order {
        for &(v, w) in &out[u] {
            rank[v] = rank[v].max(rank[u] + w);
        }
    }

    // Pull down: walking backwards, raise each node with successors to the
    // highest rank they all still allow. Raising only ever keeps
    // `rank[u] >= rank[pred] + w`, and the minimum over successors keeps
    // `rank[succ] >= rank[u] + w`, so every constraint still holds.
    for &u in order.iter().rev() {
        if let Some(allowed) = out[u].iter().map(|&(v, w)| rank[v].saturating_sub(w)).min()
            && allowed > rank[u]
        {
            rank[u] = allowed;
        }
    }

    // Start at rank 0 (the pull-down can lift every node off it).
    let lowest = rank.iter().copied().min().unwrap_or(0);
    rank.iter().map(|r| r - lowest).collect()
}

/// The segment between the centres of `a` and `b`, trimmed to start on `a`'s
/// border and end on `b`'s.
fn clipped_segment(a: &Rect, b: &Rect) -> ((f64, f64), (f64, f64)) {
    let (ca, cb) = (a.center(), b.center());
    (border_point(a, cb), border_point(b, ca))
}

/// Where the ray from the centre of `rect` towards `toward` leaves `rect`.
fn border_point(rect: &Rect, toward: (f64, f64)) -> (f64, f64) {
    let (cx, cy) = rect.center();
    let (dx, dy) = (toward.0 - cx, toward.1 - cy);
    // How far along the ray (as a fraction of its length) each pair of sides
    // is reached; the nearer one is where the ray exits.
    let tx = if dx == 0.0 {
        f64::INFINITY
    } else {
        (rect.w / 2.0) / dx.abs()
    };
    let ty = if dy == 0.0 {
        f64::INFINITY
    } else {
        (rect.h / 2.0) / dy.abs()
    };
    // Capped at 1 so a target inside the rectangle (overlapping nodes, which
    // the layout never produces) yields the target, not a point past it.
    let t = tx.min(ty).min(1.0);
    (cx + t * dx, cy + t * dy)
}

/// Whether the segment `a`–`b` touches `rect` (Liang–Barsky clipping: narrow
/// the segment's parameter range `[t0, t1]` against each side in turn; if it
/// is still non-empty at the end, part of the segment lies inside).
fn segment_hits_rect(a: (f64, f64), b: (f64, f64), rect: &Rect) -> bool {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let mut t0 = 0.0f64;
    let mut t1 = 1.0f64;
    for (pv, qv) in [
        (-dx, a.0 - rect.x),
        (dx, rect.right() - a.0),
        (-dy, a.1 - rect.y),
        (dy, rect.bottom() - a.1),
    ] {
        if pv == 0.0 {
            // Parallel to this side: outside it means no intersection at all.
            if qv < 0.0 {
                return false;
            }
        } else {
            let t = qv / pv;
            if pv < 0.0 {
                if t > t1 {
                    return false;
                }
                t0 = t0.max(t);
            } else {
                if t < t0 {
                    return false;
                }
                t1 = t1.min(t);
            }
        }
    }
    t0 <= t1
}

/// Disjoint-set forest over card indices, for step 2.
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    /// The set's representative, halving the path on the way up so repeated
    /// lookups stay cheap.
    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            // The lower index becomes the root, for determinism.
            let (low, high) = if ra < rb { (ra, rb) } else { (rb, ra) };
            self.parent[high] = low;
        }
    }
}

#[cfg(test)]
mod tests;
