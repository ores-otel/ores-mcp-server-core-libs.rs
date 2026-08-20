# Lifecycle formal model

`McpLifecycle.tla` mirrors the pure Rust `transition_target` relation. The TLC
configuration bounds revisions at 12 and the audit ring at 3, then checks:

- state, revision, event, and audit-record types;
- the audit-capacity invariant;
- strictly increasing retained audit revisions;
- agreement between the latest audit revision and the snapshot revision;
- terminality of `stopped`.

Run the same pinned checker used by CI:

```sh
curl -fsSL --proto '=https' --tlsv1.2 \
  https://github.com/tlaplus/tlaplus/releases/download/v1.7.4/tla2tools.jar \
  -o /tmp/tla2tools-1.7.4.jar
printf '%s  %s\n' \
  '936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88' \
  /tmp/tla2tools-1.7.4.jar | sha256sum -c -
java -XX:+UseParallelGC -jar /tmp/tla2tools-1.7.4.jar \
  -config formal/McpLifecycle.cfg formal/McpLifecycle.tla
```

TLC's revision bound makes the cyclic model finite. Full finite-graph
reachability and nonterminal-deadlock checks do not need that bound and are
performed exhaustively by Rust tests over every state/event pair. Loom explores
the relevant controller publication interleavings against the same pure Rust
transition function.
