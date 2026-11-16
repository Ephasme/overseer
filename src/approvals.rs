//! The approval gate: which gated tool calls (`bash`, `write_file`, `edit`) run without asking.
//!
//! The per-agent `Policy` is fixed at spawn; this gate is the user's, live for the whole session.
//! A tier decides when a call may prompt the user, and the session cache remembers an answer so
//! the same call is not asked twice: every answer on a caching tier, and `Always` on any tier.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::swarm::Event;
use crate::tools;

/// How long [`Gate::admit`] waits between checks for an answer, so a stop releases a prompt
/// without anyone notifying the gate.
const ANSWER_POLL: Duration = Duration::from_millis(50);

/// When a gated call may prompt the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tier {
    /// Ask on every gated call the user has not already allowed outright.
    Ask,
    /// Ask the first time each distinct call appears, then replay the cached answer.
    CachedAsk,
    /// `write_file` and `edit` always run; `bash` is asked like `CachedAsk`.
    Write,
    /// Nothing ever asks.
    Yolo,
}

impl Tier {
    /// The tier a `config.toml` `approvals` line names.
    pub(crate) fn parse(value: &str) -> Result<Tier, String> {
        match value {
            "ask" => Ok(Tier::Ask),
            "cached-ask" => Ok(Tier::CachedAsk),
            "write" => Ok(Tier::Write),
            "yolo" => Ok(Tier::Yolo),
            _ => Err(format!(
                "unknown approvals value {value:?}; expected one of \"ask\", \"cached-ask\", \"write\", \"yolo\""
            )),
        }
    }

    /// The name a status line or config error shows.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Tier::Ask => "ask",
            Tier::CachedAsk => "cached-ask",
            Tier::Write => "write",
            Tier::Yolo => "yolo",
        }
    }

    /// The tier the F8 key binding cycles to next.
    pub(crate) fn next(self) -> Tier {
        match self {
            Tier::Ask => Tier::CachedAsk,
            Tier::CachedAsk => Tier::Write,
            Tier::Write => Tier::Yolo,
            Tier::Yolo => Tier::Ask,
        }
    }

    /// Whether every answer on this tier and tool is remembered, not only `Always`.
    fn caches(self, tool: &str) -> bool {
        matches!(self, Tier::CachedAsk) || (self == Tier::Write && tool == tools::BASH)
    }
}

/// One answer to a prompt: run it, run it and remember it, or refuse it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Choice {
    /// Run this call and remember it for the session, on any tier.
    Always,
    /// Run this call once; on a caching tier the run itself is the memory.
    Once,
    /// Refuse this call; on a caching tier the refusal is remembered where the run would be.
    Deny,
}

/// One gated call waiting for the user.
struct Pending {
    id: u64,
}

/// The session-wide approval gate, shared by every agent and the pipeline host.
pub(crate) struct Gate {
    tier: Mutex<Tier>,
    /// Call key -> the answer to replay. Holds every answer on a caching tier, and every
    /// `Always` whatever the tier.
    cache: Mutex<HashMap<String, Choice>>,
    requests: Mutex<Vec<Pending>>,
    /// Answers the session sent for requests that may not be pending anymore.
    answers: Mutex<HashMap<u64, Choice>>,
    wake: Condvar,
    /// Whether a terminal can answer prompts; a pipe denies instead of asking.
    interactive: bool,
    next_id: AtomicU64,
}

impl Gate {
    pub(crate) fn new(tier: Tier, interactive: bool) -> Self {
        Self {
            tier: Mutex::new(tier),
            cache: Mutex::new(HashMap::new()),
            requests: Mutex::new(Vec::new()),
            answers: Mutex::new(HashMap::new()),
            wake: Condvar::new(),
            interactive,
            next_id: AtomicU64::new(1),
        }
    }

    /// The tier the status line shows.
    pub(crate) fn tier(&self) -> Tier {
        *Self::poison(&self.tier)
    }

    /// Advances the tier (the F8 cycle) and returns the new one.
    pub(crate) fn cycle(&self) -> Tier {
        let mut guard = Self::poison(&self.tier);
        *guard = guard.next();
        *guard
    }

    /// The number of gated calls waiting for an answer; tests assert a stop leaves none.
    #[cfg(test)]
    pub(crate) fn pending_count(&self) -> usize {
        Self::poison(&self.requests).len()
    }

    /// Records the user's answer, dropping responses that can no longer be consumed: an answer
    /// racing an agent's stop would otherwise sit in the map forever.
    ///
    /// Takes `requests` before `answers`, the order [`Gate::remove_pending`] uses too: the two
    /// locks are never held in opposite orders.
    pub(crate) fn answer(&self, request: u64, choice: Choice) {
        let requests = Self::poison(&self.requests);
        // Ids are monotonic: anything older than the smallest pending id can never be claimed.
        let smallest = requests.iter().map(|p| p.id).min();
        let mut answers = Self::poison(&self.answers);
        answers.insert(request, choice);
        match smallest {
            Some(smallest) => answers.retain(|id, _| *id >= smallest),
            None => answers.clear(),
        }
        drop(answers);
        drop(requests);
        self.wake.notify_all();
    }

    /// Decides one gated call. `Ok(())` runs it; the `Err` text goes back to the model.
    pub(crate) fn admit(
        &self,
        agent: usize,
        tool: &str,
        key: &str,
        display: &str,
        cancel: Option<&AtomicBool>,
        ui: &Sender<Event>,
    ) -> Result<(), String> {
        let tier = self.tier();
        if tier == Tier::Yolo || (tier == Tier::Write && tool != tools::BASH) {
            return Ok(());
        }
        // Namespaced by tool: a shell command and a file path that read the same must never share
        // one answer, or an `Always` for `./build.sh` would also approve writing to `./build.sh`.
        let cache_key = format!("{tool}\u{0}{key}");
        match Self::poison(&self.cache).get(&cache_key) {
            Some(Choice::Always | Choice::Once) => return Ok(()),
            Some(Choice::Deny) => {
                return Err(format!(
                    "the user did not approve this {tool} call earlier in this session; do not repeat it without asking"
                ));
            }
            None => {}
        }
        if !self.interactive {
            return Err(
                "no terminal is attached to answer approvals; ask the user to run overseer in a terminal or set approvals = \"yolo\""
                    .to_string(),
            );
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        Self::poison(&self.requests).push(Pending { id });
        let _ = ui.send(Event::ApprovalRequest {
            id: agent,
            request: id,
            tool: tool.to_string(),
            call: display.to_string(),
        });
        let mut answers = Self::poison(&self.answers);
        loop {
            // The 50 ms poll is what makes a stop work: nothing the stop path touches notifies this condvar.
            if let Some(flag) = cancel
                && flag.load(Ordering::Relaxed)
            {
                // `remove_pending` takes the answers lock too: release it before asking.
                drop(answers);
                self.remove_pending(id);
                // The frontend still shows the prompt: without this it would keep taking keys
                // for a call nobody waits on.
                let _ = ui.send(Event::ApprovalWithdrawn { request: id });
                return Err("the call was cut short before the user could answer".to_string());
            }
            if let Some(choice) = answers.remove(&id) {
                drop(answers);
                // `Always` outlasts every tier; the rest lasts where the tier caches.
                if matches!(choice, Choice::Always) || tier.caches(tool) {
                    Self::poison(&self.cache).insert(cache_key, choice);
                }
                self.remove_pending(id);
                return match choice {
                    Choice::Always | Choice::Once => Ok(()),
                    Choice::Deny => {
                        Err(format!("the user did not approve this {tool} call; do not repeat it without asking first"))
                    }
                };
            }
            let (guard, _timeout) =
                self.wake.wait_timeout(answers, ANSWER_POLL).unwrap_or_else(PoisonError::into_inner);
            answers = guard;
        }
    }

    /// Drops one pending entry by id, and any answer already recorded for it: a prompt that
    /// ended without being waited on leaves nothing behind, and needs no later cleanup.
    fn remove_pending(&self, id: u64) {
        let mut requests = Self::poison(&self.requests);
        requests.retain(|p| p.id != id);
        drop(requests);
        Self::poison(&self.answers).remove(&id);
    }

    /// The `Mutex` + `PoisonError::into_inner` recovery used across the crate.
    fn poison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::mpsc::{self, Receiver};
    use std::thread;
    use std::time::{Duration, Instant};

    /// Runs one `admit` on its own thread, the way an agent does: the test thread answers.
    fn ask_async(gate: &Arc<Gate>, ui: &Sender<Event>, tool: &'static str) {
        let gate = Arc::clone(gate);
        let ui = ui.clone();
        thread::spawn(move || {
            let _ = gate.admit(1, tool, "the call", "the call", None, &ui);
        });
    }

    fn first_request(ui: &Receiver<Event>) -> u64 {
        match ui.recv_timeout(Duration::from_secs(1)) {
            Ok(Event::ApprovalRequest { request, .. }) => request,
            other => panic!("expected an approval request, got {other:?}"),
        }
    }

    /// Waits until the background `admit` has finished; the condvar wakes it.
    fn settled(gate: &Gate) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while gate.pending_count() > 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(gate.pending_count(), 0, "the admit never settled");
    }

    fn admits(gate: &Gate, ui: &Sender<Event>, tool: &str) -> Result<(), String> {
        gate.admit(1, tool, "the call", "the call", None, ui)
    }

    #[test]
    fn a_cached_ask_answer_caches_and_replays() {
        let (tx, rx) = mpsc::channel::<Event>();
        let gate = Arc::new(Gate::new(Tier::CachedAsk, true));
        ask_async(&gate, &tx, tools::BASH);
        gate.answer(first_request(&rx), Choice::Always);
        settled(&gate);
        // The replay happens without a second request.
        assert!(admits(&gate, &tx, tools::BASH).is_ok());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_cached_deny_errors_without_a_new_request() {
        let (tx, rx) = mpsc::channel::<Event>();
        let gate = Arc::new(Gate::new(Tier::CachedAsk, true));
        ask_async(&gate, &tx, tools::BASH);
        gate.answer(first_request(&rx), Choice::Deny);
        settled(&gate);
        let error = admits(&gate, &tx, tools::BASH).unwrap_err();
        assert!(error.contains("earlier in this session"), "{error}");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn an_always_answer_replays_on_every_tier_but_once_only_where_the_tier_caches() {
        // Under `ask`, `Always` is the user's explicit decision and outlives the tier, while
        // `Once` is remembered nowhere: the next identical call prompts again.
        let (tx, rx) = mpsc::channel::<Event>();
        let gate = Arc::new(Gate::new(Tier::Ask, true));
        ask_async(&gate, &tx, tools::BASH);
        gate.answer(first_request(&rx), Choice::Once);
        settled(&gate);
        ask_async(&gate, &tx, tools::BASH);
        gate.answer(first_request(&rx), Choice::Always);
        settled(&gate);
        assert!(admits(&gate, &tx, tools::BASH).is_ok(), "always allow must not ask again");
        assert!(rx.try_recv().is_err());
        // Under `cached-ask` a bare `Once` is the memory.
        let (tx2, rx2) = mpsc::channel::<Event>();
        let cached = Arc::new(Gate::new(Tier::CachedAsk, true));
        ask_async(&cached, &tx2, tools::BASH);
        cached.answer(first_request(&rx2), Choice::Once);
        settled(&cached);
        assert!(admits(&cached, &tx2, tools::BASH).is_ok(), "a cached once is replayed");
        assert!(rx2.try_recv().is_err());
    }

    #[test]
    fn the_write_tier_lets_writes_through_and_asks_for_bash() {
        let (tx, rx) = mpsc::channel::<Event>();
        let gate = Arc::new(Gate::new(Tier::Write, true));
        assert!(admits(&gate, &tx, tools::WRITE_FILE).is_ok());
        assert!(admits(&gate, &tx, tools::EDIT).is_ok());
        ask_async(&gate, &tx, tools::BASH);
        gate.answer(first_request(&rx), Choice::Always);
        settled(&gate);
        assert!(admits(&gate, &tx, tools::BASH).is_ok());
    }

    #[test]
    fn a_non_interactive_gate_denies_without_asking() {
        let (tx, _rx) = mpsc::channel::<Event>();
        let gate = Gate::new(Tier::Ask, false);
        let error = admits(&gate, &tx, tools::BASH).unwrap_err();
        assert!(error.contains("no terminal"), "{error}");
        assert_eq!(gate.pending_count(), 0);
    }

    #[test]
    fn a_cancelled_wait_errors_withdraws_the_prompt_and_leaves_nothing_behind() {
        let (tx, rx) = mpsc::channel::<Event>();
        let gate = Gate::new(Tier::Ask, true);
        let cancel = AtomicBool::new(true);
        let error = gate.admit(1, tools::BASH, "the call", "the call", Some(&cancel), &tx).unwrap_err();
        assert!(error.contains("cut short"), "{error}");
        assert_eq!(gate.pending_count(), 0);
        // The request came first, then its withdrawal: the frontend stops showing it.
        let request = first_request(&rx);
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), Event::ApprovalWithdrawn { request });
        assert!(Gate::poison(&gate.answers).is_empty(), "a cancelled prompt keeps no answer slot");
    }

    #[test]
    fn a_cache_answer_is_scoped_to_its_tool() {
        let (tx, rx) = mpsc::channel::<Event>();
        let gate = Arc::new(Gate::new(Tier::CachedAsk, true));
        ask_async(&gate, &tx, tools::BASH);
        gate.answer(first_request(&rx), Choice::Always);
        settled(&gate);
        // The same bytes as a file path are a different call: it must still ask.
        ask_async(&gate, &tx, tools::WRITE_FILE);
        first_request(&rx);
    }

    #[test]
    fn stale_answers_are_dropped_when_nothing_is_pending() {
        let gate = Gate::new(Tier::Ask, false);
        gate.answer(7, Choice::Always);
        gate.answer(9, Choice::Deny);
        assert!(Gate::poison(&gate.answers).is_empty(), "an answer nobody waits on must not be kept");
        assert_eq!(gate.pending_count(), 0);
    }

    #[test]
    fn yolo_never_prompts() {
        let (tx, _rx) = mpsc::channel::<Event>();
        let gate = Gate::new(Tier::Yolo, true);
        assert!(admits(&gate, &tx, tools::BASH).is_ok());
        assert!(admits(&gate, &tx, tools::WRITE_FILE).is_ok());
        assert_eq!(gate.pending_count(), 0);
    }

    #[test]
    fn tier_parsing_and_cycling_cover_every_value() {
        for (text, tier) in
            [("ask", Tier::Ask), ("cached-ask", Tier::CachedAsk), ("write", Tier::Write), ("yolo", Tier::Yolo)]
        {
            assert_eq!(Tier::parse(text).unwrap(), tier);
            assert_eq!(tier.name(), text);
        }
        assert!(Tier::parse("sudo").is_err());
        let mut tier = Tier::Ask;
        for _ in 0..4 {
            tier = tier.next();
        }
        assert_eq!(tier, Tier::Ask);
    }
}
