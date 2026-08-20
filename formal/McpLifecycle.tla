----------------------------- MODULE McpLifecycle -----------------------------
EXTENDS Naturals, Sequences, TLC

(***************************************************************************
Formal model corresponding to src/state_machine.rs::transition_target.

TLC explores bounded revision histories because Ready <-> Degraded permits an
unbounded number of valid transitions. The Rust model checker separately
explores the complete finite state graph without a revision bound.
***************************************************************************)

CONSTANTS AuditCapacity, MaxRevision

States == {"created", "starting", "ready", "degraded", "draining", "stopped"}
Events == {"start", "started", "degrade", "recover", "drain", "stop"}

VARIABLES state, revision, audit
vars == <<state, revision, audit>>

Init ==
    /\ state = "created"
    /\ revision = 0
    /\ audit = <<>>

BoundedAppend(entry) ==
    LET extended == Append(audit, entry)
    IN IF Len(extended) <= AuditCapacity
       THEN extended
       ELSE SubSeq(extended, Len(extended) - AuditCapacity + 1, Len(extended))

Step(event, target) ==
    /\ state' = target
    /\ revision' = revision + 1
    /\ audit' = BoundedAppend([
           revision |-> revision + 1,
           from |-> state,
           event |-> event,
           to |-> target
       ])

Start ==
    /\ state = "created"
    /\ Step("start", "starting")

StopBeforeReady ==
    /\ state \in {"created", "starting"}
    /\ Step("stop", "stopped")

Started ==
    /\ state = "starting"
    /\ Step("started", "ready")

Degrade ==
    /\ state = "ready"
    /\ Step("degrade", "degraded")

Recover ==
    /\ state = "degraded"
    /\ Step("recover", "ready")

Drain ==
    /\ state \in {"starting", "ready", "degraded"}
    /\ Step("drain", "draining")

StopAfterDrain ==
    /\ state = "draining"
    /\ Step("stop", "stopped")

Next ==
    \/ Start
    \/ StopBeforeReady
    \/ Started
    \/ Degrade
    \/ Recover
    \/ Drain
    \/ StopAfterDrain

Spec == Init /\ [][Next]_vars

RevisionBound == revision <= MaxRevision

AuditRecordType == [
    revision : Nat,
    from : States,
    event : Events,
    to : States
]

TypeOK ==
    /\ state \in States
    /\ revision \in Nat
    /\ audit \in Seq(AuditRecordType)

AuditBounded == Len(audit) <= AuditCapacity

AuditStrictlyOrdered ==
    \A i, j \in 1..Len(audit) : i < j => audit[i].revision < audit[j].revision

AuditLatestMatchesRevision ==
    IF Len(audit) = 0
    THEN revision = 0
    ELSE audit[Len(audit)].revision = revision

StoppedIsTerminal == state = "stopped" => ~ENABLED Next

=============================================================================
