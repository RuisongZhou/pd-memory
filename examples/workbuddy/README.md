# WorkBuddy integration

Production installation and design: [installation guide](../../docs/hosts/workbuddy.md).

```bash
bash scripts/workbuddy/install.sh --apply --verify-model
```

The installer builds release, creates the private model config and wrapper, merges
GUI/CLI MCP and three user hooks, then checks the integration with isolated data.
It previews by default; use --apply to write, --rollback to restore a deployment.
The API key is entered through a hidden prompt, an existing environment variable,
or --key-file. Only explicit --mock selects the offline demo.

| File | Purpose |
|---|---|
| ../../scripts/install/ | Production installer (`common.py` + `hosts/workbuddy.py`), backup/rollback and validation |
| install_workbuddy.sh | Alternate entry point forwarding to the same installer |
| pd-memory.config.json | Offline demo SDK config, separate .run/data |
| workbuddy.mcp.json | Mock GUI MCP fragment; custom services use ~/.workbuddy/mcp.json |
| workbuddy.cli.mcp.json | Mock CodeBuddy CLI MCP fragment |
| hooks/settings.json, hooks/hooks.json | Mock hook fixtures for manual/plugin experiments |
| simulate_workbuddy.py | Contract simulator and real headless WorkBuddy CLI harness |
| run_e2e.sh | Build and run harness; local mock model, no cloud credentials |
| EVIDENCE.md | Verified scope, current deployment and historical protocol evidence |

```bash
# Installer tests
cargo build --locked -p pd-memory-cli
python3 -m unittest discover -s scripts/install/tests -p 'test_*.py' -v

# Host contract tests
bash examples/workbuddy/run_e2e.sh
bash examples/workbuddy/run_e2e.sh --no-real
```

The fixtures retain fake keys and placeholders; the production installer generates
its own wrapper commands and does not load these mock fragments. Never commit
real config, API keys, transcripts or generated memories.
