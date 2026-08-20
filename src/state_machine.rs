//! Formally specified, audited, thread-safe MCP lifecycle state machine.
//!
//! The transition relation is a pure finite function used by the production
//! controller, exhaustive model checks, concurrency tests, and the companion
//! TLA+ specification. The controller serializes its tiny critical section
//! with a standard mutex and never holds that lock across an `.await`.

use std::{
    collections::{BTreeSet, VecDeque},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

const MAX_AUDIT_CAPACITY: usize = 4_096;

/// All declared lifecycle states, in deterministic model-check order.
pub const ALL_STATES: [LifecycleState; 6] = [
    LifecycleState::Created,
    LifecycleState::Starting,
    LifecycleState::Ready,
    LifecycleState::Degraded,
    LifecycleState::Draining,
    LifecycleState::Stopped,
];

/// All declared lifecycle events, in deterministic model-check order.
pub const ALL_EVENTS: [LifecycleEvent; 6] = [
    LifecycleEvent::Start,
    LifecycleEvent::Started,
    LifecycleEvent::Degrade,
    LifecycleEvent::Recover,
    LifecycleEvent::Drain,
    LifecycleEvent::Stop,
];

/// Runtime lifecycle states shared by every MCP transport.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    /// Controller exists but no transport startup has begun.
    Created,
    /// Configuration is validated and a transport is being started.
    Starting,
    /// The selected transport is accepting MCP work.
    Ready,
    /// The process is live but an essential dependency or exporter is impaired.
    Degraded,
    /// New work is no longer accepted and in-flight work is being drained.
    Draining,
    /// Terminal state; no transition may leave it.
    Stopped,
}

impl LifecycleState {
    /// Stable serialized name used in logs and read-only MCP status tools.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Degraded => "degraded",
            Self::Draining => "draining",
            Self::Stopped => "stopped",
        }
    }

    /// Whether no transition may leave this state.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Stopped)
    }
}

/// Closed lifecycle events; none carry caller-controlled data.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleEvent {
    /// Begin transport startup.
    Start,
    /// Transport startup completed and the service is ready.
    Started,
    /// An essential runtime capability became impaired.
    Degrade,
    /// All essential runtime capabilities recovered.
    Recover,
    /// Stop accepting new work and begin graceful drain.
    Drain,
    /// Finish shutdown and enter the terminal state.
    Stop,
}

impl LifecycleEvent {
    /// Stable serialized event name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Started => "started",
            Self::Degrade => "degrade",
            Self::Recover => "recover",
            Self::Drain => "drain",
            Self::Stop => "stop",
        }
    }
}

/// Pure transition relation shared by runtime code and model checks.
#[must_use]
pub const fn transition_target(
    state: LifecycleState,
    event: LifecycleEvent,
) -> Option<LifecycleState> {
    match (state, event) {
        (LifecycleState::Created, LifecycleEvent::Start) => Some(LifecycleState::Starting),
        (
            LifecycleState::Created | LifecycleState::Starting | LifecycleState::Draining,
            LifecycleEvent::Stop,
        ) => Some(LifecycleState::Stopped),
        (LifecycleState::Starting, LifecycleEvent::Started)
        | (LifecycleState::Degraded, LifecycleEvent::Recover) => Some(LifecycleState::Ready),
        (
            LifecycleState::Starting | LifecycleState::Ready | LifecycleState::Degraded,
            LifecycleEvent::Drain,
        ) => Some(LifecycleState::Draining),
        (LifecycleState::Ready, LifecycleEvent::Degrade) => Some(LifecycleState::Degraded),
        _ => None,
    }
}

/// Revisioned lifecycle snapshot delivered to synchronous and async readers.
#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LifecycleSnapshot {
    state: LifecycleState,
    revision: u64,
}

impl LifecycleSnapshot {
    const INITIAL: Self = Self {
        state: LifecycleState::Created,
        revision: 0,
    };

    /// Current lifecycle state.
    #[must_use]
    pub const fn state(self) -> LifecycleState {
        self.state
    }

    /// Monotonic successful-transition revision.
    #[must_use]
    pub const fn revision(self) -> u64 {
        self.revision
    }
}

/// One bounded audit entry containing only closed state-machine fields.
#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransitionRecord {
    revision: u64,
    from: LifecycleState,
    event: LifecycleEvent,
    to: LifecycleState,
}

impl TransitionRecord {
    /// Revision assigned to this successful transition.
    #[must_use]
    pub const fn revision(self) -> u64 {
        self.revision
    }

    /// State before the transition.
    #[must_use]
    pub const fn from(self) -> LifecycleState {
        self.from
    }

    /// Applied event.
    #[must_use]
    pub const fn event(self) -> LifecycleEvent {
        self.event
    }

    /// State after the transition.
    #[must_use]
    pub const fn to(self) -> LifecycleState {
        self.to
    }
}

#[derive(Debug)]
struct LifecycleData {
    snapshot: LifecycleSnapshot,
    audit: VecDeque<TransitionRecord>,
}

#[derive(Debug)]
struct LifecycleInner {
    data: Mutex<LifecycleData>,
    updates: watch::Sender<LifecycleSnapshot>,
    audit_capacity: usize,
}

/// Cloneable thread-safe lifecycle controller with ordered async updates.
///
/// Transition mutation, audit insertion, and watch publication occur while
/// holding the same short synchronous lock. This prevents concurrent senders
/// from publishing an older revision after a newer revision.
#[derive(Clone, Debug)]
pub struct LifecycleController {
    inner: Arc<LifecycleInner>,
}

impl LifecycleController {
    /// Creates a controller with a fixed bounded audit capacity.
    ///
    /// # Errors
    ///
    /// Returns [`TransitionError::InvalidAuditCapacity`] for zero or more than
    /// 4,096 records.
    pub fn new(audit_capacity: usize) -> Result<Self, TransitionError> {
        if audit_capacity == 0 || audit_capacity > MAX_AUDIT_CAPACITY {
            return Err(TransitionError::InvalidAuditCapacity);
        }
        let (updates, _) = watch::channel(LifecycleSnapshot::INITIAL);
        Ok(Self {
            inner: Arc::new(LifecycleInner {
                data: Mutex::new(LifecycleData {
                    snapshot: LifecycleSnapshot::INITIAL,
                    audit: VecDeque::with_capacity(audit_capacity),
                }),
                updates,
                audit_capacity,
            }),
        })
    }

    /// Returns the current state and revision.
    ///
    /// # Errors
    ///
    /// Fails closed if an earlier panic poisoned the controller lock.
    pub fn snapshot(&self) -> Result<LifecycleSnapshot, TransitionError> {
        Ok(self.lock_data()?.snapshot)
    }

    /// Returns the bounded audit history oldest-first.
    ///
    /// # Errors
    ///
    /// Fails closed if an earlier panic poisoned the controller lock.
    pub fn audit(&self) -> Result<Vec<TransitionRecord>, TransitionError> {
        Ok(self.lock_data()?.audit.iter().copied().collect())
    }

    /// Returns one atomic snapshot plus its matching bounded audit history.
    ///
    /// # Errors
    ///
    /// Fails closed if an earlier panic poisoned the controller lock.
    pub fn snapshot_and_audit(
        &self,
    ) -> Result<(LifecycleSnapshot, Vec<TransitionRecord>), TransitionError> {
        let data = self.lock_data()?;
        Ok((data.snapshot, data.audit.iter().copied().collect()))
    }

    /// Applies one event atomically and publishes its revision in strict order.
    ///
    /// # Errors
    ///
    /// Returns [`TransitionError::InvalidTransition`] for an event absent from
    /// the formal relation, or another variant for a poisoned lock/revision
    /// exhaustion.
    pub fn transition(&self, event: LifecycleEvent) -> Result<LifecycleSnapshot, TransitionError> {
        let mut data = self.lock_data()?;
        let from = data.snapshot.state;
        let to = transition_target(from, event)
            .ok_or(TransitionError::InvalidTransition { from, event })?;
        let revision = data
            .snapshot
            .revision
            .checked_add(1)
            .ok_or(TransitionError::RevisionExhausted)?;
        let snapshot = LifecycleSnapshot {
            state: to,
            revision,
        };
        data.snapshot = snapshot;
        if data.audit.len() == self.inner.audit_capacity {
            let _ = data.audit.pop_front();
        }
        data.audit.push_back(TransitionRecord {
            revision,
            from,
            event,
            to,
        });
        self.inner.updates.send_replace(snapshot);
        Ok(snapshot)
    }

    /// Subscribes to revisioned lifecycle snapshots.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<LifecycleSnapshot> {
        self.inner.updates.subscribe()
    }

    /// Waits asynchronously for an exact state with timeout and cancellation.
    ///
    /// No synchronous lock is held while awaiting updates.
    ///
    /// # Errors
    ///
    /// Returns [`WaitError::Timeout`], [`WaitError::Cancelled`], or
    /// [`WaitError::Closed`] when the requested state is not observed.
    pub async fn wait_for(
        &self,
        target: LifecycleState,
        timeout: Duration,
        cancellation: CancellationToken,
    ) -> Result<LifecycleSnapshot, WaitError> {
        let mut updates = self.subscribe();
        let wait = async move {
            loop {
                let current = *updates.borrow_and_update();
                if current.state == target {
                    return Ok(current);
                }
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return Err(WaitError::Cancelled),
                    changed = updates.changed() => {
                        if changed.is_err() {
                            return Err(WaitError::Closed);
                        }
                    }
                }
            }
        };
        tokio::time::timeout(timeout, wait)
            .await
            .map_err(|_| WaitError::Timeout)?
    }

    fn lock_data(&self) -> Result<MutexGuard<'_, LifecycleData>, TransitionError> {
        self.inner
            .data
            .lock()
            .map_err(|_| TransitionError::Poisoned)
    }
}

/// State-machine construction or transition failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum TransitionError {
    /// Audit capacity is zero or above the fixed hard limit.
    #[error("lifecycle audit capacity is outside the permitted range")]
    InvalidAuditCapacity,
    /// Event is not present in the transition relation for the current state.
    #[error("invalid lifecycle transition: {from:?} + {event:?}")]
    InvalidTransition {
        /// Current state.
        from: LifecycleState,
        /// Rejected event.
        event: LifecycleEvent,
    },
    /// Monotonic revision cannot advance further.
    #[error("lifecycle revision is exhausted")]
    RevisionExhausted,
    /// A panic occurred while the state lock was held.
    #[error("lifecycle controller lock is poisoned")]
    Poisoned,
}

/// Async lifecycle wait failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum WaitError {
    /// Timeout elapsed before the target state appeared.
    #[error("timed out waiting for lifecycle state")]
    Timeout,
    /// Caller cancelled the wait.
    #[error("lifecycle wait was cancelled")]
    Cancelled,
    /// Update channel closed unexpectedly.
    #[error("lifecycle update channel closed")]
    Closed,
}

/// Result of exhaustive validation over the finite transition graph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelValidationReport {
    reachable_states: BTreeSet<LifecycleState>,
    transition_count: usize,
}

impl ModelValidationReport {
    /// States reachable from [`LifecycleState::Created`].
    #[must_use]
    pub const fn reachable_states(&self) -> &BTreeSet<LifecycleState> {
        &self.reachable_states
    }

    /// Number of allowed edges in the declared relation.
    #[must_use]
    pub const fn transition_count(&self) -> usize {
        self.transition_count
    }
}

/// Exhaustively checks reachability, target closure, terminality, and deadlock
/// freedom for every declared state/event pair.
///
/// # Errors
///
/// Returns [`ModelViolation`] when the finite transition relation violates a
/// lifecycle invariant.
pub fn validate_model() -> Result<ModelValidationReport, ModelViolation> {
    let declared = ALL_STATES.into_iter().collect::<BTreeSet<_>>();
    let mut transition_count = 0;
    for state in ALL_STATES {
        for event in ALL_EVENTS {
            if let Some(target) = transition_target(state, event) {
                transition_count += 1;
                if !declared.contains(&target) {
                    return Err(ModelViolation::UndeclaredTarget);
                }
                if state.is_terminal() {
                    return Err(ModelViolation::TerminalHasOutgoingTransition);
                }
            }
        }
    }

    let mut reachable = BTreeSet::from([LifecycleState::Created]);
    let mut frontier = vec![LifecycleState::Created];
    while let Some(state) = frontier.pop() {
        for event in ALL_EVENTS {
            if let Some(target) = transition_target(state, event) {
                if reachable.insert(target) {
                    frontier.push(target);
                }
            }
        }
    }
    if reachable != declared {
        return Err(ModelViolation::UnreachableState);
    }
    for state in reachable
        .iter()
        .copied()
        .filter(|state| !state.is_terminal())
    {
        if ALL_EVENTS
            .into_iter()
            .all(|event| transition_target(state, event).is_none())
        {
            return Err(ModelViolation::NonTerminalDeadlock);
        }
    }
    Ok(ModelValidationReport {
        reachable_states: reachable,
        transition_count,
    })
}

/// Exhaustive model invariant violation.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ModelViolation {
    /// Transition relation targets a state outside [`ALL_STATES`].
    #[error("lifecycle relation contains an undeclared target")]
    UndeclaredTarget,
    /// A terminal state has an outgoing transition.
    #[error("terminal lifecycle state has an outgoing transition")]
    TerminalHasOutgoingTransition,
    /// At least one declared state is unreachable from the initial state.
    #[error("declared lifecycle state is unreachable")]
    UnreachableState,
    /// A reachable non-terminal state has no outgoing transition.
    #[error("reachable non-terminal lifecycle state is deadlocked")]
    NonTerminalDeadlock,
}

#[cfg(test)]
mod tests {
    use std::{sync::Barrier, thread};

    use super::*;

    #[test]
    fn finite_model_satisfies_all_invariants() {
        let report = validate_model().expect("finite model is valid");
        assert_eq!(report.reachable_states().len(), ALL_STATES.len());
        assert_eq!(report.transition_count(), 10);
        assert!(LifecycleState::Stopped.is_terminal());
        assert!(
            ALL_EVENTS
                .into_iter()
                .all(|event| transition_target(LifecycleState::Stopped, event).is_none())
        );
    }

    #[test]
    fn duplicate_concurrent_start_is_linearized_once() {
        let controller = LifecycleController::new(8).expect("valid controller");
        let barrier = Arc::new(Barrier::new(3));
        let mut joins = Vec::new();
        for _ in 0..2 {
            let controller = controller.clone();
            let barrier = barrier.clone();
            joins.push(thread::spawn(move || {
                barrier.wait();
                controller.transition(LifecycleEvent::Start)
            }));
        }
        barrier.wait();
        let mut successes = 0;
        for join in joins {
            if join.join().expect("thread did not panic").is_ok() {
                successes += 1;
            }
        }
        assert_eq!(successes, 1);
        assert_eq!(
            controller.snapshot().expect("snapshot"),
            LifecycleSnapshot {
                state: LifecycleState::Starting,
                revision: 1,
            }
        );
        assert_eq!(controller.audit().expect("audit").len(), 1);
    }

    #[test]
    fn audit_is_bounded_and_revision_ordered() {
        let controller = LifecycleController::new(3).expect("valid controller");
        for event in [
            LifecycleEvent::Start,
            LifecycleEvent::Started,
            LifecycleEvent::Degrade,
            LifecycleEvent::Recover,
            LifecycleEvent::Drain,
            LifecycleEvent::Stop,
        ] {
            controller.transition(event).expect("valid transition");
        }
        let audit = controller.audit().expect("audit");
        assert_eq!(audit.len(), 3);
        assert_eq!(
            audit
                .iter()
                .map(|record| record.revision())
                .collect::<Vec<_>>(),
            [4, 5, 6]
        );
        let (snapshot, atomic_audit) = controller.snapshot_and_audit().expect("atomic view");
        assert_eq!(snapshot.revision(), 6);
        assert_eq!(atomic_audit.last().map(|record| record.revision()), Some(6));
    }

    #[tokio::test]
    async fn async_wait_observes_ordered_state_or_cancellation() {
        let controller = LifecycleController::new(8).expect("valid controller");
        let cancellation = CancellationToken::new();
        let waiter = {
            let controller = controller.clone();
            let cancellation = cancellation.clone();
            tokio::spawn(async move {
                controller
                    .wait_for(LifecycleState::Ready, Duration::from_secs(1), cancellation)
                    .await
            })
        };
        controller.transition(LifecycleEvent::Start).expect("start");
        controller
            .transition(LifecycleEvent::Started)
            .expect("started");
        let observed = waiter.await.expect("wait task").expect("ready observed");
        assert_eq!(observed.state(), LifecycleState::Ready);
        assert_eq!(observed.revision(), 2);

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert_eq!(
            controller
                .wait_for(LifecycleState::Stopped, Duration::from_secs(1), cancelled,)
                .await,
            Err(WaitError::Cancelled)
        );
    }

    #[test]
    fn controller_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LifecycleController>();
    }
}
