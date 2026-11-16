//! The session meter: wall clock and spend, read by every agent before every model and tool call.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Session-level runtime and spend bounds, shared by the whole tree.
pub(crate) struct Bounds {
    max_time: Option<Duration>,
    max_cost: Option<f64>,
    /// Soft ceiling: crossing it warns once and stops nothing.
    warn_cost: Option<f64>,
    start: Instant,
    /// Every dollar and token the session spent: the total, and each agent's share of it.
    ledger: Mutex<Ledger>,
    /// USD per 1M tokens, keyed by the exact model string.
    pricing: HashMap<String, Price>,
    /// Calls that reported no token counts, so a cost bound could not bill them.
    unbilled: AtomicU64,
    /// The warn ceiling fired once; a session that keeps spending does not keep warning.
    warned: AtomicBool,
}

/// What one agent's model calls left behind: the tokens they reported and what they cost.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Spend {
    pub(crate) input: u64,
    pub(crate) output: u64,
    pub(crate) cost: f64,
}

impl Spend {
    pub(crate) fn tokens(&self) -> u64 {
        self.input.saturating_add(self.output)
    }
}

/// The session's running totals, under one lock so tokens, dollars and the per-agent rows can never
/// disagree about the call that just landed.
#[derive(Default)]
struct Ledger {
    input: u64,
    output: u64,
    spent: f64,
    agents: HashMap<usize, Spend>,
}

/// USD per 1M tokens. `input`/`output`, never `in`: `in` is a keyword.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Price {
    pub(crate) input: f64,
    pub(crate) output: f64,
}

impl Bounds {
    pub(crate) fn new(max_time: Option<Duration>, max_cost: Option<f64>, pricing: HashMap<String, Price>) -> Self {
        Self {
            max_time,
            max_cost,
            warn_cost: None,
            start: Instant::now(),
            ledger: Mutex::default(),
            pricing,
            unbilled: AtomicU64::new(0),
            warned: AtomicBool::new(false),
        }
    }

    /// The same meter with a soft ceiling: crossing it warns once, and stops nothing.
    pub(crate) fn with_warn_cost(mut self, warn_cost: Option<f64>) -> Self {
        self.warn_cost = warn_cost;
        self
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
        self.max_cost.is_some() || self.warn_cost.is_some()
    }

    /// Counts `(input, output)` tokens against `agent`'s row and the session total, and bills them
    /// at `model`'s price. A model absent from `pricing` costs $0 and still counts its tokens.
    #[allow(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub(crate) fn account(&self, agent: usize, model: &str, tokens: (u64, u64)) {
        let (input, output) = tokens;
        let cost = self
            .pricing
            .get(model)
            .map_or(0.0, |price| input as f64 / 1e6 * price.input + output as f64 / 1e6 * price.output);
        let mut ledger = self.ledger.lock().unwrap_or_else(PoisonError::into_inner);
        ledger.input = ledger.input.saturating_add(input);
        ledger.output = ledger.output.saturating_add(output);
        ledger.spent += cost;
        let row = ledger.agents.entry(agent).or_default();
        row.input = row.input.saturating_add(input);
        row.output = row.output.saturating_add(output);
        row.cost += cost;
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
        self.ledger.lock().unwrap_or_else(PoisonError::into_inner).spent
    }

    /// Every token the session has been billed for, input and output together.
    pub(crate) fn tokens(&self) -> u64 {
        let ledger = self.ledger.lock().unwrap_or_else(PoisonError::into_inner);
        ledger.input.saturating_add(ledger.output)
    }

    /// Each agent's share, lowest id first so a frontend renders a stable order.
    pub(crate) fn agents(&self) -> Vec<(usize, Spend)> {
        let ledger = self.ledger.lock().unwrap_or_else(PoisonError::into_inner);
        let mut rows: Vec<(usize, Spend)> = ledger.agents.iter().map(|(id, spend)| (*id, *spend)).collect();
        rows.sort_by_key(|(id, _)| *id);
        rows
    }

    /// The soft ceiling's notice, at most once: `Some` on the call that crossed it.
    ///
    /// The latch is claimed before the message is built, so two agents whose calls land together
    /// still produce one warning.
    pub(crate) fn warning(&self) -> Option<String> {
        let limit = self.warn_cost?;
        let spent = self.spent();
        if spent < limit || self.warned.swap(true, Ordering::SeqCst) {
            return None;
        }
        Some(format!("cost warning: session spend ${spent:.2} reached warn_cost ${limit:.2}"))
    }

    /// The same constructor with a supplied `start`, so a test can build a deadline already past.
    #[cfg(test)]
    pub(crate) fn started_at(
        start: Instant,
        max_time: Option<Duration>,
        max_cost: Option<f64>,
        pricing: HashMap<String, Price>,
    ) -> Self {
        Self {
            max_time,
            max_cost,
            warn_cost: None,
            start,
            ledger: Mutex::default(),
            pricing,
            unbilled: AtomicU64::new(0),
            warned: AtomicBool::new(false),
        }
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
        bounds.account(1, "m", (300_000, 10_000)); // 0.9 + 0.15 = 1.05
        assert_eq!(bounds.exceeded(), Some("cost"));
        let both = Bounds::started_at(Instant::now(), Some(Duration::ZERO), Some(1.0), priced());
        assert_eq!(both.exceeded(), Some("time"));
    }

    #[test]
    fn account_accumulates_input_and_output_prices() {
        let bounds = Bounds::started_at(Instant::now(), None, None, priced());
        bounds.account(1, "m", (1_000_000, 1_000_000));
        assert_eq!(bounds.spent(), 18.0, "1M in at $3/M plus 1M out at $15/M");
        bounds.account(1, "m", (1_000_000, 0));
        assert_eq!(bounds.spent(), 21.0);
        assert_eq!(bounds.tokens(), 3_000_000, "every token counts, priced or not");
    }

    #[test]
    fn each_agent_prices_its_own_row_at_its_own_model() {
        let pricing = HashMap::from([
            ("a".to_string(), Price { input: 1.0, output: 1.0 }),
            ("b".to_string(), Price { input: 4.0, output: 4.0 }),
        ]);
        let bounds = Bounds::started_at(Instant::now(), None, None, pricing);
        bounds.account(1, "a", (1_000_000, 0));
        bounds.account(2, "b", (1_000_000, 0));
        let rows = bounds.agents();
        assert_eq!(
            rows,
            [
                (1, Spend { input: 1_000_000, output: 0, cost: 1.0 }),
                (2, Spend { input: 1_000_000, output: 0, cost: 4.0 })
            ]
        );
        assert_eq!(bounds.spent(), 5.0, "the total is the sum of the rows");
    }

    #[test]
    fn an_unpriced_model_costs_nothing() {
        let bounds = Bounds::started_at(Instant::now(), None, None, priced());
        bounds.account(1, "other", (1_000_000, 1_000_000));
        assert_eq!(bounds.spent(), 0.0);
        assert_eq!(bounds.agents()[0].1.tokens(), 2_000_000, "its tokens are still counted and shown");
    }

    #[test]
    fn the_warn_ceiling_speaks_once_and_never_stops_anything() {
        let bounds = Bounds::started_at(Instant::now(), None, None, priced()).with_warn_cost(Some(1.0));
        bounds.account(1, "m", (100_000, 0));
        assert_eq!(bounds.warning(), None, "under the ceiling, nothing to say");
        bounds.account(1, "m", (200_000, 0)); // 0.9 total
        assert_eq!(bounds.warning(), None, "still under");
        bounds.account(1, "m", (100_000, 0)); // 1.2 total
        let notice = bounds.warning().expect("the crossing warns");
        assert!(notice.contains("warn_cost"), "{notice}");
        assert_eq!(bounds.warning(), None, "a limit announces its crossing once");
        assert_eq!(bounds.exceeded(), None, "a warning stops nothing");
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
    fn wants_usage_tracks_either_cost_ceiling() {
        assert!(!Bounds::started_at(Instant::now(), Some(Duration::from_secs(1)), None, HashMap::new()).wants_usage());
        assert!(Bounds::started_at(Instant::now(), None, Some(1.0), HashMap::new()).wants_usage());
        let warned = Bounds::started_at(Instant::now(), None, None, HashMap::new()).with_warn_cost(Some(1.0));
        assert!(warned.wants_usage(), "a warning needs the same token counts a ceiling does");
    }
}
