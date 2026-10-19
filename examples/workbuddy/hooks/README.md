# WorkBuddy / CodeBuddy hooks for pd-memory

This directory ships two encodings of the same hook wiring:

| File | Use it for |
|---|---|
| `hooks.json` | plugin-level file (`<plugin>/hooks/hooks.json`, declared from `.codebuddy-plugin/plugin.json` with `"hooks": "./hooks/hooks.json"`). Wrapped in `{"description","hooks"}`. |
| `settings.json` | a full settings object (`{"hooks": {...}}`) for `--settings <file>` or to merge into user/project settings under the top-level `"hooks"` key. |

All `__PD_MEMORY_*__` placeholders are substituted by
`examples/workbuddy/install_workbuddy.sh --with-hooks` (or by hand). The
generated command is:

```sh
env PD_MEMORY_PROVIDER=mock PD_MEMORY_DATA_ROOT=<data-root> \
    <pd-memory-binary> --config <sdk-config.json> hook
```

`pd-memory hook` reads exactly one JSON object from stdin (WorkBuddy/CodeBuddy
use snake_case keys: `hook_event_name`, `session_id`, `transcript_path`, `cwd`,
`prompt`, ...) and prints one JSON object on stdout
(`continue`, `systemMessage`, `hookSpecificOutput.additionalContext`, ...).

## Verified vs. unverified

All facts below were checked against WorkBuddy 5.8.2 / bundled CodeBuddy Code
CLI **2.161.0** on this machine. Commands used are listed in
`docs/workbuddy.md` and `docs/upstream.md`.

| Claim | Status | Evidence |
|---|---|---|
| Hooks config accepted in `settings.json` under top-level `"hooks"` | **Verified** | CLI bundle 2.161.0: `settingsManager.get("hooks")` merges with plugin hooks; our own headless run with `CODEBUDDY_CONFIG_DIR=<tmp>` fired `SessionStart` + `UserPromptSubmit` (marker file written) |
| Hook group schema `{ "<Event>": [ { "matcher"?: "<regex>", "hooks": [ { "type": "command", "command": "...", "timeout": 15 } ] } ] }` | **Verified** | real plugin files under `~/.workbuddy/plugins/**/hooks/hooks.json`, plus CLI zod schema strings in `codebuddy-lite-wb.mjs` |
| `SessionStart`, `UserPromptSubmit`, `Stop` fire in a headless `-p` run | **Verified** | real payloads captured by the research subagent, §3.5 of `docs/upstream.md` |
| `SessionEnd`, `PreCompact` fire in a headless `-p` run | **Unverified / not fired** | not triggered in a single `-p` run; both events exist in the runtime enum and have known payload fields (`reason`, `trigger`). They should fire on interactive exit / context compaction in the GUI, but this was not exercised (GUI not started) |
| GUI loads hooks from `~/.workbuddy/settings.json` | **Expected, not GUI-verified** | daemon log shows GUI sidecar starts the CLI with `--setting-sources=user`; user settings resolve to `CODEBUDDY_CONFIG_DIR/settings.json` (= `~/.workbuddy/settings.json` when launched by WorkBuddy). Not observed in a live GUI session because the GUI was intentionally not restarted |
| GUI loads project-level `.codebuddy/settings.json` hooks | **No** (by design) | GUI passes `--setting-sources=user` only |
| Plugin directory hooks are loaded (CLI `--plugin-dir`) | **Verified** | CLI `--plugin-dir` run fired plugin `SessionStart`/`Stop` hooks (research doc §2.3) |
| Installing a plugin into the GUI requires registry/marketplace plumbing | **Unverified** | plugin directories under `~/.workbuddy/plugins/cache/**` are written by the GUI plugin manager; hand-copying a plugin was not attempted |

## Install (choose one)

### 1. User-level hooks (GUI-compatible, backup + merge)

```bash
cd pd-memory
# dry-run first: prints the merged settings.json diff, touches nothing
examples/workbuddy/install_workbuddy.sh --with-hooks
# apply (backs up ~/.workbuddy/settings.json to settings.json.bak.<timestamp>)
examples/workbuddy/install_workbuddy.sh --apply --with-hooks
# revert to the newest backup (keeps the pre-revert state as another backup)
examples/workbuddy/install_workbuddy.sh --revert
```

The merge is idempotent: hook groups whose `description` starts with
`pd-memory:` are removed and re-added, all other events/groups are preserved.
If `~/.workbuddy/settings.json` does not exist it is created only with
`--apply`.

### 2. Project-level hooks (CLI only)

```bash
mkdir -p <project>/.codebuddy
# merge the `"hooks"` value of hooks/settings.json under "hooks" in <project>/.codebuddy/settings.json
```

Set `--setting-sources project` (or the default `user,project,local`) when
running the CLI. The WorkBuddy GUI will **not** load these (it passes
`--setting-sources=user`).

### 3. One-shot settings file (best for tests)

```bash
NODE=~/.workbuddy/binaries/node/versions/22.22.2-6/bin/node
CLI=/Applications/WorkBuddy.app/Contents/Resources/app.asar.unpacked/cli/bin/codebuddy
CODEBUDDY_FORCE_LITE_WB_BUNDLE=1 CODEBUDDY_CONFIG_DIR=$TDIR WORKBUDDY_CONFIG_DIR=$TDIR \
  "$NODE" "$CLI" -p 'hello' --model mock-model \
  --settings examples/workbuddy/hooks/settings.json --output-format json
```

### 4. Plugin (ships MCP + hooks together; recommended distribution shape)

```
pd-memory-plugin/
  .codebuddy-plugin/plugin.json   # {"name":"pd-memory","version":"0.1.0","hooks":"./hooks/hooks.json","mcpServers":"./.mcp.json"}
  hooks/hooks.json                # copy of this directory's hooks.json with placeholders filled
  .mcp.json                       # mcpServers fragment, see ../workbuddy.cli.mcp.json
```

Load with `--plugin-dir <dir>` for headless runs. Installing into the GUI's
plugin registry was not verified (see table).

## Event → pd-memory action mapping

| Event | Typical trigger | pd-memory action |
|---|---|---|
| `SessionStart` | new CLI process / resumed session | recall: inject project memory as `hookSpecificOutput.additionalContext` |
| `UserPromptSubmit` | user sends a prompt | recall: inject relevant memory before the model call |
| `Stop` | assistant finishes a turn | capture `transcript_path` / `last_assistant_message`, queue turn, flush when due |
| `SessionEnd` | session ends (not fired in `-p`) | capture the tail of the session |
| `PreCompact` | context compaction (not fired in `-p`) | flush/dream so nothing is lost when the transcript is summarized |

`matcher` is a **regex** (empty / `"*"` / absent matches everything). For
`SessionStart` it matches `source` (`startup`, `resume`, `clear`), for
`PreCompact` it matches `trigger` (`auto` / `manual`); for `UserPromptSubmit`
and `Stop` matchers are ignored. The shipped configs omit matchers so all
sources are captured.
