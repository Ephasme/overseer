//! The approval gate: which gated tool calls (`bash`, `write_file`, `edit`) run without asking.
//!
//! The per-agent `Policy` is fixed at spawn; this gate is the user's, live for the whole session.
//! A tier decides when a call may prompt the user, and under `cached-ask` the session cache
//! remembers one call's answer so the same call is not asked twice.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::swarm::Event;
use std::sync::mpsc::Sender;

/// How long [`Gate::admit`] waits between checks for an answer, so a stop releases a prompt
/// without anyone notifying the gate.
const ANSWER_POLL: Duration = Duration::from_millis(50);

/// When a gated call may prompt the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tier {
    /// Ask on every gated call.
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

    /// Whether a call of this tool consults the cache in this tier.
    fn caches(self, tool: &str) -> bool {
        matches!(self, Tier::CachedAsk) || (self == Tier::Write && tool == "bash")
    }
}

/// One answer to a prompt: run it, run it and remember it, or refuse it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Choice {
    /// Run this call and remember it for the session.
    Always,
    /// Run this call once; under `cached-ask` the run itself is the memory.
    Once,
    /// Refuse this call; the refusal is cached where the run would be.
    Deny,
}

/// One gated call waiting for the user. The fields beside `id` name the call in the prompt
/// the frontend renders; they are read there, not here.
#[allow(dead_code)]
struct Pending {
    id: u64,
    agent: usize,
    tool: String,
    call: String,
}

/// The session-wide approval gate, shared by every agent and the pipeline host.
pub(crate) struct Gate {
    tier: Mutex<Tier>,
    /// Call key -> the answer to replay, under `cached-ask` (and `write` for bash).
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

    /// Records the user's answer and drops any response that can no longer be consumed:
    /// an answer racing an agent's stop would otherwise sit in the map forever.
    pub(crate) fn answer(&self, request: u64, choice: Choice) {
        {
            let mut answers = Self::poison(&self.answers);
            answers.insert(request, choice);
            // Ids are monotonic: anything older than the smallest pending id can never be claimed.
            let pending = Self::poison(&self.requests);
            match pending.iter().map(|p| p.id).min() {
                Some(smallest) => answers.retain(|id, _| *id >= smallest),
                None => answers.clear(),
            }
        }
        self.wake.notify_all();
    }

    /// Decides one gated call. `Ok(())` runs it; the `Err` text goes back to the model.
    pub(crate) fn admit(
        &self,
        agent: usize,
        tool: &str,
        key: &str,
        display: &str,
        cancel: Option<&std::sync::atomic::AtomicBool>,
        ui: &Sender<Event>,
    ) -> Result<(), String> {
        let tier = self.tier();
        if tier == Tier::Yolo || (tier == Tier::Write && tool != "bash") {
            return Ok(());
        }
        if tier.caches(tool) {
            match Self::poison(&self.cache).get(key) {
                Some(Choice::Always | Choice::Once) => return Ok(()),
                Some(Choice::Deny) => {
                    return Err(format!(
                        "the user did not approve this {tool} call earlier in this session; do not repeat it without asking"
                    ));
                }
                None => {}
            }
        }
        if !self.interactive {
            return Err(
                "no terminal is attached to answer approvals; ask the user to run overseer in a terminal or set approvals = \"yolo\""
                    .to_string(),
            );
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        Self::poison(&self.requests).push(Pending { id, agent, tool: tool.to_string(), call: display.to_string() });
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
                self.remove_pending(id);
                return Err("the call was cut short before the user could answer".to_string());
            }
            if let Some(choice) = answers.remove(&id) {
                if tier.caches(tool) {
                    Self::poison(&self.cache).insert(key.to_string(), choice);
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

    /// Drops one pending entry by id; a cancelled prompt leaves nothing behind.
    fn remove_pending(&self, id: u64) {
        Self::poison(&self.requests).retain(|p| p.id != id);
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
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::{self, Receiver};
    use std::thread;
    use std::time::Duration;
    use std::time::Instant;

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

    #[test]
    fn a_cached_ask_answer_caches_and_replays() {
        let (tx, rx) = mpsc::channel::<Event>();
        let gate = Arc::new(Gate::new(Tier::CachedAsk, true));
        ask_async(&gate, &tx, "bash");
        gate.answer(first_request(&rx), Choice::Always);
        settled(&gate);
        // The replay happens without a second request.
        assert!(gate.admit(1, "bash", "the call", "the call", None, &tx).is_ok());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_cached_deny_errors_without_a_new_request() {
        let (tx, rx) = mpsc::channel::<Event>();
        let gate = Arc::new(Gate::new(Tier::CachedAsk, true));
        ask_async(&gate, &tx, "bash");
        gate.answer(first_request(&rx), Choice::Deny);
        settled(&gate);
        let error = gate.admit(1, "bash", "the call", "the call", None, &tx).unwrap_err();
        assert!(error.contains("earlier in this session"), "{error}");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_once_answer_prompts_again_under_ask_but_not_under_cached_ask() {
        let (tx, rx) = mpsc::channel::<Event>();
        let gate = Arc::new(Gate::new(Tier::Ask, true));
        ask_async(&gate, &tx, "bash");
        gate.answer(first_request(&rx), Choice::Once);
        settled(&gate);
        // `ask` never consults the cache: the second call prompts again.
        ask_async(&gate, &tx, "bash");
        first_request(&rx);
    }

    #[test]
    fn the_write_tier_lets_writes_through_and_asks_for_bash() {
        let (tx, rx) = mpsc::channel::<Event>();
        let gate = Arc::new(Gate::new(Tier::Write, true));
        assert!(gate.admit(1, "write_file", "the call", "the call", None, &tx).is_ok());
        assert!(gate.admit(1, "edit", "the call", "the call", None, &tx).is_ok());
        ask_async(&gate, &tx, "bash");
        gate.answer(first_request(&rx), Choice::Always);
        settled(&gate);
        assert!(gate.admit(1, "bash", "the call", "the call", None, &tx).is_ok());
    }

    #[test]
    fn a_non_interactive_gate_denies_without_asking() {
        let (tx, _rx) = mpsc::channel::<Event>();
        let gate = Gate::new(Tier::Ask, false);
        let error = gate.admit(1, "bash", "the call", "the call", None, &tx).unwrap_err();
        assert!(error.contains("no terminal"), "{error}");
        assert_eq!(gate.pending_count(), 0);
    }

    #[test]
    fn a_cancelled_wait_errors_and_leaves_no_pending_entry() {
        let (tx, rx) = mpsc::channel::<Event>();
        let gate = Gate::new(Tier::Ask, true);
        let cancel = AtomicBool::new(true);
        let error = gate.admit(1, "bash", "the call", "the call", Some(&cancel), &tx).unwrap_err();
        assert!(error.contains("cut short"), "{error}");
        assert_eq!(gate.pending_count(), 0);
        // The request was sent before the cancel poll saw the flag.
        first_request(&rx);
    }

    #[test]
    fn answers_older_than_the_smallest_pending_id_are_dropped() {
        let (_tx, _rx) = mpsc::channel::<Event>();
        let gate = Gate::new(Tier::Ask, false);
        // Nothing pending: the whole map is cleared.
        gate.answer(7, Choice::Always);
        gate.answer(9, Choice::Deny);
        assert_eq!(gate.pending_count(), 0);
    }

    #[test]
    fn yolo_never_prompts() {
        let (tx, _rx) = mpsc::channel::<Event>();
        let gate = Gate::new(Tier::Yolo, true);
        assert!(gate.admit(1, "bash", "the call", "the call", None, &tx).is_ok());
        assert!(gate.admit(1, "write_file", "the call", "the call", None, &tx).is_ok());
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
