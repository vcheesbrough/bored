//! What a log line may carry out of the process — the redaction policy.
//!
//! **This module is a decision, not a convenience** (card #366). Every log
//! line the backend writes leaves the process: Alloy collects stdout into
//! Loki, and after #415 the same records also travel over OTLP. That store
//! has its own access control and its own retention, neither of which is the
//! database's. So the rule, taken from the `observability` skill §6, is:
//!
//! * **identifiers, never the content they identify** — a table name, a
//!   record id, an index or field name from our own schema may be logged; a
//!   card body, tag, link reason or any other value a user typed may not;
//! * **query names and call sites, never query text or bound values** — a
//!   SurrealQL parse error quotes the query back, so it is reduced to its
//!   variant name;
//! * **URLs keep scheme, host, port and path, never the query string or
//!   userinfo** — a query string is where OAuth codes, tokens and search
//!   terms travel.
//!
//! The functions here do the reducing, and they are the *only* way a driver
//! or HTTP-client error is turned into text for a log line. Redacting in one
//! place — rather than trusting each call site to remember — is what makes
//! the policy enforceable: `ApiError::Internal` cannot even be constructed
//! from an arbitrary message any more (see `error.rs`).
//!
//! AGENTS.md has no observability section yet; #415 creates one and carries
//! this rule into it.

use std::fmt::Write as _;

/// The longest identifier we are willing to echo. Real identifiers in bored
/// are short — a ULID is 26 characters, the longest table or index name is
/// under 20 — so anything longer is not one of ours and is dropped.
const MAX_IDENT_LEN: usize = 64;

/// Returned in place of anything that failed the identifier check. A fixed
/// marker (rather than an empty string) tells the reader something *was*
/// there and was removed on purpose.
pub(crate) const REDACTED: &str = "<redacted>";

/// Pass `candidate` through only if it looks like one of our identifiers:
/// non-empty, at most [`MAX_IDENT_LEN`] characters, and made only of ASCII
/// letters, digits, `_` and `-`.
///
/// That character set covers every table, index, field and record id bored
/// creates (ids are lowercase ULIDs), and excludes the quote, space and
/// punctuation characters every piece of free text needs — so a user value
/// that happens to sit where an identifier was expected cannot survive.
pub(crate) fn identifier(candidate: &str) -> &str {
    // `chars().all(..)` walks every character and stops at the first that
    // fails the predicate; `is_ascii_alphanumeric` rejects non-ASCII letters,
    // which keeps homoglyph tricks out as well.
    let is_identifier = !candidate.is_empty()
        && candidate.len() <= MAX_IDENT_LEN
        && candidate
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if is_identifier { candidate } else { REDACTED }
}

/// Render a SurrealDB record id as `table:id`, keeping each half only if it
/// passes [`identifier`].
///
/// Takes the driver's own `Thing` (table + id) where the error carries one,
/// so nothing has to be parsed back out of message text. Compound ids —
/// arrays, objects, ranges — are never used by bored, so they are reduced to
/// the marker rather than rendered (their rendering would include values).
pub(crate) fn thing(thing: &surrealdb::sql::Thing) -> String {
    // `Id` is the driver's enum of record-id shapes. Only the two plain
    // scalar shapes are rendered; `match` forces a decision for every other
    // shape the driver has today via the wildcard arm.
    let id = match &thing.id {
        surrealdb::sql::Id::String(s) => identifier(s).to_string(),
        surrealdb::sql::Id::Number(n) => n.to_string(),
        _ => REDACTED.to_string(),
    };
    format!("{}:{id}", identifier(&thing.tb))
}

/// As [`thing`], for the variants where the driver has already formatted the
/// record id into a `String` (`FieldCheck`, `FieldValue`).
///
/// SurrealDB escapes an id it cannot print bare with `⟨…⟩` or backticks; such
/// an id fails [`identifier`] and becomes the marker, which is the intent.
pub(crate) fn record_string(record: &str) -> String {
    // `split_once` returns the text either side of the *first* `:` — a table
    // name never contains one — or `None` if there is no colon at all.
    match record.split_once(':') {
        Some((table, id)) => format!("{}:{}", identifier(table), identifier(id)),
        None => REDACTED.to_string(),
    }
}

/// The variant path of an error, read from its `Debug` rendering — e.g.
/// `Db::IndexExists` or `Api::Query` — and nothing after it.
///
/// Why `Debug`: the driver's inner error enum has ~250 variants and is marked
/// `#[non_exhaustive]`, so naming each one in a `match` is neither possible to
/// keep complete nor worth it. Derived `Debug` always starts with the variant
/// name, and a variant name is an identifier we wrote into the driver, not
/// data. The walk stops at the first character that is not part of a
/// `Name(` chain — i.e. before the first quote, brace or space — so no field
/// value can be read, however the variant is shaped:
///
/// * `Db(IndexExists { thing: …, value: "'secret'" })` → `Db::IndexExists`
/// * `Api(Query("…"))` → `Api::Query`
/// * `Db(QueryTimedout)` → `Db::QueryTimedout`
pub(crate) fn variant_path(debug: &str) -> String {
    let mut path = String::new();
    // `rest` is a moving window over the unread part of the Debug string.
    let mut rest = debug;
    // Two levels are enough (`surrealdb::Error` → the inner enum); a third
    // would start reading into field types, which are not variant names.
    for _ in 0..2 {
        // Length of the leading run of identifier characters.
        let len = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        if len == 0 {
            break;
        }
        if !path.is_empty() {
            path.push_str("::");
        }
        path.push_str(&rest[..len]);
        // Only descend if the name is immediately followed by `(` — a tuple
        // variant wrapping another enum. Anything else (`{`, ` `, end of
        // string) means we have reached the innermost variant name.
        match rest[len..].strip_prefix('(') {
            Some(inner) => rest = inner,
            None => break,
        }
    }
    if path.is_empty() {
        REDACTED.to_string()
    } else {
        path
    }
}

/// A URL reduced to what the policy allows: scheme, host, port and path.
/// Userinfo, query string and fragment are dropped.
///
/// A string that does not parse as a URL is not echoed at all — we cannot
/// tell which part of it would have been the query.
pub(crate) fn url(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(parsed) => url_parts(&parsed),
        Err(_) => REDACTED.to_string(),
    }
}

/// [`url`] for an already-parsed `Url` (reqwest hands us one).
pub(crate) fn url_parts(parsed: &url::Url) -> String {
    // `host_str` is `None` for URLs such as `mailto:` that have no host; the
    // scheme alone is still worth saying.
    let mut out = format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or(""));
    // `port()` is `None` when the URL uses its scheme's default port, so an
    // ordinary `https://` URL does not grow a redundant `:443`.
    if let Some(port) = parsed.port() {
        // `write!` into a `String` cannot fail; `let _ =` discards the
        // always-`Ok` result without an `unwrap`.
        let _ = write!(out, ":{port}");
    }
    out.push_str(parsed.path());
    out
}

/// Describe an outbound HTTP failure without letting it quote anything we
/// did not choose to show.
///
/// reqwest's own `Display` embeds the full request URL, query string
/// included, and a JSON-decode failure quotes the offending bit of the
/// *response body* — which, for a token endpoint, can be a token. So the
/// description is assembled from parts:
///
/// * the failure kind (`connect`, `timeout`, `status`, `decode`, …);
/// * the HTTP status, where there is one;
/// * the URL, through [`url_parts`];
/// * for every kind **except** `decode` and `body`, the error's source chain
///   (`dns error`, `Connection refused`, `invalid peer certificate: …`),
///   which is the transport's own wording and carries no payload.
pub(crate) fn http_error(error: &reqwest::Error) -> String {
    // One label per failure kind. The checks are ordered from most to least
    // specific: a timeout during connect reports both `is_timeout` and
    // `is_connect`, and "timeout" is the more useful of the two.
    let kind = if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_status() {
        "status"
    } else if error.is_decode() {
        "decode"
    } else if error.is_body() {
        "body"
    } else if error.is_redirect() {
        "redirect"
    } else if error.is_builder() {
        "builder"
    } else if error.is_request() {
        "request"
    } else {
        "other"
    };
    let mut out = kind.to_string();
    if let Some(status) = error.status() {
        let _ = write!(out, " status={}", status.as_u16());
    }
    if let Some(url) = error.url() {
        let _ = write!(out, " url={}", url_parts(url));
    }
    // A decode or body error's source is a serde / hyper error about the
    // response *content*; everything else's source chain is about the
    // transport and safe to show.
    if kind != "decode" && kind != "body" {
        // `source()` walks one level down the chain of causes; the loop
        // follows it to the bottom. `std::error::Error` must be in scope for
        // the method, hence the fully qualified call.
        let mut source = std::error::Error::source(error);
        while let Some(cause) = source {
            let _ = write!(out, ": {cause}");
            source = cause.source();
        }
    }
    out
}

/// An OAuth `error` code as the identity provider sent it back on the
/// callback redirect — or the marker, if it is not shaped like one.
///
/// RFC 6749 §4.1.2.1 codes are short ASCII words (`access_denied`,
/// `invalid_scope`, …). The parameter arrives in a query string anyone can
/// type, so only something with that shape is logged.
pub(crate) fn oauth_error_code(code: &str) -> &str {
    identifier(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_pass_and_free_text_does_not() {
        assert_eq!(identifier("board_name_unique"), "board_name_unique");
        assert_eq!(
            identifier("01m2x21gr2dpztsd3p77skp2gt"),
            "01m2x21gr2dpztsd3p77skp2gt"
        );
        // Each of these is something a user could type into a card.
        for text in [
            "a card body",
            "it's",
            "",
            "x".repeat(65).as_str(),
            "naïve",
            "a`b",
        ] {
            assert_eq!(identifier(text), REDACTED, "{text:?} must not pass");
        }
    }

    #[test]
    fn record_strings_keep_table_and_plain_ids_only() {
        assert_eq!(record_string("cards:c1"), "cards:c1");
        assert_eq!(record_string("cards:⟨a card body⟩"), "cards:<redacted>");
        assert_eq!(record_string("no colon here"), REDACTED);
    }

    #[test]
    fn variant_path_stops_before_any_field_value() {
        assert_eq!(
            variant_path(r#"Db(IndexExists { thing: Thing { tb: "boards" }, value: "'secret'" })"#),
            "Db::IndexExists"
        );
        assert_eq!(variant_path(r#"Api(Query("SELEKT secret"))"#), "Api::Query");
        assert_eq!(variant_path("Db(QueryTimedout)"), "Db::QueryTimedout");
        // A tuple variant wrapping a string: the walk must not treat the
        // string's content as a third-level name.
        assert_eq!(variant_path(r#"Db(Thrown("secret"))"#), "Db::Thrown");
        assert_eq!(variant_path(r#""starts with a quote""#), REDACTED);
    }

    #[test]
    fn urls_lose_query_fragment_and_userinfo() {
        assert_eq!(
            url("https://user:pw@auth.example/application/o/token/?code=SECRET#frag"),
            "https://auth.example/application/o/token/"
        );
        assert_eq!(
            url("http://mock:8080/default/jwks"),
            "http://mock:8080/default/jwks"
        );
        assert_eq!(url("not a url ?code=SECRET"), REDACTED);
    }

    #[test]
    fn oauth_error_codes_pass_but_prose_does_not() {
        assert_eq!(oauth_error_code("access_denied"), "access_denied");
        assert_eq!(oauth_error_code("<script>alert(1)</script>"), REDACTED);
    }
}
