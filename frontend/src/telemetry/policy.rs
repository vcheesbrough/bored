//! What the exporter does with each answer from the ingest (card #416).
//!
//! Pure decision logic — status codes and counters in, verdicts out, no
//! browser API — so every rule of `client-export.md` *Failure is normal* is
//! unit-tested on the host:
//!
//! - **Retry only what is retryable**: `429`, `502`, `503`, `504` (and a
//!   request that never got an answer at all), with exponential backoff and
//!   jitter, honouring `Retry-After`, a capped number of attempts, then drop.
//!   The collector's `503 not ready` (with `Retry-After: 5`) is this case.
//! - **Every other 4xx/5xx is permanent** for that payload — `400`, `403`,
//!   `413`, `500` too: drop it.
//! - **`401` gets one refresh.** Every refusal of the token is `401`, so the
//!   code cannot tell an expired token from a misconfigured provider: drop the
//!   batch, fetch a fresh token, and if the next answer is `401` too, stop for
//!   the session.
//! - **Assume you are a crowd**: after repeated failed batches, give up for the
//!   rest of the session instead of retrying in lockstep with every other tab.
//! - **Log transitions, not batches**: the verdicts carry at most one
//!   [`Transition`] — first failure, back to working, gave up — which is all
//!   the console ever hears about.

/// Attempts per batch before it is dropped (the first send counts as one).
pub const MAX_ATTEMPTS: u32 = 4;

/// Batches dropped in a row — refused, or retried to exhaustion — before the
/// exporter gives up for the session.
pub const GIVE_UP_AFTER_FAILED_BATCHES: u32 = 3;

/// First backoff ceiling. Doubles per attempt up to [`MAX_BACKOFF_MS`].
const BASE_BACKOFF_MS: u32 = 1_000;

/// Longest the exporter waits between attempts on its own account.
const MAX_BACKOFF_MS: u32 = 30_000;

/// Longest `Retry-After` honoured. A server asking for more is still obeyed up
/// to this, and by then the batch will usually have been dropped anyway.
const MAX_RETRY_AFTER_MS: u32 = 300_000;

/// The four ways an answer can be read. `0` stands for "no HTTP answer at
/// all" — a refused connection, a blocker cancelling the request, DNS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusClass {
    Accepted,
    Retryable,
    Unauthorized,
    Permanent,
}

/// Map an HTTP status to what it means for the exporter. Exactly OTLP's list:
/// nothing outside `429`/`502`/`503`/`504` (and "no answer") is retried.
pub fn classify(status: u16) -> StatusClass {
    match status {
        200..=299 => StatusClass::Accepted,
        0 | 429 | 502 | 503 | 504 => StatusClass::Retryable,
        401 => StatusClass::Unauthorized,
        _ => StatusClass::Permanent,
    }
}

/// The wait before retry number `attempt` (1-based: the wait after the first
/// failure is `attempt == 1`).
///
/// "Equal jitter": half the ceiling is fixed, the other half random. The fixed
/// half keeps a retry from firing immediately; the random half spreads a
/// crowd of tabs that all failed at the same moment. `unit_random` is a value
/// in `[0, 1)` supplied by the caller, which is what makes this testable.
pub fn backoff_ms(attempt: u32, unit_random: f64) -> u32 {
    // `checked_shl` returns `None` instead of overflowing when the shift is
    // too large; either way the ceiling clamps to the maximum.
    let exponential = 1u32
        .checked_shl(attempt.saturating_sub(1))
        .and_then(|factor| BASE_BACKOFF_MS.checked_mul(factor))
        .unwrap_or(MAX_BACKOFF_MS);
    let ceiling = exponential.min(MAX_BACKOFF_MS);
    let half = ceiling / 2;
    // `clamp` guards against a caller handing in a value outside [0, 1).
    let random = unit_random.clamp(0.0, 1.0);
    half + (f64::from(ceiling - half) * random) as u32
}

/// Parse a `Retry-After` header given in delta-seconds, as milliseconds,
/// capped. The HTTP-date form is not supported (the ingest never sends it);
/// such a value is ignored and the ordinary backoff applies.
pub fn retry_after_ms(header: Option<&str>) -> Option<u32> {
    let seconds: u32 = header?.trim().parse().ok()?;
    Some(seconds.saturating_mul(1_000).min(MAX_RETRY_AFTER_MS))
}

/// Why export stopped for the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// A fresh token was refused too: a provider or ingest misconfiguration
    /// that retrying will not fix.
    Unauthorized,
    /// Several batches in a row failed.
    RepeatedFailure,
    /// The product would not hand out a token (`/api/telemetry/token`
    /// answered this status) — no session, or client telemetry switched off
    /// server-side.
    TokenUnavailable(u16),
}

/// A change worth one console line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// Exporting was working (or had not been tried) and has just failed.
    FirstFailure { status: u16 },
    /// Exporting had been failing and has just succeeded.
    Recovered,
    /// Export is stopped for the rest of the session.
    GaveUp(StopReason),
}

/// What to do with the batch that was just sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Accepted; forget it.
    Sent,
    /// Keep it and send it again after `delay_ms`.
    Retry { delay_ms: u32 },
    /// Drop it. With `refresh_token`, discard the cached token first, so the
    /// next batch is sent with a fresh one — the one refresh a `401` gets.
    Dropped { refresh_token: bool },
    /// Drop it and stop exporting for the session.
    Stop(StopReason),
}

/// The exporter's memory between answers.
#[derive(Debug, Default)]
pub struct ExportPolicy {
    /// `Some` once export has stopped for the session.
    stopped: Option<StopReason>,
    /// A `401` has been answered with a token refresh, and no batch has been
    /// accepted since. The next `401` is the second in a row.
    refreshed_after_401: bool,
    /// Batches dropped in a row (refused, or retried to exhaustion).
    failed_batches: u32,
    /// Whether the last outcome was a failure — the state the transitions are
    /// computed against.
    failing: bool,
}

impl ExportPolicy {
    pub fn stopped(&self) -> Option<StopReason> {
        self.stopped
    }

    /// Read one answer. `attempt` is which send of this batch it answers
    /// (1-based); `retry_after` is the raw `Retry-After` header, if any;
    /// `unit_random` feeds the jitter.
    pub fn on_response(
        &mut self,
        status: u16,
        attempt: u32,
        retry_after: Option<&str>,
        unit_random: f64,
    ) -> (Verdict, Option<Transition>) {
        // Once stopped, nothing changes that — a late answer to a request that
        // was in flight when export stopped is simply dropped.
        if let Some(reason) = self.stopped {
            return (Verdict::Stop(reason), None);
        }
        match classify(status) {
            StatusClass::Accepted => {
                self.failed_batches = 0;
                self.refreshed_after_401 = false;
                let transition =
                    std::mem::replace(&mut self.failing, false).then_some(Transition::Recovered);
                (Verdict::Sent, transition)
            }
            StatusClass::Unauthorized => {
                if self.refreshed_after_401 {
                    // The second `401` in a row, on a token fetched after the
                    // first: not going to change. Stop for the session.
                    return self.stop(StopReason::Unauthorized);
                }
                self.refreshed_after_401 = true;
                let transition = self.fail(status);
                (
                    Verdict::Dropped {
                        refresh_token: true,
                    },
                    transition,
                )
            }
            StatusClass::Retryable if attempt < MAX_ATTEMPTS => {
                let backoff = backoff_ms(attempt, unit_random);
                // Honour `Retry-After` as a floor: never sooner than the
                // server asked, never sooner than our own backoff either.
                let delay_ms =
                    retry_after_ms(retry_after).map_or(backoff, |after| after.max(backoff));
                let transition = self.fail(status);
                (Verdict::Retry { delay_ms }, transition)
            }
            // Retryable but out of attempts, or permanent: the batch goes.
            StatusClass::Retryable | StatusClass::Permanent => self.drop_batch(status),
        }
    }

    /// Read a failure to obtain a token from the product. A definite "no"
    /// (`401` no session, `403` not a browser session, `404` telemetry off
    /// server-side) stops export; anything else — the server unreachable,
    /// a `5xx` — is transient and costs only this tick.
    pub fn on_token_failure(&mut self, status: u16) -> Option<Transition> {
        if self.stopped.is_some() {
            return None;
        }
        match status {
            401 | 403 | 404 => self.stop(StopReason::TokenUnavailable(status)).1,
            _ => self.fail(status),
        }
    }

    /// Stop for a reason decided outside the ingest's answers.
    pub fn stop(&mut self, reason: StopReason) -> (Verdict, Option<Transition>) {
        self.stopped = Some(reason);
        (Verdict::Stop(reason), Some(Transition::GaveUp(reason)))
    }

    /// Mark a failure; the transition fires only on the first of a run.
    fn fail(&mut self, status: u16) -> Option<Transition> {
        (!std::mem::replace(&mut self.failing, true)).then_some(Transition::FirstFailure { status })
    }

    fn drop_batch(&mut self, status: u16) -> (Verdict, Option<Transition>) {
        self.failed_batches += 1;
        if self.failed_batches >= GIVE_UP_AFTER_FAILED_BATCHES {
            return self.stop(StopReason::RepeatedFailure);
        }
        let transition = self.fail(status);
        (
            Verdict::Dropped {
                refresh_token: false,
            },
            transition,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exact_status_mapping() {
        // Written out as a table rather than derived from `classify`'s arms, so
        // the test is an independent statement of client-export.md.
        let table: &[(u16, StatusClass)] = &[
            (200, StatusClass::Accepted),
            (204, StatusClass::Accepted),
            (0, StatusClass::Retryable),
            (429, StatusClass::Retryable),
            (502, StatusClass::Retryable),
            (503, StatusClass::Retryable),
            (504, StatusClass::Retryable),
            (401, StatusClass::Unauthorized),
            (400, StatusClass::Permanent),
            (403, StatusClass::Permanent),
            (404, StatusClass::Permanent),
            (408, StatusClass::Permanent),
            (413, StatusClass::Permanent),
            (500, StatusClass::Permanent),
            (501, StatusClass::Permanent),
            (505, StatusClass::Permanent),
        ];
        for &(status, expected) in table {
            assert_eq!(classify(status), expected, "status {status}");
        }
    }

    #[test]
    fn backoff_grows_then_caps_and_stays_within_its_jitter_band() {
        // Bounds computed by hand: ceiling 1s, 2s, 4s, 8s, 16s, 30s (capped).
        let ceilings = [1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000];
        for (index, ceiling) in ceilings.iter().enumerate() {
            let attempt = index as u32 + 1;
            let low = backoff_ms(attempt, 0.0);
            let high = backoff_ms(attempt, 0.999_999);
            assert_eq!(low, ceiling / 2, "attempt {attempt} lower bound");
            assert!(
                high <= *ceiling && high >= ceiling - 1,
                "attempt {attempt} upper bound"
            );
        }
        // Absurd attempt numbers must not overflow.
        assert_eq!(backoff_ms(u32::MAX, 0.0), 15_000);
    }

    #[test]
    fn jitter_actually_spreads_the_delay() {
        // A constant-returning implementation fails this whatever its formula.
        let spread: std::collections::HashSet<u32> = (0..10)
            .map(|step| backoff_ms(3, f64::from(step) / 10.0))
            .collect();
        assert!(spread.len() >= 9, "jitter produced {spread:?}");
    }

    #[test]
    fn retry_after_is_parsed_as_seconds_and_capped() {
        assert_eq!(retry_after_ms(Some("5")), Some(5_000));
        assert_eq!(retry_after_ms(Some(" 12 ")), Some(12_000));
        assert_eq!(retry_after_ms(Some("100000")), Some(300_000));
        assert_eq!(retry_after_ms(Some("Wed, 21 Oct 2015 07:28:00 GMT")), None);
        assert_eq!(retry_after_ms(None), None);
    }

    #[test]
    fn a_retryable_answer_retries_with_retry_after_as_a_floor() {
        let mut policy = ExportPolicy::default();
        let (verdict, transition) = policy.on_response(503, 1, Some("5"), 0.0);
        // Backoff for attempt 1 at random 0 is 500 ms; Retry-After wins.
        assert_eq!(verdict, Verdict::Retry { delay_ms: 5_000 });
        assert_eq!(transition, Some(Transition::FirstFailure { status: 503 }));

        // A Retry-After shorter than our own backoff does not shorten it.
        let (verdict, _) = policy.on_response(429, 3, Some("1"), 0.0);
        assert_eq!(verdict, Verdict::Retry { delay_ms: 2_000 });
    }

    #[test]
    fn retries_stop_at_the_attempt_cap_and_the_batch_is_dropped() {
        let mut policy = ExportPolicy::default();
        for attempt in 1..MAX_ATTEMPTS {
            assert!(matches!(
                policy.on_response(502, attempt, None, 0.5).0,
                Verdict::Retry { .. }
            ));
        }
        assert_eq!(
            policy.on_response(502, MAX_ATTEMPTS, None, 0.5).0,
            Verdict::Dropped {
                refresh_token: false
            }
        );
    }

    #[test]
    fn permanent_answers_drop_without_retrying() {
        for status in [400, 403, 413, 500] {
            let mut policy = ExportPolicy::default();
            assert_eq!(
                policy.on_response(status, 1, Some("5"), 0.5).0,
                Verdict::Dropped {
                    refresh_token: false
                },
                "status {status}"
            );
        }
    }

    #[test]
    fn a_401_gets_exactly_one_refresh_then_stops() {
        let mut policy = ExportPolicy::default();
        assert_eq!(
            policy.on_response(401, 1, None, 0.5).0,
            Verdict::Dropped {
                refresh_token: true
            }
        );
        let (verdict, transition) = policy.on_response(401, 1, None, 0.5);
        assert_eq!(verdict, Verdict::Stop(StopReason::Unauthorized));
        assert_eq!(
            transition,
            Some(Transition::GaveUp(StopReason::Unauthorized))
        );
        // Stopped is final.
        assert_eq!(
            policy.on_response(200, 1, None, 0.5).0,
            Verdict::Stop(StopReason::Unauthorized)
        );
        assert_eq!(policy.stopped(), Some(StopReason::Unauthorized));
    }

    #[test]
    fn a_success_between_401s_restores_the_refresh() {
        // An access token expiring every quarter hour is normal: each expiry
        // may cost one 401, and must not add up to "second 401 in a row".
        let mut policy = ExportPolicy::default();
        let _ = policy.on_response(401, 1, None, 0.5);
        let _ = policy.on_response(200, 1, None, 0.5);
        assert_eq!(
            policy.on_response(401, 1, None, 0.5).0,
            Verdict::Dropped {
                refresh_token: true
            }
        );
    }

    #[test]
    fn repeated_failed_batches_give_up_for_the_session() {
        let mut policy = ExportPolicy::default();
        for _ in 1..GIVE_UP_AFTER_FAILED_BATCHES {
            assert!(matches!(
                policy.on_response(400, 1, None, 0.5).0,
                Verdict::Dropped { .. }
            ));
        }
        assert_eq!(
            policy.on_response(400, 1, None, 0.5),
            (
                Verdict::Stop(StopReason::RepeatedFailure),
                Some(Transition::GaveUp(StopReason::RepeatedFailure))
            )
        );
    }

    #[test]
    fn transitions_fire_once_per_change_not_per_batch() {
        let mut policy = ExportPolicy::default();
        assert_eq!(
            policy.on_response(503, 1, None, 0.5).1,
            Some(Transition::FirstFailure { status: 503 })
        );
        // Still failing: silent.
        assert_eq!(policy.on_response(503, 2, None, 0.5).1, None);
        // Back to working: one line.
        assert_eq!(
            policy.on_response(200, 3, None, 0.5).1,
            Some(Transition::Recovered)
        );
        // Still working: silent.
        assert_eq!(policy.on_response(200, 1, None, 0.5).1, None);
    }

    #[test]
    fn a_definite_token_refusal_stops_and_a_transient_one_does_not() {
        for status in [401, 403, 404] {
            let mut policy = ExportPolicy::default();
            assert_eq!(
                policy.on_token_failure(status),
                Some(Transition::GaveUp(StopReason::TokenUnavailable(status)))
            );
            assert!(policy.stopped().is_some());
        }
        let mut policy = ExportPolicy::default();
        assert_eq!(
            policy.on_token_failure(0),
            Some(Transition::FirstFailure { status: 0 })
        );
        assert_eq!(policy.on_token_failure(502), None, "still failing: silent");
        assert!(policy.stopped().is_none());
    }
}
