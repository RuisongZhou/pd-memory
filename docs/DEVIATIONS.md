# pd-memory — parity deviations register

This SDK is a Rust reimplementation of PilotDeck's memory system. The behavior
baseline is the live TypeScript core under `src/context/memory/edgeclaw-memory-core/src`.
This register records resolved differences and remaining SDK scope boundaries;
it does not grant exemptions to the differential tests.

## 1. Runtime / storage

| # | Deviation | Rationale | Impact |
|---|---|---|---|
| 1 | Control plane defaults to **JSONL + JSON** (`control/l0_sessions.jsonl`, `pipeline_state.json`) instead of `node:sqlite`. SQLite is an optional desktop crate (`pd-memory-sqlite`). | Android: no native C toolchain needed, crash-safe appends, inspectable files. Ports (`QueueStore`/`StateStore`) keep semantics identical (`indexed` flag, timestamp ordering, KV state). | No behavior change observable through the engine; bulk SQL tooling is unavailable unless SQLite mode is used. |
| 2 | `count_pending_turns` counts **pending L0 rows** (`indexed = 0`), matching the reference (`countPendingDialogueTurns`). | Parity. | Same trigger thresholds. |
| 3 | L0 enqueue uses the reference upsert semantics; duplicate capture is filtered before enqueue. | Rechecked against `heartbeat.ts` and the JSON control plane. | Duplicate capture does not re-index; flush counts are now compared without an exception. |
| 4 | The JSON control plane locks individual queue/state mutations with `control/.lock`. | Replaces SQLite transactions for those operations. | This is not a process-wide flush/Dream lock; callers must serialize operations against shared roots, as with the reference host. |
| 5 | Files are written tmp+rename; dream commits by directory swap; JSONL appends are the only non-atomic write (torn trailing line repaired). | Crash safety requirement (spec §9.1 #7 / §9.2). | Higher robustness; no format change. |
| 6 | SQLite control plane (optional) uses the same two tables, but its own package. | Desktop parity / inspection. | Not used by default or on Android. |
| 28 | Memory file names and their tmp/backup siblings are capped at 255 bytes on every platform: the slug is cut on a character boundary so `<slug><suffix>` fits (241 bytes of slug for `-<hash10>.md`; less for `general_projects/` names with a long project id), and the sibling embeds a shortened copy of the target name. | Linux/Android filesystems reject longer names; a CJK character is 3 bytes, so a name of 72+ CJK characters already overflowed the tmp sibling and Dream's 120-character names overflowed the file itself (`File name too long`). | Names whose file name already fit in 255 bytes (every extraction-sized note) are unchanged. Longer slugs failed on Linux/Android; macOS/APFS, which counts characters, used to write them in full like the reference, so there such names (e.g. Dream names of 81-120 CJK characters) now differ from the reference. |

## 2. Memory content & formatting

| # | Deviation | Rationale | Impact |
|---|---|---|---|
| 7 | **Resolved:** global-user manifest links use filesystem-relative paths, matching `resolveManifestLinkPath`. | Restore upstream rendering for the actual memory/global roots. | Manifest text and files are compared without link rewriting or exemptions. |
| 8 | `deprecated: false` and `dream_attempts: 0` are omitted from rendered frontmatter (Rust fields are non-optional). | Semantically identical; omitting matches the reference's "write only when set" rule for the default values. | Parses to the same state in both implementations. |
| 9 | `project.meta.md` additionally persists `project_id` / `source_*` keys. | Needed to round-trip `ProjectMetaRecord`; forward compatible (unknown keys ignored by the TS parser). | No behavior change for note parsing. |
| 10 | **Resolved:** final context uses `build_memory_recall_system_context`, matching the reference four-section renderer and current-user-instruction priority. | Restore upstream behavior. | Final injected text is compared directly; no rendering exception remains. |
| 11 | **Resolved:** snapshots include before/after Dream state; rollback installs a reverse snapshot and restores the saved state. | Restore upstream rollback/redo behavior. | Files, repeated rollback, exact per-side restored state and snapshot preservation are tested. |
| 12 | **Resolved:** nonempty normalized capture returns `captured:true`, including queue duplicates; empty input returns false. | Match the PilotDeck service facade while preserving queue deduplication. | CLI/MCP forward the result without duplicate annotations; capture flags are compared without exceptions. |
| 13 | **Resolved:** no-op Dream returns success without replacing an existing snapshot; failures leave previous Dream state intact. | Match `service.ts` and `dream-review.ts`. | Empty/nonempty no-op and no-op-after-commit scenarios are compared without status exceptions. |

## 3. Pipeline semantics

| # | Deviation | Rationale | Impact |
|---|---|---|---|
| 14 | **Resolved:** every new focus user turn is eligible for extraction, including unanswered turns. | Match `heartbeat.ts`. | P8 verifies extraction before a delayed assistant reply and no re-extraction afterwards. |
| 15 | **Resolved:** classify/create failures degrade to no classification / no note; L0 records are marked indexed. Persistence errors affect only the failing record and are returned in stats. | Match `llm-extraction.ts` and `heartbeat.ts`. | No model-failure replay; later records continue after a write failure. |
| 16 | **Resolved:** JSON parsing does not request model repairs; create-note payloads use the reference loose-string fallback. | Match the live parser rather than the previous SDK policy. | Invalid/fenced/malformed note output and request counts are covered. |
| 17 | **Resolved:** explicit search uses the same model gate as automatic recall. | Match `service.search()` and `ReasoningRetriever.retrieve()`. | Project, user and none routes are checked; no search-route exception remains. |
| 18 | **Resolved:** `processed_sessions` counts successful L0 records, matching `capturedSessions`; focus turns remain `processed_turns`. | Preserve batch accounting and capture-time index cursors. | Flush counts and `lastIndexedAt` are compared directly. |
| 19 | **Resolved:** recall keys hash full exported file contents and last Dream time; TTL starts at completion, with no added eviction cap. Flush and settings changes reset the cache. | Match `ReasoningRetriever`. | Persistent-instance probes cover hits, manual body edits, TTL, mode/recent-message keys and cleanup. |
| 20 | **Resolved:** the added BM25 prefilter and its config fields were removed. | Restore manifest -> model selection. | Candidate ids/order and final selection are compared directly, including a query without lexical overlap. |
| 21 | **Resolved:** Dream cluster-reason checks match the reference patterns, including `同属.*项目` and `redundancy`. | Remove overbroad / incomplete substring matches. | Unit cases and P17 verify accepted group-membership reasons. |

| 24 | **Resolved:** scheduling reconciles anchors from pending captures / last index, uses the injected clock, and reevaluates Dream after indexing. | Match `service.ts`. | Missing/invalid anchors, manual resets, filtered flush, thresholds, reasons and global-user-only writes are checked. |
| 25 | **Resolved:** default model attempts are three; only the reference HTTP statuses retry, and OpenAI chat falls back without `response_format` after request failure. | Match `callStructuredJson`. | Offline transport tests and P16 verify fallback and parse-error request counts. |
| 26 | **Resolved:** clear removes queue/runtime state (current-project clear preserves settings and workspace path); import resets the queue and imported runtime metadata. | Match repository cleanup. | Persistent probes prevent stale backlog, timestamps and cached recall after cleanup. |
| 27 | **Resolved:** explicit batch size is not capped at 30; pending/prefix reads are not truncated, blank preferred session keys are ignored, and equal-age sessions sort by key. | Match repository selection and previous-row lookup. | Batch-over-30 and prefix-over-200 regressions are tested. |

## 4. Scope

| # | Deviation | Rationale | Impact |
|---|---|---|---|
| 22 | `workspace_mode = "general"` (multi-project overview, `GeneralProjects/`, external mirrors, cross-project routing) is **not implemented**; config validation returns a clear error. | Spec §9.2 marks it safely removable for single-project products. | Users of PilotDeck's IM/channel general mode need a later milestone. |
| 23 | No UI, no case traces / i18n trace payloads; traces are structured JSON rings readable by hosts. | SDK does not embed a UI. | Hosts render their own UI from `snapshot`/traces. |
| 24 | Model adapters: `openai-chat` + `anthropic` + `mock`; `openai-responses` / `google` report `Unsupported`. | Spec §9.2 suggests narrowing to one/two wire formats first. | Hosts using those providers must point at a compatible endpoint or add an adapter (one trait impl). |
| 25 | Tool surface adds MCP (`memory_*` 7 tools) and lifecycle hooks that the reference never wired. | This is the product goal of the SDK. | Purely additive. |

## 5. Known verification gaps

- Android: 3 cross-target `cargo check` runs pass **without NDK**; a real `.so`
  link + Gradle/AGP/Kotlin build + on-device run still need an NDK/SDK machine
  (see `android/README.md`).
- WorkBuddy: verified headlessly against WorkBuddy 5.8.2 / CodeBuddy CLI 2.161.0
  (hooks SessionStart/UserPromptSubmit/Stop, MCP stdio, injection into the model
  request). GUI injection and `SessionEnd`/`PreCompact` firing were not observed
  in `-p` mode (see `examples/workbuddy/EVIDENCE.md`).
- Real-model extraction quality was not evaluated (no API keys used); the SDK
  ships with a deterministic mock for CI, and the evidence run uses it.
- `clippy` is not installed on this machine; `cargo fmt --check` and
  warning-free `cargo check --all-targets` are enforced instead.

## 6. Current differential coverage

The single-project harness covers P1–P17 with **no behavioral exemptions**.
Manifest paths and text, capture flags, flush counts, recall candidate order,
gate routes, selected ids, final injected context, no-op Dream status, and
rollback/redo, failure handling, scheduling anchors and persistent recall-cache
behavior are checked directly. Only the replaceable queue/state storage
format (#1) is informational; it does not waive observable output differences.

User-only profile consolidation also exposed missing project-meta seeding in
the Rust Dream stage. This now matches the reference, and overview comparisons
read profile presence at the step being compared rather than at scenario end.
See `docs/parity.md` for reproduction, exact scope and remaining boundaries.
