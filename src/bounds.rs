//! The session meter: wall clock and spend, read by every agent before every model and tool call.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Session-level runtime and spend bounds, shared by the whole tree.
pub(crate) struct Bounds {
    max_time: Option<Duration>,
    max_cost: Option<f64>,
    start: Instant,
    /// USD the session has spent so far.
    spent: Mutex<f64>,
    /// USD per 1M tokens, keyed by the exact model string.
    pricing: HashMap<String, Price>,
    /// Calls that reported no token counts, so a cost bound could not bill them.
    unbilled: AtomicU64,
}

/// USD per 1M tokens. `input`/`output`, never `in`: `in` is a keyword.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Price {
    pub(crate) input: f64,
    pub(crate) output: f64,
}

impl Bounds {
    pub(crate) fn new(max_time: Option<Duration>, max_cost: Option<f64>, pricing: HashMap<String, Price>) -> Self {
        Self { max_time, max_cost, start: Instant::now(), spent: Mutex::new(0.0), pricing, unbilled: AtomicU64::new(0) }
    }

    /// Which bound is blown, or `None` while the session fits both. Time wins when both are blown.
    pub(crate) fn exceeded(&self) -> Option<&'static str> {
        if self.max_time.is_some_and(|max_time| self.start.elapsed() >= max_time) {
            return Some("time");
        }
        if self.max_cost.is_some_and(|max_cost| self.spent() >= max_cost) {
            return Some("cost");
        }
        None
    }

    /// The refusal a turn reports once a bound is blown, or `None` while the session fits.
    pub(crate) fn refusal(&self) -> Option<String> {
        self.exceeded().map(|reason| format!("session budget exceeded: {reason}"))
    }

    /// Whether the client should ask its provider for token counts: only a cost bound needs them.
    pub(crate) fn wants_usage(&self) -> bool {
        self.max_cost.is_some()
    }

    /// Bills `(input, output)` token counts for `model`. A model absent from `pricing` counts $0.
    #[allow(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub(crate) fn charge(&self, model: &str, tokens: (u64, u64)) {
        let Some(price) = self.pricing.get(model) else { return };
        let (input, output) = tokens;
        let cost = input as f64 / 1e6 * price.input + output as f64 / 1e6 * price.output;
        *self.spent.lock().unwrap_or_else(PoisonError::into_inner) += cost;
    }

    /// Counts a call the provider reported no usage for, so a cost bound went unenforced for it.
    pub(crate) fn note_unbilled(&self) {
        self.unbilled.fetch_add(1, Ordering::Relaxed);
    }

    /// How many calls escaped billing that way.
    pub(crate) fn unbilled(&self) -> u64 {
        self.unbilled.load(Ordering::Relaxed)
    }

    /// The running total, for tests and the startup notice.
    pub(crate) fn spent(&self) -> f64 {
        *self.spent.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The same constructor with a supplied `start`, so a test can build a deadline already past.
    #[cfg(test)]
    pub(crate) fn started_at(
        start: Instant,
        max_time: Option<Duration>,
        max_cost: Option<f64>,
        pricing: HashMap<String, Price>,
    ) -> Self {
        Self { max_time, max_cost, start, spent: Mutex::new(0.0), pricing, unbilled: AtomicU64::new(0) }
    }

    /// A bound whose wall clock is already past: what a refused turn sees.
    #[cfg(test)]
    pub(crate) fn expired_for_test() -> Self {
        Self::started_at(Instant::now(), Some(Duration::ZERO), None, HashMap::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn priced() -> HashMap<String, Price> {
        HashMap::from([("m".into(), Price { input: 3.0, output: 15.0 })])
    }

    #[test]
    fn time_exceeds_only_after_the_deadline() {
        let fresh = Bounds::started_at(Instant::now(), Some(Duration::from_secs(3600)), None, HashMap::new());
        assert_eq!(fresh.exceeded(), None);
        let past = Bounds::started_at(Instant::now(), Some(Duration::ZERO), None, HashMap::new());
        assert_eq!(past.exceeded(), Some("time"));
    }

    #[test]
    fn cost_exceeds_at_the_ceiling_and_time_wins_when_both() {
        let bounds = Bounds::started_at(Instant::now(), None, Some(1.0), priced());
        assert_eq!(bounds.exceeded(), None);
        bounds.charge("m", (300_000, 10_000)); // 0.9 + 0.15 = 1.05
        assert_eq!(bounds.exceeded(), Some("cost"));
        let both = Bounds::started_at(Instant::now(), Some(Duration::ZERO), Some(1.0), priced());
        assert_eq!(both.exceeded(), Some("time"));
    }

    #[test]
    fn charge_accumulates_input_and_output_prices() {
        let bounds = Bounds::started_at(Instant::now(), None, None, priced());
        bounds.charge("m", (1_000_000, 1_000_000));
        assert_eq!(bounds.spent(), 18.0, "1M in at $3/M plus 1M out at $15/M");
        bounds.charge("m", (1_000_000, 0));
        assert_eq!(bounds.spent(), 21.0);
    }

    #[test]
    fn an_unpriced_model_costs_nothing() {
        let bounds = Bounds::started_at(Instant::now(), None, None, priced());
        bounds.charge("other", (1_000_000, 1_000_000));
        assert_eq!(bounds.spent(), 0.0);
    }

    #[test]
    fn the_refusal_names_the_bound_and_stays_none_while_the_session_fits() {
        let fresh = Bounds::started_at(Instant::now(), Some(Duration::from_secs(60)), None, HashMap::new());
        assert_eq!(fresh.refusal(), None);
        let past = Bounds::expired_for_test();
        assert_eq!(past.refusal().as_deref(), Some("session budget exceeded: time"));
    }

    #[test]
    fn unbilled_calls_are_counted() {
        let bounds = Bounds::started_at(Instant::now(), None, Some(1.0), priced());
        assert_eq!(bounds.unbilled(), 0);
        bounds.note_unbilled();
        assert_eq!(bounds.unbilled(), 1);
    }

    #[test]
    fn wants_usage_tracks_the_cost_bound_alone() {
        assert!(!Bounds::started_at(Instant::now(), Some(Duration::from_secs(1)), None, HashMap::new()).wants_usage());
        assert!(Bounds::started_at(Instant::now(), None, Some(1.0), HashMap::new()).wants_usage());
    }
}
