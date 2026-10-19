# pd-memory working agreement

This is the standalone Rust SDK extracted from PilotDeck. The behavioral reference
is the memory core in a separate PilotDeck checkout; see `upstream.json` and
`docs/parity.md`. Keep behavior aligned without adding capabilities implicitly.

- Blocking + threads; no async runtime in the engine.
- Markdown is the memory source of truth; JSONL/JSON is the default control plane.
- Inject model, store, clock, telemetry and redaction ports. No environment or home
  directory reads inside the engine.
- Use atomic writes and staged directory replacement. Return errors, never panic
  in library code.
- Android core/FFI default features must compile without an NDK; no process,
  signals or SQLite dependency in that default graph.
- `cargo test` must be deterministic and offline. Use injected models and temp dirs.
- Preserve local user changes and never publish credentials or generated memory.

Run from this repository root:

```bash
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets
cargo test --locked --workspace
bash scripts/android-check.sh
PILOTDECK_ROOT=/path/to/PilotDeck bash examples/parity/run_parity.sh
```

Run clippy with warnings denied if installed. Report tests, actual coverage and
remaining limits. Do not claim real-model quality or GUI validation from mock tests.
