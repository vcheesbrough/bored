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
//!    blocker. What the bound cannot clear is drawn as it is — crossings are
//!    minimised, not guaranteed away.
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
        return Layout::default();
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
        let local = layout_component(members, &member_edges, &cards, columns, &slots, p);

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

    Layout {
        nodes,
        edges: edge_lines,
        height,
    }
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
}

/// One arrangement of a component: everything a layout round produces.
struct Arrangement {
    rows: usize,
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
    for _ in 0..MAX_ROUNDS {
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

    LocalLayout {
        rows: arrangement.rows,
        placements: (0..n)
            .map(|l| (members[l], arrangement.row[l], arrangement.x[l]))
            .collect(),
    }
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
        rows: placement.rows,
        rank,
        row: placement.row,
        x: placement.x,
        has_free_slot: placement.has_free_slot,
    }
}

/// The geometric half of an arrangement.
struct Placement {
    rows: usize,
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
        rows: total,
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
