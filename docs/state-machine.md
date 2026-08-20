# MCP lifecycle state machine

Every transport and every read-only runtime status tool should share clones of
one `LifecycleController`. A clone shares the same internal state; it does not
fork the lifecycle.

```rust
use ores_mcp_server_core_libs::state_machine::{
    LifecycleController, LifecycleEvent,
};

let lifecycle = LifecycleController::new(128)?;
let handler_lifecycle = lifecycle.clone();
let transport_lifecycle = lifecycle.clone();

transport_lifecycle.transition(LifecycleEvent::Start)?;
// Bind and validate the selected transport.
transport_lifecycle.transition(LifecycleEvent::Started)?;

// Read both under one lock when they must describe the same revision.
let (snapshot, audit) = handler_lifecycle.snapshot_and_audit()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Transition relation

| From | Event | To |
| --- | --- | --- |
| `created` | `start` | `starting` |
| `created` | `stop` | `stopped` |
| `starting` | `started` | `ready` |
| `starting` | `drain` | `draining` |
| `starting` | `stop` | `stopped` |
| `ready` | `degrade` | `degraded` |
| `ready` | `drain` | `draining` |
| `degraded` | `recover` | `ready` |
| `degraded` | `drain` | `draining` |
| `draining` | `stop` | `stopped` |

Every edge not listed is rejected without changing the snapshot, revision,
audit, or watch value. `stopped` is terminal.

## Concurrency contract

`transition` is the linearization point. State mutation, revision allocation,
bounded audit insertion, and `tokio::sync::watch` publication happen while the
same short synchronous lock is held. Therefore concurrent successful updates
cannot publish an older revision after a newer one. The lock is never held
across an await.

Watch receivers are coalescing: a slow reader may skip intermediate revisions,
but any revisions it observes are monotonic. The audit ring is the bounded
source for recent individual transitions. Records contain only revision and
closed state/event enums—never MCP arguments, model data, errors, secrets, or
other payloads.

`wait_for` observes the same watch stream and accepts both an overall timeout
and a cancellation token. Callers must treat timeout and cancellation as normal
control flow rather than mutating the lifecycle spec.

## What is mechanically checked

- Rust exhaustively evaluates all 36 state/event pairs and independently checks
  target closure, reachability of every declared state, terminality, and absence
  of reachable nonterminal deadlocks.
- Controller tests verify fail-closed invalid transitions, bounded ordered audit
  history, clone sharing, async timeout/cancellation, and monotonic watch output.
- Loom explores concurrent transition/publication orderings against the same
  pure `transition_target` function used in production.
- TLC 1.7.4, downloaded from a tag-pinned URL and SHA-256 verified in CI,
  explores revision-bounded histories and checks audit/type/terminal invariants.

TLC is deliberately bounded because the `ready`/`degraded` recovery cycle can
advance revisions forever. The unbounded topology claims are checked on the
finite state graph by Rust; the bounded TLA+ model checks the revisioned audit
behavior across representative cyclic traces. Neither model proves correctness
of networking libraries, provider APIs, or downstream handler code.
