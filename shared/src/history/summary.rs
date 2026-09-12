//! Headline + sub-line for one audit log row.
//!
//! Implements the "primary label reflects the world *after* the change"
//! rule from card #132. `snapshot_after` is preferred for headlines; the
//! `snapshot_before` is consulted only for delta context (rename "was …",
//! delete "what was deleted") and as a fallback when there is no after.
//!
//! The deriver is pure — it reads `entry` and produces strings. Time and
//! actor formatting live in their sibling modules.

use crate::AuditLogEntry;
use serde_json::Value;

/// One row's user-facing strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// Bold, single-line headline (e.g. `Edited card «Polish audit history drawer»`).
    pub headline: String,
    /// Optional muted second line for context (e.g. `Card #132`,
    /// `was «Old name»`). `None` when the headline is self-contained.
    pub sub: Option<String>,
}

impl Summary {
    fn new(headline: impl Into<String>) -> Self {
        Self {
            headline: headline.into(),
            sub: None,
        }
    }

    fn with_sub(mut self, sub: impl Into<String>) -> Self {
        self.sub = Some(sub.into());
        self
    }
}

/// Top-level: dispatch on `(action, entity_type)` and pick the best
/// headline/sub from the available snapshots.
pub fn derive_summary(entry: &AuditLogEntry) -> Summary {
    let after = entry.snapshot_after.as_ref();
    let before = entry.snapshot_before.as_ref();

    match (entry.action.as_str(), entry.entity_type.as_str()) {
        // ── create / restore ────────────────────────────────────────────
        ("create" | "restore", "board") => board_present(after, &entry.action, &entry.entity_id),
        ("create" | "restore", "column") => column_present(after, &entry.action, &entry.entity_id),
        ("create" | "restore", "card") => card_present(after, &entry.action, &entry.entity_id),

        // ── baseline (existing-at-startup snapshot) ─────────────────────
        ("baseline", "board") => board_baseline(after, &entry.entity_id),
        ("baseline", "column") => column_baseline(after, &entry.entity_id),
        ("baseline", "card") => card_baseline(after, &entry.entity_id),

        // ── update ──────────────────────────────────────────────────────
        ("update", "board") => board_update(before, after, &entry.entity_id),
        ("update", "column") => column_update(before, after, &entry.entity_id),
        ("update", "card") => card_update(before, after, &entry.entity_id),

        // ── move ────────────────────────────────────────────────────────
        ("move", "card") => card_move(before, after, &entry.entity_id),
        ("move", _) => Summary::new(format!("Moved {}", entry.entity_type)),

        // ── delete ──────────────────────────────────────────────────────
        ("delete", "board") => board_delete(before, &entry.entity_id),
        ("delete", "column") => column_delete(before, &entry.entity_id),
        ("delete", "card") => card_delete(before, &entry.entity_id),

        // ── card links ──────────────────────────────────────────────────
        ("create", "card_link") => link_present(after, "Linked"),
        ("update", "card_link") => link_update(before, after),
        ("delete", "card_link") => link_present(before, "Unlinked"),

        // ── unknown action / entity_type — defensive fallback ───────────
        (action, ty) => Summary::new(format!("{action} {ty}")),
    }
}

// ── helpers ──────────────────────────────────────────────────────────────

fn str_field<'a>(snap: Option<&'a Value>, field: &str) -> Option<&'a str> {
    snap?.get(field)?.as_str()
}

fn u64_field(snap: Option<&Value>, field: &str) -> Option<u64> {
    snap?.get(field)?.as_u64()
}

fn i64_field(snap: Option<&Value>, field: &str) -> Option<i64> {
    snap?.get(field)?.as_i64()
}

/// Extract the user-facing title for a card from its body. Picks the first
/// markdown `#`-heading; falls back to the first non-empty line. Truncated
/// to ~60 chars with an ellipsis so very long titles don't blow out the
/// drawer width.
///
/// Public because the board search popup labels its card-number suggestions the
/// same way the history drawer labels rows — one definition of "a card's title"
/// for the whole app.
pub fn card_title_from_body(body: &str) -> String {
    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let stripped = line.trim_start_matches('#').trim();
        let candidate = if stripped.is_empty() { line } else { stripped };
        return truncate(candidate, 60);
    }
    "(empty)".to_string()
}

fn truncate(s: &str, max_chars: usize) -> String {
    let n = s.chars().count();
    if n <= max_chars {
        return s.to_string();
    }
    let cut: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{cut}…")
}

fn quoted(s: &str) -> String {
    format!("«{s}»")
}

fn fallback_id_label(id: &str) -> String {
    truncate(id, 12)
}

// ── board ────────────────────────────────────────────────────────────────

fn board_present(after: Option<&Value>, action: &str, entity_id: &str) -> Summary {
    let name = str_field(after, "name").unwrap_or(entity_id);
    let verb = if action == "restore" {
        "Restored"
    } else {
        "Created"
    };
    Summary::new(format!("{verb} board {}", quoted(name)))
}

fn board_baseline(after: Option<&Value>, entity_id: &str) -> Summary {
    let name = str_field(after, "name").unwrap_or(entity_id);
    Summary::new(format!("Board {}", quoted(name)))
}

fn board_update(before: Option<&Value>, after: Option<&Value>, entity_id: &str) -> Summary {
    let new_name = str_field(after, "name").unwrap_or(entity_id);
    let old_name = str_field(before, "name");
    match old_name {
        Some(old) if old != new_name => {
            Summary::new(format!("Renamed board to {}", quoted(new_name)))
                .with_sub(format!("was {}", quoted(old)))
        }
        _ => Summary::new(format!("Updated board {}", quoted(new_name))),
    }
}

fn board_delete(before: Option<&Value>, entity_id: &str) -> Summary {
    let name = str_field(before, "name").unwrap_or(entity_id);
    Summary::new(format!("Deleted board {}", quoted(name)))
}

// ── column ───────────────────────────────────────────────────────────────

fn column_present(after: Option<&Value>, action: &str, entity_id: &str) -> Summary {
    let name = str_field(after, "name").unwrap_or(entity_id);
    let verb = if action == "restore" {
        "Restored"
    } else {
        "Created"
    };
    Summary::new(format!("{verb} column {}", quoted(name)))
}

fn column_baseline(after: Option<&Value>, entity_id: &str) -> Summary {
    let name = str_field(after, "name").unwrap_or(entity_id);
    Summary::new(format!("Column {}", quoted(name)))
}

fn column_update(before: Option<&Value>, after: Option<&Value>, entity_id: &str) -> Summary {
    let new_name = str_field(after, "name").unwrap_or(entity_id);
    let old_name = str_field(before, "name");

    if let Some(old) = old_name.filter(|o| *o != new_name) {
        return Summary::new(format!("Renamed column to {}", quoted(new_name)))
            .with_sub(format!("was {}", quoted(old)));
    }

    let new_pos = i64_field(after, "position");
    let old_pos = i64_field(before, "position");
    if let (Some(np), Some(op)) = (new_pos, old_pos)
        && np != op
    {
        return Summary::new(format!("Reordered column {}", quoted(new_name)));
    }

    Summary::new(format!("Updated column {}", quoted(new_name)))
}

fn column_delete(before: Option<&Value>, entity_id: &str) -> Summary {
    let name = str_field(before, "name").unwrap_or(entity_id);
    Summary::new(format!("Deleted column {}", quoted(name)))
}

// ── card ─────────────────────────────────────────────────────────────────

fn card_sub(after: Option<&Value>, before: Option<&Value>) -> Option<String> {
    let n = u64_field(after, "number").or_else(|| u64_field(before, "number"))?;
    Some(format!("Card #{n}"))
}

fn card_present(after: Option<&Value>, action: &str, entity_id: &str) -> Summary {
    let title = str_field(after, "body")
        .map(card_title_from_body)
        .unwrap_or_else(|| fallback_id_label(entity_id));
    let verb = if action == "restore" {
        "Restored"
    } else {
        "Created"
    };
    let mut s = Summary::new(format!("{verb} card {}", quoted(&title)));
    if let Some(sub) = card_sub(after, None) {
        s = s.with_sub(sub);
    }
    s
}

fn card_baseline(after: Option<&Value>, entity_id: &str) -> Summary {
    let title = str_field(after, "body")
        .map(card_title_from_body)
        .unwrap_or_else(|| fallback_id_label(entity_id));
    let mut s = Summary::new(format!("Card {}", quoted(&title)));
    if let Some(sub) = card_sub(after, None) {
        s = s.with_sub(sub);
    }
    s
}

fn card_update(before: Option<&Value>, after: Option<&Value>, entity_id: &str) -> Summary {
    let body_before = str_field(before, "body");
    let body_after = str_field(after, "body");
    let title_before = body_before.map(card_title_from_body);
    let title_after = body_after.map(card_title_from_body);

    let tags_before = tags_field(before);
    let tags_after = tags_field(after);
    let tag_delta = tags_delta_label(&tags_before, &tags_after);

    let new_col = str_field(after, "column_id");
    let old_col = str_field(before, "column_id");
    let column_changed = match (old_col, new_col) {
        (Some(a), Some(b)) => a != b,
        _ => false,
    };

    // Move framing wins over rename / edit when both happen in the same
    // audit row (rare in practice — the move route uses a separate
    // `move` action and rename is body-only).
    if column_changed {
        let title = title_after
            .or(title_before)
            .unwrap_or_else(|| fallback_id_label(entity_id));
        let mut s = Summary::new(format!("Moved card {}", quoted(&title)));
        // A move that also retags carries the tag delta, so no change a single
        // row recorded is invisible in the drawer.
        if let Some(sub) = join_sub(&[card_sub(after, before), tag_delta]) {
            s = s.with_sub(sub);
        }
        return s;
    }

    // Title rename — first markdown heading line changed. Mirrors the
    // board / column rename UX: headline carries the new title, sub
    // carries the old title plus the card number for context.
    if let (Some(old), Some(new)) = (title_before.as_ref(), title_after.as_ref())
        && old != new
    {
        let headline = format!("Renamed card to {}", quoted(new));
        let was = format!("was {}", quoted(old));
        let sub = match card_sub(after, before) {
            Some(n) => format!("{was} · {n}"),
            None => was,
        };
        let mut summary = Summary::new(headline);
        if let Some(sub) = join_sub(&[Some(sub), tag_delta]) {
            summary = summary.with_sub(sub);
        }
        return summary;
    }

    // Body changed below the heading (typo fix, paragraph rewrite, …).
    // Keep the "Edited" headline and append a character delta to the
    // sub so viewers get a quick signal of how big the edit was.
    let title = title_after
        .or(title_before)
        .unwrap_or_else(|| fallback_id_label(entity_id));
    let body_changed = match (body_before, body_after) {
        (Some(a), Some(b)) => a != b,
        (None, Some(_)) | (Some(_), None) => true,
        _ => false,
    };

    // Tags-only change: its own verb, so a tag edit never reads as a body edit
    // in the drawer. "Untagged" is reserved for a change that only removed.
    let headline = if body_changed {
        format!("Edited card {}", quoted(&title))
    } else if tag_delta.is_some() {
        let only_removals = tags_added(&tags_before, &tags_after).is_empty();
        let verb = if only_removals { "Untagged" } else { "Tagged" };
        format!("{verb} card {}", quoted(&title))
    } else {
        format!("Updated card {}", quoted(&title))
    };

    let card_n = card_sub(after, before);
    let body_delta = if body_changed {
        body_delta_label(body_before.unwrap_or(""), body_after.unwrap_or(""))
    } else {
        None
    };
    let sub = join_sub(&[card_n, body_delta, tag_delta]);
    let mut s = Summary::new(headline);
    if let Some(sub) = sub {
        s = s.with_sub(sub);
    }
    s
}

/// The `tags` array from a snapshot, or an empty list when the key is absent
/// (rows recorded before cards had tags).
fn tags_field(snap: Option<&Value>) -> Vec<String> {
    snap.and_then(|v| v.get("tags"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Tags present in `after` but not in `before`, compared case-insensitively so
/// a pure re-spelling (`bug` → `Bug`) doesn't read as add-plus-remove.
fn tags_added(before: &[String], after: &[String]) -> Vec<String> {
    after
        .iter()
        .filter(|t| !before.iter().any(|b| crate::tags::eq_ignore_case(b, t)))
        .cloned()
        .collect()
}

/// Compact tag delta for a card update: `+bug, −stale`. `None` when the tag
/// list is unchanged.
///
/// The minus is U+2212, matching [`body_delta_label`] so a sub-line carrying
/// both deltas doesn't mix two different dashes.
fn tags_delta_label(before: &[String], after: &[String]) -> Option<String> {
    let added = tags_added(before, after);
    let removed = tags_added(after, before);
    if added.is_empty() && removed.is_empty() {
        return None;
    }
    let parts: Vec<String> = added
        .iter()
        .map(|t| format!("+{t}"))
        .chain(removed.iter().map(|t| format!("\u{2212}{t}")))
        .collect();
    Some(parts.join(", "))
}

/// Join the non-empty pieces of a sub-line with the drawer's ` · ` separator.
fn join_sub(parts: &[Option<String>]) -> Option<String> {
    let joined: Vec<&str> = parts.iter().filter_map(|p| p.as_deref()).collect();
    if joined.is_empty() {
        None
    } else {
        Some(joined.join(" · "))
    }
}

fn card_move(before: Option<&Value>, after: Option<&Value>, entity_id: &str) -> Summary {
    let title = str_field(after, "body")
        .map(card_title_from_body)
        .or_else(|| str_field(before, "body").map(card_title_from_body))
        .unwrap_or_else(|| fallback_id_label(entity_id));
    let mut s = Summary::new(format!("Moved card {}", quoted(&title)));
    if let Some(sub) = card_sub(after, before) {
        s = s.with_sub(sub);
    }
    s
}

/// Compact size delta for a card-body edit: `+12 chars`, `−3 chars`, or
/// `body changed` when the length is unchanged but the content differs
/// (typo fix that swaps characters of equal width). Returns `None` only
/// when the strings are byte-identical.
///
/// Counts are in Unicode scalar values rather than bytes so multi-byte
/// emoji / accented characters count as one. We don't compute precise
/// added/removed pairs (that would need a real diff algorithm) — the
/// sub-line is meant as a quick signal, not a forensic record.
fn body_delta_label(before: &str, after: &str) -> Option<String> {
    if before == after {
        return None;
    }
    let bn = before.chars().count() as i64;
    let an = after.chars().count() as i64;
    let delta = an - bn;
    if delta > 0 {
        Some(format!("+{delta} chars"))
    } else if delta < 0 {
        Some(format!("\u{2212}{} chars", delta.unsigned_abs()))
    } else {
        Some("body changed".to_string())
    }
}

fn card_delete(before: Option<&Value>, entity_id: &str) -> Summary {
    let title = str_field(before, "body")
        .map(card_title_from_body)
        .unwrap_or_else(|| fallback_id_label(entity_id));
    let mut s = Summary::new(format!("Deleted card {}", quoted(&title)));
    if let Some(sub) = card_sub(None, before) {
        s = s.with_sub(sub);
    }
    s
}

// ── card link ────────────────────────────────────────────────────────────

/// `#12 → #15` from a link snapshot, falling back to the raw ids when a
/// snapshot predates the numbers being recorded. The arrow reads the same way
/// the link does: the predecessor comes first.
fn link_label(snap: Option<&Value>) -> String {
    let end = |number_key: &str, id_key: &str| {
        u64_field(snap, number_key)
            .map(|n| format!("#{n}"))
            .or_else(|| str_field(snap, id_key).map(fallback_id_label))
            .unwrap_or_else(|| "?".to_string())
    };
    format!(
        "{} \u{2192} {}",
        end("predecessor_number", "predecessor_id"),
        end("successor_number", "successor_id")
    )
}

/// Reason sub-line, when the snapshot carries one.
fn link_reason_sub(snap: Option<&Value>) -> Option<String> {
    str_field(snap, "reason")
        .filter(|r| !r.is_empty())
        .map(|r| quoted(&truncate(r, 60)))
}

/// Create and delete share a shape: the verb, then which two cards, with the
/// reason as context. A delete reads its snapshot from `before`, so the caller
/// passes whichever side holds the link.
fn link_present(snap: Option<&Value>, verb: &str) -> Summary {
    let mut s = Summary::new(format!("{verb} {}", link_label(snap)));
    if let Some(sub) = link_reason_sub(snap) {
        s = s.with_sub(sub);
    }
    s
}

/// Only the reason is editable, so an update is always a reason change. The
/// sub carries the new reason (or notes that it was cleared) plus the old one.
fn link_update(before: Option<&Value>, after: Option<&Value>) -> Summary {
    let label = link_label(after.or(before));
    let new_reason = str_field(after, "reason").filter(|r| !r.is_empty());
    let old_reason = str_field(before, "reason").filter(|r| !r.is_empty());
    let headline = match new_reason {
        Some(_) => format!("Changed reason on {label}"),
        None => format!("Cleared reason on {label}"),
    };
    let now = new_reason.map(|r| quoted(&truncate(r, 60)));
    let was = old_reason.map(|r| format!("was {}", quoted(&truncate(r, 60))));
    let mut s = Summary::new(headline);
    if let Some(sub) = join_sub(&[now, was]) {
        s = s.with_sub(sub);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(
        action: &str,
        entity_type: &str,
        before: Option<Value>,
        after: Option<Value>,
    ) -> AuditLogEntry {
        AuditLogEntry {
            id: "audit-1".into(),
            created_at: "2026-05-07T12:00:00Z".into(),
            actor_sub: "alice".into(),
            actor_display_name: "Alice".into(),
            entity_type: entity_type.into(),
            entity_id: "entity-1".into(),
            board_id: "board-1".into(),
            action: action.into(),
            snapshot_before: before,
            snapshot_after: after,
            restored_from: None,
            batch_group: None,
            audit_edit_session: None,
        }
    }

    fn card_snap(body: &str, column_id: &str, number: u64) -> Value {
        tagged_card_snap(body, column_id, number, &[])
    }

    /// A snapshot shaped like the ones written before cards had tags — no
    /// `tags` key at all, rather than an empty list.
    fn card_snap_untagged(body: &str, column_id: &str, number: u64) -> Value {
        json!({
            "id": "card-1",
            "column_id": column_id,
            "body": body,
            "position": 0,
            "number": number,
            "last_edited_by": null,
            "created_at": "x",
            "updated_at": "x",
        })
    }

    fn tagged_card_snap(body: &str, column_id: &str, number: u64, tags: &[&str]) -> Value {
        json!({
            "id": "card-1",
            "column_id": column_id,
            "body": body,
            "position": 0,
            "number": number,
            "tags": tags,
            "last_edited_by": null,
            "created_at": "x",
            "updated_at": "x",
        })
    }

    fn col_snap(name: &str, position: i64) -> Value {
        json!({
            "id": "col-1",
            "board_id": "board-1",
            "name": name,
            "position": position,
            "last_edited_by": null,
            "created_at": "x",
            "updated_at": "x",
        })
    }

    fn board_snap(name: &str) -> Value {
        json!({
            "id": "board-1",
            "name": name,
            "last_edited_by": null,
            "created_at": "x",
            "updated_at": "x",
        })
    }

    // ── card ─────────────────────────────────────────────────────────

    #[test]
    fn card_create_uses_first_heading_as_title() {
        let s = derive_summary(&entry(
            "create",
            "card",
            None,
            Some(card_snap("# My new card\n\nbody", "col-a", 7)),
        ));
        assert_eq!(s.headline, "Created card «My new card»");
        assert_eq!(s.sub.as_deref(), Some("Card #7"));
    }

    #[test]
    fn card_create_falls_back_to_first_line_when_no_heading() {
        let s = derive_summary(&entry(
            "create",
            "card",
            None,
            Some(card_snap("Just a line\nmore", "col-a", 1)),
        ));
        assert_eq!(s.headline, "Created card «Just a line»");
    }

    #[test]
    fn card_create_handles_empty_body() {
        let s = derive_summary(&entry(
            "create",
            "card",
            None,
            Some(card_snap("", "col-a", 1)),
        ));
        assert_eq!(s.headline, "Created card «(empty)»");
    }

    #[test]
    fn card_update_with_body_growth_appends_char_delta() {
        // Title unchanged, body grew by 11 chars (" extended.." padding).
        let before = card_snap("# Title\n\nshort body", "col-a", 7);
        let after = card_snap("# Title\n\nshort body extended..", "col-a", 7);
        let s = derive_summary(&entry("update", "card", Some(before), Some(after)));
        assert_eq!(s.headline, "Edited card «Title»");
        assert_eq!(s.sub.as_deref(), Some("Card #7 · +11 chars"));
    }

    #[test]
    fn card_update_with_body_shrink_uses_unicode_minus() {
        let before = card_snap("# Title\n\nlong body content", "col-a", 7);
        let after = card_snap("# Title\n\nlong body", "col-a", 7);
        let s = derive_summary(&entry("update", "card", Some(before), Some(after)));
        assert_eq!(s.headline, "Edited card «Title»");
        // Note: the sign is a Unicode minus (U+2212), not ASCII hyphen.
        assert_eq!(s.sub.as_deref(), Some("Card #7 · \u{2212}8 chars"));
    }

    #[test]
    fn card_update_with_body_swap_says_body_changed() {
        // Same length, different content (typo fix).
        let before = card_snap("# Title\n\nteh quick", "col-a", 7);
        let after = card_snap("# Title\n\nthe quick", "col-a", 7);
        let s = derive_summary(&entry("update", "card", Some(before), Some(after)));
        assert_eq!(s.headline, "Edited card «Title»");
        assert_eq!(s.sub.as_deref(), Some("Card #7 · body changed"));
    }

    #[test]
    fn card_update_with_title_change_says_renamed_with_was() {
        let before = card_snap("# Old title\n\nbody", "col-a", 7);
        let after = card_snap("# New title\n\nbody", "col-a", 7);
        let s = derive_summary(&entry("update", "card", Some(before), Some(after)));
        assert_eq!(s.headline, "Renamed card to «New title»");
        assert_eq!(s.sub.as_deref(), Some("was «Old title» · Card #7"));
    }

    #[test]
    fn card_update_with_column_change_says_moved() {
        let before = card_snap("# Title", "col-a", 7);
        let after = card_snap("# Title", "col-b", 7);
        let s = derive_summary(&entry("update", "card", Some(before), Some(after)));
        assert_eq!(s.headline, "Moved card «Title»");
    }

    #[test]
    fn card_update_move_wins_over_rename_when_both_change() {
        // Column change AND title change in one update → move framing
        // takes priority because it's the more salient operation.
        let before = card_snap("# Old", "col-a", 7);
        let after = card_snap("# New", "col-b", 7);
        let s = derive_summary(&entry("update", "card", Some(before), Some(after)));
        assert_eq!(s.headline, "Moved card «New»");
    }

    #[test]
    fn card_update_with_no_diff_falls_back_to_updated() {
        // Same body, same column — exotic but possible (e.g. position-only
        // update). No body delta, so the sub is just the card number.
        let before = card_snap("# Title", "col-a", 7);
        let after = card_snap("# Title", "col-a", 7);
        let s = derive_summary(&entry("update", "card", Some(before), Some(after)));
        assert_eq!(s.headline, "Updated card «Title»");
        assert_eq!(s.sub.as_deref(), Some("Card #7"));
    }

    #[test]
    fn card_update_adding_tags_says_tagged_with_plus_delta() {
        let before = tagged_card_snap("# Title", "col-a", 7, &[]);
        let after = tagged_card_snap("# Title", "col-a", 7, &["bug", "urgent"]);
        let s = derive_summary(&entry("update", "card", Some(before), Some(after)));
        assert_eq!(s.headline, "Tagged card «Title»");
        assert_eq!(s.sub.as_deref(), Some("Card #7 · +bug, +urgent"));
    }

    #[test]
    fn card_update_removing_all_tags_says_untagged() {
        let before = tagged_card_snap("# Title", "col-a", 7, &["bug"]);
        let after = tagged_card_snap("# Title", "col-a", 7, &[]);
        let s = derive_summary(&entry("update", "card", Some(before), Some(after)));
        assert_eq!(s.headline, "Untagged card «Title»");
        // Removals use a Unicode minus (U+2212), matching the body char delta.
        assert_eq!(s.sub.as_deref(), Some("Card #7 · \u{2212}bug"));
    }

    #[test]
    fn card_update_swapping_tags_says_tagged_with_both_signs() {
        let before = tagged_card_snap("# Title", "col-a", 7, &["stale"]);
        let after = tagged_card_snap("# Title", "col-a", 7, &["fresh"]);
        let s = derive_summary(&entry("update", "card", Some(before), Some(after)));
        assert_eq!(s.headline, "Tagged card «Title»");
        assert_eq!(s.sub.as_deref(), Some("Card #7 · +fresh, \u{2212}stale"));
    }

    #[test]
    fn card_update_tag_respelling_is_not_a_change() {
        // Case-insensitive comparison: `bug` → `Bug` is the same tag, so the
        // row falls back to the plain "Updated" wording with no tag delta.
        let before = tagged_card_snap("# Title", "col-a", 7, &["bug"]);
        let after = tagged_card_snap("# Title", "col-a", 7, &["Bug"]);
        let s = derive_summary(&entry("update", "card", Some(before), Some(after)));
        assert_eq!(s.headline, "Updated card «Title»");
        assert_eq!(s.sub.as_deref(), Some("Card #7"));
    }

    #[test]
    fn card_update_tag_reorder_is_not_a_change() {
        let before = tagged_card_snap("# Title", "col-a", 7, &["a", "b"]);
        let after = tagged_card_snap("# Title", "col-a", 7, &["b", "a"]);
        let s = derive_summary(&entry("update", "card", Some(before), Some(after)));
        assert_eq!(s.headline, "Updated card «Title»");
    }

    #[test]
    fn card_update_body_edit_keeps_edited_headline_and_appends_tag_delta() {
        // Body wins the headline; the tag delta still rides along in the sub so
        // nothing about the change is lost.
        let before = tagged_card_snap("# Title\n\nshort", "col-a", 7, &[]);
        let after = tagged_card_snap("# Title\n\nshorter body", "col-a", 7, &["bug"]);
        let s = derive_summary(&entry("update", "card", Some(before), Some(after)));
        assert_eq!(s.headline, "Edited card «Title»");
        assert_eq!(s.sub.as_deref(), Some("Card #7 · +7 chars · +bug"));
    }

    #[test]
    fn card_rename_appends_tag_delta_to_sub() {
        let before = tagged_card_snap("# Old title", "col-a", 7, &[]);
        let after = tagged_card_snap("# New title", "col-a", 7, &["bug"]);
        let s = derive_summary(&entry("update", "card", Some(before), Some(after)));
        assert_eq!(s.headline, "Renamed card to «New title»");
        assert_eq!(s.sub.as_deref(), Some("was «Old title» · Card #7 · +bug"));
    }

    #[test]
    fn card_move_in_the_same_update_still_reports_the_tag_delta() {
        let before = tagged_card_snap("# Title", "col-a", 7, &[]);
        let after = tagged_card_snap("# Title", "col-b", 7, &["bug"]);
        let s = derive_summary(&entry("update", "card", Some(before), Some(after)));
        assert_eq!(s.headline, "Moved card «Title»");
        assert_eq!(s.sub.as_deref(), Some("Card #7 · +bug"));
    }

    #[test]
    fn card_update_tolerates_snapshots_without_a_tags_key() {
        // Rows recorded before tags existed have no `tags` field at all; they
        // must read exactly as they did before, with no phantom tag delta.
        let before = card_snap_untagged("# Title\n\nold", "col-a", 7);
        let after = card_snap_untagged("# Title\n\nold text", "col-a", 7);
        let s = derive_summary(&entry("update", "card", Some(before), Some(after)));
        assert_eq!(s.headline, "Edited card «Title»");
        assert_eq!(s.sub.as_deref(), Some("Card #7 · +5 chars"));
    }

    #[test]
    fn card_move_uses_after_title() {
        let before = card_snap("# Title", "col-a", 7);
        let after = card_snap("# Title", "col-b", 7);
        let s = derive_summary(&entry("move", "card", Some(before), Some(after)));
        assert_eq!(s.headline, "Moved card «Title»");
        assert_eq!(s.sub.as_deref(), Some("Card #7"));
    }

    #[test]
    fn card_delete_uses_before_title() {
        let before = card_snap("# Doomed", "col-a", 7);
        let s = derive_summary(&entry("delete", "card", Some(before), None));
        assert_eq!(s.headline, "Deleted card «Doomed»");
        assert_eq!(s.sub.as_deref(), Some("Card #7"));
    }

    #[test]
    fn card_baseline_uses_after() {
        let after = card_snap("# Existing", "col-a", 1);
        let s = derive_summary(&entry("baseline", "card", None, Some(after)));
        assert_eq!(s.headline, "Card «Existing»");
        assert_eq!(s.sub.as_deref(), Some("Card #1"));
    }

    #[test]
    fn card_restore_uses_after() {
        let after = card_snap("# Back", "col-a", 1);
        let s = derive_summary(&entry("restore", "card", None, Some(after)));
        assert_eq!(s.headline, "Restored card «Back»");
    }

    #[test]
    fn card_title_truncates_long_first_line() {
        let body = format!("# {}", "x".repeat(100));
        let s = derive_summary(&entry(
            "create",
            "card",
            None,
            Some(card_snap(&body, "col-a", 1)),
        ));
        // 60 char window: 59 chars + ellipsis.
        let inside = &s.headline["Created card «".len()..s.headline.len() - "»".len()];
        let chars = inside.chars().count();
        assert_eq!(chars, 60, "title chars: got {chars} for {inside:?}");
        assert!(inside.ends_with('…'));
    }

    // ── board ────────────────────────────────────────────────────────

    #[test]
    fn board_rename_includes_was_in_sub() {
        let s = derive_summary(&entry(
            "update",
            "board",
            Some(board_snap("old-name")),
            Some(board_snap("new-name")),
        ));
        assert_eq!(s.headline, "Renamed board to «new-name»");
        assert_eq!(s.sub.as_deref(), Some("was «old-name»"));
    }

    #[test]
    fn board_create_after_only() {
        let s = derive_summary(&entry(
            "create",
            "board",
            None,
            Some(board_snap("project-alpha")),
        ));
        assert_eq!(s.headline, "Created board «project-alpha»");
        assert!(s.sub.is_none());
    }

    #[test]
    fn board_delete_uses_before() {
        let s = derive_summary(&entry(
            "delete",
            "board",
            Some(board_snap("departed")),
            None,
        ));
        assert_eq!(s.headline, "Deleted board «departed»");
    }

    #[test]
    fn board_baseline() {
        let s = derive_summary(&entry(
            "baseline",
            "board",
            None,
            Some(board_snap("seeded")),
        ));
        assert_eq!(s.headline, "Board «seeded»");
    }

    // ── column ───────────────────────────────────────────────────────

    #[test]
    fn column_rename_includes_was() {
        let s = derive_summary(&entry(
            "update",
            "column",
            Some(col_snap("Todo", 0)),
            Some(col_snap("Backlog", 0)),
        ));
        assert_eq!(s.headline, "Renamed column to «Backlog»");
        assert_eq!(s.sub.as_deref(), Some("was «Todo»"));
    }

    #[test]
    fn column_position_change_only_says_reordered() {
        let s = derive_summary(&entry(
            "update",
            "column",
            Some(col_snap("Todo", 0)),
            Some(col_snap("Todo", 2)),
        ));
        assert_eq!(s.headline, "Reordered column «Todo»");
        assert!(s.sub.is_none());
    }

    #[test]
    fn column_create_after_only() {
        let s = derive_summary(&entry("create", "column", None, Some(col_snap("Done", 3))));
        assert_eq!(s.headline, "Created column «Done»");
    }

    #[test]
    fn column_delete_uses_before() {
        let s = derive_summary(&entry("delete", "column", Some(col_snap("Gone", 1)), None));
        assert_eq!(s.headline, "Deleted column «Gone»");
    }

    // ── card link ────────────────────────────────────────────────────

    fn link_snap(reason: Option<&str>) -> Value {
        json!({
            "id": "link-1",
            "predecessor_id": "card-a",
            "successor_id": "card-b",
            "predecessor_number": 12,
            "successor_number": 15,
            "reason": reason,
            "last_edited_by": null,
            "created_at": "x",
            "updated_at": "x",
        })
    }

    #[test]
    fn link_create_names_both_cards_in_order() {
        let s = derive_summary(&entry("create", "card_link", None, Some(link_snap(None))));
        assert_eq!(s.headline, "Linked #12 \u{2192} #15");
        assert!(s.sub.is_none());
    }

    #[test]
    fn link_create_puts_the_reason_in_the_sub() {
        let s = derive_summary(&entry(
            "create",
            "card_link",
            None,
            Some(link_snap(Some("needs the API first"))),
        ));
        assert_eq!(s.sub.as_deref(), Some("«needs the API first»"));
    }

    #[test]
    fn link_delete_reads_the_before_snapshot() {
        let s = derive_summary(&entry("delete", "card_link", Some(link_snap(None)), None));
        assert_eq!(s.headline, "Unlinked #12 \u{2192} #15");
    }

    #[test]
    fn link_reason_change_shows_new_and_old() {
        let s = derive_summary(&entry(
            "update",
            "card_link",
            Some(link_snap(Some("old why"))),
            Some(link_snap(Some("new why"))),
        ));
        assert_eq!(s.headline, "Changed reason on #12 \u{2192} #15");
        assert_eq!(s.sub.as_deref(), Some("«new why» · was «old why»"));
    }

    #[test]
    fn link_reason_cleared_says_so() {
        let s = derive_summary(&entry(
            "update",
            "card_link",
            Some(link_snap(Some("old why"))),
            Some(link_snap(None)),
        ));
        assert_eq!(s.headline, "Cleared reason on #12 \u{2192} #15");
        assert_eq!(s.sub.as_deref(), Some("was «old why»"));
    }

    #[test]
    fn link_without_numbers_falls_back_to_ids() {
        let snap = json!({ "predecessor_id": "card-aaaaaaaaaaaaaaaa", "successor_id": "card-b" });
        let s = derive_summary(&entry("create", "card_link", None, Some(snap)));
        assert_eq!(s.headline, "Linked card-aaaaaa… \u{2192} card-b");
    }

    // ── defensive ────────────────────────────────────────────────────

    #[test]
    fn unknown_action_falls_through() {
        let s = derive_summary(&entry("magic", "card", None, None));
        assert_eq!(s.headline, "magic card");
    }

    #[test]
    fn missing_after_for_create_falls_back_to_entity_id() {
        let s = derive_summary(&entry("create", "card", None, None));
        // No body → fallback to truncated id.
        assert!(s.headline.starts_with("Created card «entity-1"));
    }
}
