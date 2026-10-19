# WorkBuddy hook fixtures

These are **mock-only fixtures** for manual and plugin experiments:

- settings.json: user/project settings object with a top-level hooks map.
- hooks.json: plugin form with description and hooks.

The production installer generates its own commands pointing to the installed
pd-memory-workbuddy wrapper. It does not substitute or load these mock templates.

```bash
bash scripts/workbuddy/install.sh --apply --verify-model
```

Production installs wire only the three verified events: SessionStart,
UserPromptSubmit and Stop. SessionEnd and PreCompact remain in these fixtures
for manual experiments; their host triggering has not been verified.

Hook stdin uses snake_case (hook_event_name, session_id, transcript_path, cwd,
prompt). Stdout is one JSON object with continue=true and optional
hookSpecificOutput.additionalContext. Both MCP and hooks use the same private
model configuration and pinned workspace.

See [installation and protocol guide](../../../docs/workbuddy.md) and
[verification evidence](../EVIDENCE.md). A CLI hook invocation, successful hook
output, and actual GUI session injection are separate verification claims.
