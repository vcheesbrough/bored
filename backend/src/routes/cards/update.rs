//! What a `PUT /api/cards/:id` actually changes.
//!
//! The handler in the parent module does the I/O — load the card, check the
//! target column, run the `UPDATE`, write the audit row, broadcast the event.
//! Sitting between the load and the write is a decision that needs no database
//! at all: *given this request and this stored card, what changes?*
//!
//! That decision is this file. It is the fiddly part of updating a card —
//! a request can be a no-op, it can be a layout move, it can be a content edit,
//! and which of those it is decides how the change appears in the history
//! drawer. Pulling it out of the handler makes it directly unit-testable
//! (see the tests at the bottom), which it was not while it lived inline
//! among the `await`s.
//!
//! Nothing here touches [`crate::models`] beyond reading the stored tag list,
//! and nothing here is `async`.

use axum::http::StatusCode;
use surrealdb::{engine::local::Db, method::Query};

use crate::models::DbCard;

use super::normalize_tags;

/// The `UPDATE` statement a request resolves to: which fields to write, and
/// the values to bind to them.
///
/// `set_parts` and the value fields are kept in step — a field is `Some` here
/// exactly when `set_parts` carries the matching `$placeholder` — so
/// [`Self::statement`] and [`Self::bind`] can never disagree about how many
/// parameters the query has.
#[derive(Debug)]
pub(super) struct CardWrite {
    /// SQL `SET` fragments, e.g. `"body = $body"`, joined by [`Self::statement`].
    set_parts: Vec<String>,
    body: Option<String>,
    column_id: Option<String>,
    position: Option<i32>,
    /// The normalized replacement tag list — `Some` **only** when it actually
    /// differs from the card's current tags. Re-sending the tags a card already
    /// has must not be written, or it would manufacture an audit row for an
    /// edit that did not happen.
    tags: Option<Vec<String>>,
}

/// How a write is recorded in the audit log.
#[derive(Debug)]
pub(super) struct CardAudit {
    /// `"move"` or `"update"` — see [`CardUpdate::plan`] for which is which.
    pub(super) action: &'static str,
    /// The client's edit-session token, when this row is allowed to merge into
    /// an in-flight editing stretch. `None` means "give this its own row".
    pub(super) edit_session: Option<String>,
}

/// A planned card update: the write to perform and how to record it.
///
/// Split into two owned halves so the handler can destructure it — the write
/// half is consumed by [`CardWrite::bind`] while the audit half stays alive for
/// use *after* the query has run.
#[derive(Debug)]
pub(super) struct CardUpdate {
    pub(super) write: CardWrite,
    pub(super) audit: CardAudit,
}

impl CardUpdate {
    /// Work out what `payload` changes about `existing`, without touching the
    /// database.
    ///
    /// Takes the payload **by value** because the plan keeps the request's
    /// strings for binding — the caller has no use for them afterwards, and
    /// owning them here saves the handler from juggling partial borrows of the
    /// payload across an `await`.
    ///
    /// Returns:
    ///
    /// * `Ok(None)` — the request changes nothing. The caller returns the
    ///   stored card untouched: no write, no audit row, no event.
    /// * `Ok(Some(plan))` — the write to perform.
    /// * `Err(422)` — the tag list was rejected by [`shared::tags::normalize`].
    pub(super) fn plan(
        payload: shared::UpdateCardRequest,
        existing: &DbCard,
    ) -> Result<Option<Self>, StatusCode> {
        // Tags arrive as a full replacement list; normalize before deciding
        // whether this request changes anything, so a request that only
        // re-sends the tags a card already has is treated as a no-op rather
        // than an edit.
        let tags = payload.tags.as_deref().map(normalize_tags).transpose()?;
        let tags_changed = tags
            .as_ref()
            .is_some_and(|new_tags| *new_tags != existing.tags);

        // Build a single atomic UPDATE covering all changed fields.
        let mut set_parts: Vec<String> = Vec::new();

        if payload.body.is_some() {
            set_parts.push("body = $body".to_string());
        }
        if payload.column_id.is_some() {
            set_parts.push("column = type::thing('columns', $col_id)".to_string());
        }
        if payload.position.is_some() {
            set_parts.push("position = $position".to_string());
        }
        if tags_changed {
            set_parts.push("tags = $tags".to_string());
        }

        // Nothing changed — the caller returns the existing card unchanged.
        if set_parts.is_empty() {
            return Ok(None);
        }

        // Always stamp the editor — every successful mutation records who did it.
        set_parts.push("last_edited_by = $editor".to_string());

        // Layout-only changes are "move" (history toggles / filters). Body edits — alone or
        // combined with position/column in one PUT — stay "update" so audit_edit_session merge works.
        // A tag change is content, not layout, so it keeps the row out of "move" too: tagging a
        // card must never be something the history drawer's "show moves" toggle hides.
        let has_layout_change = payload.column_id.is_some() || payload.position.is_some();
        let has_body_change = payload.body.is_some();
        let action = if has_layout_change && !has_body_change && !tags_changed {
            "move"
        } else {
            "update"
        };

        // Every tag change gets its own discrete audit row. Merging one into
        // an in-flight body-edit session would hide it behind a "+12 chars"
        // summary and make the tag delta unrecoverable from history, so the
        // session token is deliberately dropped whenever tags moved.
        //
        // Only "update" rows can merge at all (see `audit::record_and_broadcast`),
        // so a "move" drops the token too. An empty token is treated as absent.
        let edit_session = if action == "update" && !tags_changed {
            payload.audit_edit_session.filter(|s| !s.is_empty())
        } else {
            None
        };

        Ok(Some(Self {
            write: CardWrite {
                set_parts,
                body: payload.body,
                column_id: payload.column_id,
                position: payload.position,
                // Bound only when it changed, matching the `set_parts` fragment.
                tags: if tags_changed { tags } else { None },
            },
            audit: CardAudit {
                action,
                edit_session,
            },
        }))
    }
}

impl CardWrite {
    /// The `UPDATE` statement for this plan. The `$card_id` and `$editor`
    /// parameters are the caller's to bind; everything else comes from
    /// [`Self::bind`].
    pub(super) fn statement(&self) -> String {
        format!(
            "UPDATE type::thing('cards', $card_id) SET {}",
            self.set_parts.join(", ")
        )
    }

    /// Bind this plan's values onto `q`, consuming the plan.
    ///
    /// Each `if let` here pairs with a `set_parts` fragment pushed in
    /// [`CardUpdate::plan`], so the statement never references a parameter that
    /// was not bound. Bind order is irrelevant to SurrealDB — the parameters
    /// are named — but is kept stable for readability.
    pub(super) fn bind<'a>(self, mut q: Query<'a, Db>) -> Query<'a, Db> {
        if let Some(body) = self.body {
            q = q.bind(("body", body));
        }
        if let Some(col_id) = self.column_id {
            q = q.bind(("col_id", col_id));
        }
        if let Some(position) = self.position {
            q = q.bind(("position", position));
        }
        if let Some(tags) = self.tags {
            q = q.bind(("tags", tags));
        }
        q
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use surrealdb::sql::{Datetime, Thing};

    /// A stored card carrying `tags`. Only the tag list is read by the planner;
    /// everything else is filler so the struct can be constructed.
    fn card_tagged(tags: &[&str]) -> DbCard {
        DbCard {
            // `Thing` is SurrealDB's record id: a (table, id) pair.
            id: Thing::from(("cards", "c1")),
            column: Thing::from(("columns", "col")),
            body: "stored body".to_string(),
            position: 1024,
            number: Some(7),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            last_edited_by: None,
            created_at: Datetime::default(),
            updated_at: Datetime::default(),
        }
    }

    /// Plan `payload` against a card with no tags, asserting it is not a no-op.
    fn plan_of(payload: shared::UpdateCardRequest) -> CardUpdate {
        CardUpdate::plan(payload, &card_tagged(&[]))
            .expect("payload should be accepted")
            .expect("payload should plan a write")
    }

    #[test]
    fn an_empty_payload_plans_nothing() {
        // Every field `None`: there is no field to write, so the handler must
        // short-circuit rather than issue an `UPDATE ... SET last_edited_by`.
        let planned = CardUpdate::plan(shared::UpdateCardRequest::default(), &card_tagged(&[]))
            .expect("an empty payload is valid");
        assert!(planned.is_none());
    }

    #[test]
    fn re_sending_the_tags_a_card_already_has_plans_nothing() {
        // The client always sends the complete tag set it wants, so "no change"
        // arrives as the current list rather than as an absent field.
        let payload = shared::UpdateCardRequest {
            tags: Some(vec!["alpha".to_string(), "beta".to_string()]),
            ..Default::default()
        };
        let planned = CardUpdate::plan(payload, &card_tagged(&["alpha", "beta"]))
            .expect("an unchanged tag list is valid");
        assert!(planned.is_none());
    }

    #[test]
    fn tags_that_normalize_onto_the_stored_list_plan_nothing() {
        // Normalization runs *before* the comparison, so padding and the `#`
        // prefix the user typed do not by themselves make this an edit.
        let payload = shared::UpdateCardRequest {
            tags: Some(vec!["  alpha ".to_string(), "#beta".to_string()]),
            ..Default::default()
        };
        let planned = CardUpdate::plan(payload, &card_tagged(&["alpha", "beta"]))
            .expect("a re-normalized tag list is valid");
        assert!(planned.is_none());
    }

    #[test]
    fn recasing_a_tag_is_a_real_change() {
        // `shared::tags::normalize` dedups case-insensitively but keeps the
        // spelling it was given, and the change check is plain `Vec` equality.
        // So "alpha" -> "Alpha" is an edit the user made and is written as one
        // — a deliberate counterpart to the no-op above, pinned here because
        // the two cases look alike and are one `eq_ignore_case` apart.
        let payload = shared::UpdateCardRequest {
            tags: Some(vec!["Alpha".to_string()]),
            ..Default::default()
        };
        let plan = CardUpdate::plan(payload, &card_tagged(&["alpha"]))
            .expect("valid")
            .expect("recasing is a write");
        assert_eq!(plan.audit.action, "update");
        assert_eq!(
            plan.write.statement(),
            "UPDATE type::thing('cards', $card_id) SET tags = $tags, last_edited_by = $editor"
        );
    }

    #[test]
    fn a_body_edit_is_an_update() {
        let payload = shared::UpdateCardRequest {
            body: Some("new body".to_string()),
            ..Default::default()
        };
        let plan = plan_of(payload);
        assert_eq!(plan.audit.action, "update");
        assert_eq!(
            plan.write.statement(),
            "UPDATE type::thing('cards', $card_id) SET body = $body, last_edited_by = $editor"
        );
    }

    #[test]
    fn a_position_change_alone_is_a_move() {
        // Layout-only: the history drawer's "show moves" toggle may hide it.
        let payload = shared::UpdateCardRequest {
            position: Some(2048),
            ..Default::default()
        };
        assert_eq!(plan_of(payload).audit.action, "move");
    }

    #[test]
    fn a_column_change_alone_is_a_move() {
        let payload = shared::UpdateCardRequest {
            column_id: Some("col2".to_string()),
            ..Default::default()
        };
        assert_eq!(plan_of(payload).audit.action, "move");
    }

    #[test]
    fn a_body_edit_combined_with_a_move_stays_an_update() {
        // One PUT carrying both must not be filed as a "move", or the body edit
        // would vanish from a history view with moves hidden.
        let payload = shared::UpdateCardRequest {
            body: Some("new body".to_string()),
            position: Some(2048),
            ..Default::default()
        };
        assert_eq!(plan_of(payload).audit.action, "update");
    }

    #[test]
    fn a_tag_change_combined_with_a_move_stays_an_update() {
        // Tags are content, not layout. Filing this as a "move" would let the
        // "show moves" toggle hide a tag change, which is not recoverable from
        // anywhere else in the history drawer.
        let payload = shared::UpdateCardRequest {
            tags: Some(vec!["alpha".to_string()]),
            position: Some(2048),
            ..Default::default()
        };
        assert_eq!(plan_of(payload).audit.action, "update");
    }

    #[test]
    fn a_body_edit_keeps_its_edit_session_token() {
        // Repeated saves in one editing stretch merge into a single audit row;
        // the token is what lets `record_and_broadcast` find the row to merge.
        let payload = shared::UpdateCardRequest {
            body: Some("new body".to_string()),
            audit_edit_session: Some("sess-1".to_string()),
            ..Default::default()
        };
        assert_eq!(
            plan_of(payload).audit.edit_session.as_deref(),
            Some("sess-1")
        );
    }

    #[test]
    fn a_tag_change_drops_the_edit_session_token() {
        // Merging a tag change into a body-edit session would hide the tag
        // delta behind that session's "+N chars" summary.
        let payload = shared::UpdateCardRequest {
            body: Some("new body".to_string()),
            tags: Some(vec!["alpha".to_string()]),
            audit_edit_session: Some("sess-1".to_string()),
            ..Default::default()
        };
        assert_eq!(plan_of(payload).audit.edit_session, None);
    }

    #[test]
    fn a_move_drops_the_edit_session_token() {
        // Only "update" rows are merge candidates, so carrying the token on a
        // "move" would be dead weight in the audit row.
        let payload = shared::UpdateCardRequest {
            position: Some(2048),
            audit_edit_session: Some("sess-1".to_string()),
            ..Default::default()
        };
        assert_eq!(plan_of(payload).audit.edit_session, None);
    }

    #[test]
    fn an_empty_edit_session_token_is_treated_as_absent() {
        let payload = shared::UpdateCardRequest {
            body: Some("new body".to_string()),
            audit_edit_session: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(plan_of(payload).audit.edit_session, None);
    }

    #[test]
    fn an_over_long_tag_is_rejected_as_unprocessable() {
        let payload = shared::UpdateCardRequest {
            tags: Some(vec!["x".repeat(shared::tags::MAX_TAG_CHARS + 1)]),
            ..Default::default()
        };
        let err = CardUpdate::plan(payload, &card_tagged(&[])).expect_err("tag exceeds the cap");
        assert_eq!(err, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn too_many_tags_are_rejected_as_unprocessable() {
        let tags: Vec<String> = (0..=shared::tags::MAX_TAGS_PER_CARD)
            .map(|i| format!("tag{i}"))
            .collect();
        let payload = shared::UpdateCardRequest {
            tags: Some(tags),
            ..Default::default()
        };
        let err = CardUpdate::plan(payload, &card_tagged(&[])).expect_err("too many tags");
        assert_eq!(err, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn every_changed_field_lands_in_one_statement() {
        // The whole update is a single atomic `UPDATE`, in a fixed fragment
        // order, with `last_edited_by` always stamped last.
        let payload = shared::UpdateCardRequest {
            body: Some("new body".to_string()),
            position: Some(2048),
            column_id: Some("col2".to_string()),
            tags: Some(vec!["alpha".to_string()]),
            audit_edit_session: None,
        };
        assert_eq!(
            plan_of(payload).write.statement(),
            "UPDATE type::thing('cards', $card_id) SET \
             body = $body, \
             column = type::thing('columns', $col_id), \
             position = $position, \
             tags = $tags, \
             last_edited_by = $editor"
        );
    }

    #[test]
    fn an_unchanged_tag_list_is_not_written_alongside_a_real_change() {
        // The card already has these tags, so the statement must not carry a
        // `tags = $tags` fragment even though the request supplied the field.
        let payload = shared::UpdateCardRequest {
            body: Some("new body".to_string()),
            tags: Some(vec!["alpha".to_string()]),
            ..Default::default()
        };
        let plan = CardUpdate::plan(payload, &card_tagged(&["alpha"]))
            .expect("valid")
            .expect("the body change is a write");
        assert_eq!(
            plan.write.statement(),
            "UPDATE type::thing('cards', $card_id) SET body = $body, last_edited_by = $editor"
        );
        // ...and because tags did not move, the edit session survives.
        assert_eq!(plan.audit.action, "update");
    }
}
