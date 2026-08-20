//! Exhaustive and async contract tests for the public lifecycle state machine.

use std::{collections::BTreeSet, time::Duration};

use ores_mcp_server_core_libs::state_machine::{
    ALL_EVENTS, ALL_STATES, LifecycleController, LifecycleEvent, LifecycleState, TransitionError,
    WaitError, transition_target, validate_model,
};
use tokio_util::sync::CancellationToken;

const EXPECTED_EDGES: [(LifecycleState, LifecycleEvent, LifecycleState); 10] = [
    (
        LifecycleState::Created,
        LifecycleEvent::Start,
        LifecycleState::Starting,
    ),
    (
        LifecycleState::Created,
        LifecycleEvent::Stop,
        LifecycleState::Stopped,
    ),
    (
        LifecycleState::Starting,
        LifecycleEvent::Started,
        LifecycleState::Ready,
    ),
    (
        LifecycleState::Starting,
        LifecycleEvent::Drain,
        LifecycleState::Draining,
    ),
    (
        LifecycleState::Starting,
        LifecycleEvent::Stop,
        LifecycleState::Stopped,
    ),
    (
        LifecycleState::Ready,
        LifecycleEvent::Degrade,
        LifecycleState::Degraded,
    ),
    (
        LifecycleState::Ready,
        LifecycleEvent::Drain,
        LifecycleState::Draining,
    ),
    (
        LifecycleState::Degraded,
        LifecycleEvent::Recover,
        LifecycleState::Ready,
    ),
    (
        LifecycleState::Degraded,
        LifecycleEvent::Drain,
        LifecycleState::Draining,
    ),
    (
        LifecycleState::Draining,
        LifecycleEvent::Stop,
        LifecycleState::Stopped,
    ),
];

fn path_to(state: LifecycleState) -> &'static [LifecycleEvent] {
    match state {
        LifecycleState::Created => &[],
        LifecycleState::Starting => &[LifecycleEvent::Start],
        LifecycleState::Ready => &[LifecycleEvent::Start, LifecycleEvent::Started],
        LifecycleState::Degraded => &[
            LifecycleEvent::Start,
            LifecycleEvent::Started,
            LifecycleEvent::Degrade,
        ],
        LifecycleState::Draining => &[
            LifecycleEvent::Start,
            LifecycleEvent::Started,
            LifecycleEvent::Drain,
        ],
        LifecycleState::Stopped => &[LifecycleEvent::Stop],
    }
}

fn controller_at(state: LifecycleState) -> LifecycleController {
    let controller = LifecycleController::new(64).expect("valid controller");
    for event in path_to(state) {
        controller.transition(*event).expect("valid setup edge");
    }
    assert_eq!(controller.snapshot().expect("snapshot").state(), state);
    controller
}

#[test]
fn all_state_event_pairs_match_the_declared_relation() {
    let expected = EXPECTED_EDGES.into_iter().collect::<BTreeSet<_>>();
    let mut observed = BTreeSet::new();
    for state in ALL_STATES {
        for event in ALL_EVENTS {
            if let Some(target) = transition_target(state, event) {
                observed.insert((state, event, target));
            }
        }
    }
    assert_eq!(observed, expected);
    assert_eq!(observed.len(), 10);
}

#[test]
fn exhaustive_model_has_full_reachability_and_no_early_deadlock() {
    let report = validate_model().expect("model invariants hold");
    assert_eq!(report.reachable_states(), &ALL_STATES.into_iter().collect());
    assert_eq!(report.transition_count(), EXPECTED_EDGES.len());
    for state in ALL_STATES {
        let has_edge = ALL_EVENTS
            .into_iter()
            .any(|event| transition_target(state, event).is_some());
        assert_eq!(has_edge, !state.is_terminal());
    }
}

#[test]
fn every_undefined_edge_fails_closed() {
    for state in ALL_STATES {
        for event in ALL_EVENTS {
            if transition_target(state, event).is_some() {
                continue;
            }
            let controller = controller_at(state);
            let before = controller.snapshot().expect("snapshot");
            let audit_before = controller.audit().expect("audit");
            assert_eq!(
                controller.transition(event),
                Err(TransitionError::InvalidTransition { from: state, event })
            );
            assert_eq!(controller.snapshot().expect("snapshot"), before);
            assert_eq!(controller.audit().expect("audit"), audit_before);
        }
    }
}

#[tokio::test]
async fn shared_clones_publish_only_increasing_revisions() {
    let controller = LifecycleController::new(16).expect("valid controller");
    let mut updates = controller.subscribe();
    let writer = {
        let controller = controller.clone();
        tokio::task::spawn_blocking(move || {
            for event in [
                LifecycleEvent::Start,
                LifecycleEvent::Started,
                LifecycleEvent::Degrade,
                LifecycleEvent::Recover,
                LifecycleEvent::Drain,
                LifecycleEvent::Stop,
            ] {
                controller.transition(event).expect("valid edge");
                std::thread::yield_now();
            }
        })
    };

    let mut last_revision = updates.borrow_and_update().revision();
    while last_revision < 6 {
        updates.changed().await.expect("publisher remains open");
        let next = updates.borrow_and_update().revision();
        assert!(next > last_revision, "watch revision regressed or repeated");
        last_revision = next;
    }
    writer.await.expect("writer did not panic");
    assert_eq!(controller.snapshot().expect("snapshot").revision(), 6);
    assert_eq!(
        controller
            .audit()
            .expect("audit")
            .iter()
            .map(|record| record.revision())
            .collect::<Vec<_>>(),
        [1, 2, 3, 4, 5, 6]
    );
}

#[tokio::test]
async fn wait_is_timeout_and_cancellation_friendly_without_blocking_transition() {
    let controller = LifecycleController::new(8).expect("valid controller");
    assert_eq!(
        controller
            .wait_for(
                LifecycleState::Ready,
                Duration::from_millis(1),
                CancellationToken::new(),
            )
            .await,
        Err(WaitError::Timeout)
    );

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
    controller
        .transition(LifecycleEvent::Start)
        .expect("wait does not hold the controller lock");
    cancellation.cancel();
    assert_eq!(
        waiter.await.expect("waiter did not panic"),
        Err(WaitError::Cancelled)
    );
}
