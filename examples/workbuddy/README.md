# WorkBuddy integration examples

Quick map of this directory (full guide: [`../../docs/workbuddy.md`](../../docs/workbuddy.md),
upstream source: [`../../docs/upstream.md`](../../docs/upstream.md),
latest run evidence: [`EVIDENCE.md`](EVIDENCE.md)).

| File | Purpose |
|---|---|
| `pd-memory.config.json` | demo SDK config; `data_root` is relative to this file (`.run/data`), `PD_MEMORY_PROVIDER=mock` |
| `workbuddy.mcp.json` | `mcpServers` fragment for the WorkBuddy **GUI connector registry** (`~/.workbuddy/connectors/default/mcp.json`) |
| `workbuddy.cli.mcp.json` | `mcpServers` + `disabledMcpServers` fragment for the CodeBuddy CLI native `.mcp.json` |
| `hooks/hooks.json` | plugin-level hooks (`<plugin>/hooks/hooks.json`) |
| `hooks/settings.json` | full settings object for `--settings` / user settings (`~/.workbuddy/settings.json` → `"hooks"`) |
| `hooks/README.md` | verified vs unverified matrix, install instructions, event mapping |
| `install_workbuddy.sh` | dry-run by default; `--apply` merges MCP(+hooks) with backups; `--revert` restores |
| `simulate_workbuddy.py` | end-to-end harness: contract simulator + real headless WorkBuddy CLI route |
| `run_e2e.sh` | build + harness + tee to `EVIDENCE.log` |
| `.gitignore` | run state/backups are never committed |

One-command verification:

```bash
cd pd-memory
bash examples/workbuddy/run_e2e.sh
```

Passing `--verbose` echoes raw stdout/stderr, `--no-real` runs only the offline
contract simulator, `--only-real` runs only the real WorkBuddy CLI route.
