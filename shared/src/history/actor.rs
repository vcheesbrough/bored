//! Friendly actor labels for audit log rows.
//!
//! Rules in priority order (first match wins):
//!
//! 1. `me_name` matches `actor_display_name` (case-insensitive) → [`YOU`].
//! 2. `actor_sub == "anonymous"` (auth-disabled / no IdP) → [`SOMEONE`].
//! 3. `actor_display_name == actor_sub` AND the sub looks like an opaque id
//!    (≥ 16 alphanumeric chars — ULID, sha256 hex, etc.) → [`EARLIER_COLLABORATOR`].
//!    This is the baseline-backfill case: pre-audit rows have only `sub`
//!    and we duplicated it into `actor_display_name`, so the row would
//!    otherwise render an unreadable hash.
//! 4. Otherwise → `actor_display_name` verbatim (already a sensible label
//!    written by the audit recorder from `preferred_username` / `email`).

/// Label for the currently-signed-in user.
pub const YOU: &str = "You";

/// Anonymous label used when no IdP is configured (dev / tests) or when
/// the IdP couldn't supply any usable display name.
pub const SOMEONE: &str = "Someone";

/// Label for actors whose only identifier is an opaque sub (e.g. baseline
/// rows backfilled from `last_edited_by`).
pub const EARLIER_COLLABORATOR: &str = "Earlier collaborator";

/// Apply the rules above to produce the row's actor label.
///
/// `me_name` is the value of `UserInfo.name` from `GET /api/me`. Pass
/// `None` when the current user is unknown (e.g. anonymous mode); a
/// `Some("")` is treated the same as `None` so callers don't have to
/// special-case empty strings.
pub fn label_actor(actor_sub: &str, actor_display_name: &str, me_name: Option<&str>) -> String {
    if let Some(me) = me_name.filter(|s| !s.is_empty()) {
        if actor_display_name.eq_ignore_ascii_case(me) {
            return YOU.to_string();
        }
    }
    if actor_sub == "anonymous" {
        return SOMEONE.to_string();
    }
    if actor_display_name == actor_sub && looks_like_opaque_id(actor_sub) {
        return EARLIER_COLLABORATOR.to_string();
    }
    actor_display_name.to_string()
}

/// Heuristic: 16+ ASCII alphanumeric chars (no spaces, punctuation, accents)
/// → looks like a machine identifier rather than a human label. Catches
/// 26-char ULIDs, 32-char hex hashes, 64-char sha256 hex, etc.
fn looks_like_opaque_id(s: &str) -> bool {
    let len = s.chars().count();
    len >= 16 && s.chars().all(|c| c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_me_name_case_insensitive() {
        assert_eq!(label_actor("sub-foo", "Vincent", Some("vincent")), "You");
        assert_eq!(label_actor("sub-foo", "VINCENT", Some("vincent")), "You");
        assert_eq!(label_actor("sub-foo", "vincent", Some("Vincent")), "You");
    }

    #[test]
    fn anonymous_becomes_someone_regardless_of_me() {
        assert_eq!(
            label_actor("anonymous", "anonymous", Some("test-user")),
            "Someone"
        );
        assert_eq!(label_actor("anonymous", "anonymous", None), "Someone");
    }

    #[test]
    fn ulid_sub_with_matching_display_becomes_earlier_collaborator() {
        let s = label_actor(
            "01kphf6gr4kt1wzabj80mm4pme",
            "01kphf6gr4kt1wzabj80mm4pme",
            None,
        );
        assert_eq!(s, "Earlier collaborator");
    }

    #[test]
    fn long_hex_sub_with_matching_display_becomes_earlier_collaborator() {
        // 64-char hex (Authentik internal sub before display is set).
        let sub = "68bef8840ad5ba6ebae9f94f1b9d1c7ae99a6e7c75ddbb8d389394c2d4f66c10";
        assert_eq!(label_actor(sub, sub, None), "Earlier collaborator");
    }

    #[test]
    fn human_display_name_passes_through() {
        assert_eq!(
            label_actor("sub-1234567890123456", "Alice", Some("Bob")),
            "Alice"
        );
    }

    #[test]
    fn short_sub_not_treated_as_opaque() {
        // `alice` looks like a real username; even when display==sub we keep it.
        assert_eq!(label_actor("alice", "alice", None), "alice");
    }

    #[test]
    fn empty_me_name_doesnt_match() {
        assert_eq!(label_actor("sub-x", "anyone", Some("")), "anyone");
    }

    #[test]
    fn me_name_match_takes_priority_over_anonymous_rule() {
        // Rule ordering: when `me_name` matches `actor_display_name`, the
        // first («You») rule fires before the second (anonymous → «Someone»)
        // rule even when `actor_sub == "anonymous"`. Both are the same actor
        // in that case so the result is correct.
        assert_eq!(
            label_actor("anonymous", "anonymous", Some("anonymous")),
            "You"
        );
    }
}
