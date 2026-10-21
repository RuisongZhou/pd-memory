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
- Android core/FFI must build and run on Android. Building with the NDK (r26+,
  via `cargo-ndk`) is fine, so dependencies that compile C/C++ are allowed; do
  not spawn processes or install signal handlers in that graph.
  `scripts/android-check.sh` needs an NDK toolchain once C code is in the graph.
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
