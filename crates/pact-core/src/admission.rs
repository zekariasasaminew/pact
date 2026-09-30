//! Admission control for `spawn_many` -- see DESIGN.md ("pact-core >
//! Admission control", issue #285). Bounds how many agents are in their
//! "running" phase at once, refuses to launch into a machine that is
//! already short of memory, and spaces launches out so a provider does
//! not see N simultaneous new sessions.

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// The limits one `spawn_many` batch runs under. Defaults were chosen for
/// the owner's 14 GB laptop, where measured lean agents peak at
/// 0.3-0.45 GB each and a test run or build at 1.5-1.9 GB: two editors
/// plus one verifier is what fits with a browser open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionPolicy {
    /// Most agents in their running phase at once. Workspace creation and
    /// dependency prep are not counted -- in link mode they take about a
    /// second and having them done means a queued task launches the
    /// moment a slot frees.
    pub max_concurrent: usize,
    /// Minimum available memory (MiB) before another agent is admitted.
    /// `0` disables the check.
    pub min_free_mem_mb: u64,
    /// Minimum gap between two admissions. Providers reject bursts of new
    /// sessions (anthropics/claude-code#53922), so even a machine with
    /// room should not start N agents in the same instant.
    pub stagger: Duration,
}

impl Default for AdmissionPolicy {
    fn default() -> Self {
        Self {
            max_concurrent: 2,
            min_free_mem_mb: 1500,
            stagger: Duration::from_millis(2000),
        }
    }
}

/// What the pure decision function says to do right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionDecision {
    Admit,
    /// Every slot is taken; wait for a running agent to finish.
    WaitForSlot,
    /// A slot is free but the machine is short of memory; wait for it to
    /// come back rather than launch into paging. Zero admissions is a
    /// valid state.
    WaitForMemory,
}

/// The decision, kept pure so it can be unit-tested without a machine
/// behind it: `running` agents already admitted, `available_mb` of
/// memory right now.
pub fn decide(running: usize, policy: &AdmissionPolicy, available_mb: u64) -> AdmissionDecision {
    if running >= policy.max_concurrent.max(1) {
        AdmissionDecision::WaitForSlot
    } else if policy.min_free_mem_mb > 0 && available_mb < policy.min_free_mem_mb {
        AdmissionDecision::WaitForMemory
    } else {
        AdmissionDecision::Admit
    }
}

struct State {
    running: usize,
    /// When the most recent admission became effective -- the next one
    /// may not happen before this plus the stagger. A reservation, not
    /// an observation: set when a slot is claimed, so two waiters that
    /// both see room still launch `stagger` apart.
    next_allowed: Instant,
}

/// Shared by every task thread in one `spawn_many` batch.
pub struct Admission {
    policy: AdmissionPolicy,
    state: Mutex<State>,
    changed: Condvar,
}

/// How often a waiter re-checks memory (a slot release wakes it sooner).
const RECHECK_INTERVAL: Duration = Duration::from_secs(5);
/// How often a waiter repeats its "still queued" message.
const REPORT_INTERVAL: Duration = Duration::from_secs(15);

impl Admission {
    pub fn new(policy: AdmissionPolicy) -> Self {
        Self {
            policy,
            state: Mutex::new(State { running: 0, next_allowed: Instant::now() }),
            changed: Condvar::new(),
        }
    }

    pub fn policy(&self) -> &AdmissionPolicy {
        &self.policy
    }

    /// Blocks until this caller may launch, then returns a guard whose
    /// drop releases the slot. `on_wait` is told, at most every
    /// `REPORT_INTERVAL`, why the caller is still queued.
    pub fn acquire(&self, mut on_wait: impl FnMut(&str)) -> AdmissionGuard<'_> {
        let mut last_report: Option<Instant> = None;
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            let available_mb = available_memory_mb();
            match decide(state.running, &self.policy, available_mb) {
                AdmissionDecision::Admit => {
                    state.running += 1;
                    let now = Instant::now();
                    let due = state.next_allowed.max(now);
                    state.next_allowed = due + self.policy.stagger;
                    drop(state);
                    if due > now {
                        std::thread::sleep(due - now);
                    }
                    return AdmissionGuard { admission: self };
                }
                decision => {
                    let message = match decision {
                        AdmissionDecision::WaitForSlot => format!(
                            "queued: {} of {} agent slots in use, waiting for one to finish",
                            state.running, self.policy.max_concurrent
                        ),
                        _ => format!(
                            "queued: {available_mb} MB of memory available, need {} MB before launching another agent",
                            self.policy.min_free_mem_mb
                        ),
                    };
                    if last_report.is_none_or(|t| t.elapsed() >= REPORT_INTERVAL) {
                        on_wait(&message);
                        last_report = Some(Instant::now());
                    }
                    state = self
                        .changed
                        .wait_timeout(state, RECHECK_INTERVAL)
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .0;
                }
            }
        }
    }

    fn release(&self) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.running = state.running.saturating_sub(1);
        drop(state);
        self.changed.notify_all();
    }
}

/// A held admission slot; dropping it lets the next queued task launch.
pub struct AdmissionGuard<'a> {
    admission: &'a Admission,
}

impl Drop for AdmissionGuard<'_> {
    fn drop(&mut self) {
        self.admission.release();
    }
}

/// Memory available for new allocations right now, in MiB, per `sysinfo`.
pub fn available_memory_mb() -> u64 {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    system.available_memory() / (1024 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn policy(max: usize, floor: u64) -> AdmissionPolicy {
        AdmissionPolicy { max_concurrent: max, min_free_mem_mb: floor, stagger: Duration::ZERO }
    }

    #[test]
    fn decide_admits_when_a_slot_is_free_and_memory_is_above_the_floor() {
        assert_eq!(decide(0, &policy(2, 1500), 4000), AdmissionDecision::Admit);
        assert_eq!(decide(1, &policy(2, 1500), 1500), AdmissionDecision::Admit);
    }

    #[test]
    fn decide_waits_for_a_slot_before_it_even_looks_at_memory() {
        assert_eq!(decide(2, &policy(2, 1500), 8000), AdmissionDecision::WaitForSlot);
    }

    #[test]
    fn decide_waits_for_memory_below_the_floor_even_with_a_free_slot() {
        assert_eq!(decide(0, &policy(2, 1500), 600), AdmissionDecision::WaitForMemory);
    }

    #[test]
    fn decide_treats_a_zero_floor_as_no_memory_check() {
        assert_eq!(decide(0, &policy(2, 0), 0), AdmissionDecision::Admit);
    }

    #[test]
    fn decide_never_deadlocks_on_a_zero_cap() {
        // A cap of 0 would otherwise mean nothing ever runs; it is read as 1.
        assert_eq!(decide(0, &policy(0, 0), 0), AdmissionDecision::Admit);
        assert_eq!(decide(1, &policy(0, 0), 0), AdmissionDecision::WaitForSlot);
    }

    #[test]
    fn acquire_never_lets_more_than_max_concurrent_run_at_once() {
        let admission = Arc::new(Admission::new(policy(2, 0)));
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..6)
            .map(|_| {
                let admission = Arc::clone(&admission);
                let running = Arc::clone(&running);
                let peak = Arc::clone(&peak);
                std::thread::spawn(move || {
                    let _slot = admission.acquire(|_| {});
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(40));
                    running.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), 2, "peak concurrency must equal the cap, not exceed it");
    }

    #[test]
    fn acquire_spaces_admissions_by_the_stagger() {
        let admission = Admission::new(AdmissionPolicy {
            max_concurrent: 4,
            min_free_mem_mb: 0,
            stagger: Duration::from_millis(120),
        });
        let start = Instant::now();
        let _a = admission.acquire(|_| {});
        let _b = admission.acquire(|_| {});
        let _c = admission.acquire(|_| {});
        assert!(start.elapsed() >= Duration::from_millis(240), "three admissions need two stagger gaps, took {:?}", start.elapsed());
    }

    #[test]
    fn acquire_reports_why_it_is_waiting() {
        let admission = Admission::new(policy(1, 0));
        let held = admission.acquire(|_| {});
        let reasons = Arc::new(Mutex::new(Vec::new()));
        let reasons_for_thread = Arc::clone(&reasons);
        let admission_ref = &admission;
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let _slot = admission_ref.acquire(|msg| reasons_for_thread.lock().unwrap().push(msg.to_string()));
            });
            std::thread::sleep(Duration::from_millis(60));
            drop(held);
        });
        let reasons = reasons.lock().unwrap();
        assert!(reasons.iter().any(|r| r.contains("1 of 1 agent slots in use")), "got: {reasons:?}");
    }
}
