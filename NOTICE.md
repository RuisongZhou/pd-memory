# NOTICE — provenance and licensing

pd-memory is extracted from the PilotDeck SDK workspace and distributed under
AGPL-3.0-only. The original repository license is retained in [LICENSE](LICENSE).

- Behavioral reference: [PilotDeck](https://github.com/OpenBMB/PilotDeck), especially
  `src/context/memory/edgeclaw-memory-core`; the exact verified revision and
  source/build digests are recorded in [upstream.json](upstream.json).
- The reference also builds on [ClawXMemory](https://github.com/OpenBMB/ClawXMemory).
  pd-memory is a Rust reimplementation with compatible memory formats and flows.
  No PilotDeck TypeScript source or compiled JavaScript is vendored here.
- Third-party Rust dependency versions are recorded in Cargo.lock. Each dependency
  retains its own license and copyright notices.
- PilotDeck, ClawXMemory, WorkBuddy and CodeBuddy are names of their respective
  projects/products. This project does not claim affiliation with them.
