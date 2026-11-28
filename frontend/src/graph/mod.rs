//! The board link graph (card #303): a second way of looking at a board, in
//! which every card is a small node in its own column and every link is a
//! straight arrow from the card that comes first to the card that comes after.
//!
//! Graph mode is a *mode of the board page*, not a page of its own: the board
//! route `/boards/:slug` is a parent route with two children, `""` (text mode)
//! and `"graph"`, so `BoardView` — which owns the columns, the cards, the
//! links and the event stream — stays mounted when the user flips between
//! them. Nothing is refetched and the stream is not reconnected.
//!
//! - [`layout`] works out where everything goes (pure Rust, host-tested).
//! - [`view`] measures the column headers, feeds the layout, and draws it.
//! - [`BoardMode`] is which of the two modes the URL asks for.

pub mod layout;
pub mod view;

use leptos::prelude::*;

/// Which way the board page shows the board.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoardMode {
    /// The ordinary board: columns of cards.
    Text,
    /// The link graph.
    Graph,
}

impl BoardMode {
    /// The mode a board URL's path asks for: graph mode is
    /// `/boards/:slug/graph`, everything else is text mode. A trailing slash
    /// is tolerated, since the router matches `/boards/x/graph/` too.
    pub fn from_path(path: &str) -> Self {
        // By segment, not by suffix: `/boards/graph` is text mode on a board
        // that happens to be called "graph". Empty segments (a leading or
        // trailing slash) are skipped.
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        match segments.as_slice() {
            ["boards", _slug, "graph"] => Self::Graph,
            _ => Self::Text,
        }
    }

    /// The board's own URL in this mode — where closing a card's modal
    /// returns to.
    pub fn board_path(self, slug: &str) -> String {
        match self {
            Self::Text => format!("/boards/{slug}"),
            Self::Graph => format!("/boards/{slug}/graph"),
        }
    }

    /// The URL that opens card `number` over board `slug` in this mode, so
    /// opening a card from the graph keeps the graph behind it.
    pub fn card_path(self, slug: &str, number: u32) -> String {
        format!("{}?card={number}", self.board_path(slug))
    }

    /// The other mode — what the navbar switch flips to.
    pub fn toggled(self) -> Self {
        match self {
            Self::Text => Self::Graph,
            Self::Graph => Self::Text,
        }
    }
}

/// The current mode, provided as a context by `BoardView`. A `Memo` over the
/// router's path, so readers are only woken when the mode actually changes —
/// not on every `?card=` change.
#[derive(Clone, Copy)]
pub struct BoardModeContext(pub Memo<BoardMode>);

impl BoardModeContext {
    /// The mode right now, untracked, for click handlers. `try_` because a
    /// handler can outlive the board view that provided the memo; text mode
    /// is the safe answer then.
    pub fn now(self) -> BoardMode {
        self.0.try_get_untracked().unwrap_or(BoardMode::Text)
    }
}

#[cfg(test)]
mod tests {
    use super::BoardMode;

    #[test]
    fn the_graph_path_is_graph_mode() {
        assert_eq!(BoardMode::from_path("/boards/work/graph"), BoardMode::Graph);
        assert_eq!(
            BoardMode::from_path("/boards/work/graph/"),
            BoardMode::Graph
        );
    }

    #[test]
    fn the_plain_board_path_is_text_mode() {
        assert_eq!(BoardMode::from_path("/boards/work"), BoardMode::Text);
        assert_eq!(BoardMode::from_path("/boards/work/"), BoardMode::Text);
        // A board whose slug merely ends in "graph" is not graph mode.
        assert_eq!(BoardMode::from_path("/boards/graph"), BoardMode::Text);
        assert_eq!(BoardMode::from_path("/boards/my-graph"), BoardMode::Text);
    }

    #[test]
    fn each_mode_has_its_own_board_and_card_paths() {
        assert_eq!(BoardMode::Text.board_path("work"), "/boards/work");
        assert_eq!(BoardMode::Graph.board_path("work"), "/boards/work/graph");
        assert_eq!(BoardMode::Text.card_path("work", 7), "/boards/work?card=7");
        assert_eq!(
            BoardMode::Graph.card_path("work", 7),
            "/boards/work/graph?card=7"
        );
    }

    #[test]
    fn toggling_flips_between_the_two_modes() {
        assert_eq!(BoardMode::Text.toggled(), BoardMode::Graph);
        assert_eq!(BoardMode::Graph.toggled(), BoardMode::Text);
    }
}
