//! Loom interleaving checks against the production transition function.

use loom::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use ores_mcp_server_core_libs::state_machine::{LifecycleEvent, LifecycleState, transition_target};

#[test]
fn concurrent_transition_and_publication_is_linearizable() {
    loom::model(|| {
        let data = Arc::new(Mutex::new((
            LifecycleState::Ready,
            2_u64,
            Vec::<u64>::new(),
        )));
        let published_revision = Arc::new(AtomicU64::new(2));

        let degrade = {
            let data = data.clone();
            let published_revision = published_revision.clone();
            loom::thread::spawn(move || {
                apply_model_transition(&data, &published_revision, LifecycleEvent::Degrade);
            })
        };
        let drain = {
            let data = data.clone();
            let published_revision = published_revision.clone();
            loom::thread::spawn(move || {
                apply_model_transition(&data, &published_revision, LifecycleEvent::Drain);
            })
        };

        degrade.join().expect("degrade thread");
        drain.join().expect("drain thread");
        let data = data.lock().expect("model lock");
        assert_eq!(data.0, LifecycleState::Draining);
        assert!(matches!(data.1, 3 | 4));
        assert!(data.2.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(published_revision.load(Ordering::SeqCst), data.1);
    });
}

fn apply_model_transition(
    data: &Mutex<(LifecycleState, u64, Vec<u64>)>,
    published_revision: &AtomicU64,
    event: LifecycleEvent,
) {
    let mut data = data.lock().expect("model lock");
    if let Some(target) = transition_target(data.0, event) {
        data.0 = target;
        data.1 += 1;
        let revision = data.1;
        data.2.push(revision);
        // Production publishes while holding the same transition lock.
        published_revision.store(revision, Ordering::SeqCst);
    }
}
