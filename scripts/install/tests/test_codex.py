"""Offline Codex installation tests; run after cargo build -p pd-memory-cli.

Tests that need the real `codex` CLI (to parse the files and report hook
trust) skip when it is not on PATH. They only use a temporary CODEX_HOME.
"""

import contextlib
import io
import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import common  # noqa: E402
from hosts import codex as installer  # noqa: E402

BINARY = Path(os.environ.get("PD_MEMORY_TEST_BINARY", common.ROOT / "target/debug/pd-memory"))
# The legacy entry point is a shim for `scripts/install.sh --host codex`.
ENTRY = common.ROOT / "scripts/codex/install.sh"
WRAPPER = Path("/home/me/.local/bin/pd-memory-codex")
CODEX = shutil.which("codex")

CONFIG = """\
model = "gpt-test"
# a user comment that must survive
notify = ["/Applications/Notifier.app/notify", "turn-ended"]

[mcp_servers.zotero]
command = "zotero-mcp"

[mcp_servers.zotero.env]
ZOTERO_LOCAL = "true"

[projects."/Users/me/code"]
trust_level = "trusted"
"""


def toml(text):
    return installer.tomllib.loads(text)


class TomlMergeTests(unittest.TestCase):
    def test_adds_table_and_keeps_every_other_byte(self):
        merged = installer.merge_config_toml(CONFIG, WRAPPER)
        self.assertTrue(merged.startswith(CONFIG))
        parsed = toml(merged)
        self.assertEqual(parsed["notify"], toml(CONFIG)["notify"])
        self.assertEqual(parsed["mcp_servers"]["zotero"], toml(CONFIG)["mcp_servers"]["zotero"])
        self.assertEqual(parsed["mcp_servers"]["pd-memory"], {
            "command": str(WRAPPER), "args": ["serve", "mcp"],
            "startup_timeout_sec": 30, "tool_timeout_sec": 300,
        })
        self.assertEqual(installer.merge_config_toml(merged, WRAPPER), merged)
        self.assertEqual(installer.merge_config_toml(merged, None), CONFIG)

    def test_replaces_in_place_when_codex_appended_tables_after_ours(self):
        old = installer.merge_config_toml(CONFIG, Path("/old/pd-memory-codex"))
        old += '\n# trust\n[hooks.state."/home/me/.codex/hooks.json:stop:0:0"]\ntrusted_hash = "sha256:1"\n'
        merged = installer.merge_config_toml(old, WRAPPER)
        self.assertEqual(merged, old.replace("/old/pd-memory-codex", str(WRAPPER)))
        removed = installer.merge_config_toml(merged, None)
        self.assertNotIn("pd-memory", removed)
        self.assertIn('\n\n# trust\n[hooks.state.', removed)
        self.assertEqual(toml(removed)["hooks"], toml(old)["hooks"])

    def test_quoted_header_and_subtables_are_replaced(self):
        text = CONFIG + '\n[mcp_servers."pd-memory"]\ncommand = "old"\n\n[mcp_servers."pd-memory".env]\nX = "1"\n'
        merged = installer.merge_config_toml(text, WRAPPER)
        self.assertNotIn('"old"', merged)
        self.assertNotIn("env", toml(merged)["mcp_servers"]["pd-memory"])

    def test_empty_or_missing_config(self):
        merged = installer.merge_config_toml("", WRAPPER)
        self.assertTrue(merged.startswith("[mcp_servers.pd-memory]\n"))
        self.assertEqual(installer.merge_config_toml(merged, None), "")
        self.assertEqual(installer.merge_config_toml(CONFIG, None), CONFIG)

    def test_unsafe_documents_are_refused(self):
        for text in [
            'mcp_servers = { pd-memory = { command = "x" } }\n',
            '[mcp_servers]\npd-memory.command = "x"\n',
            'mcp_servers = "not a table"\n',
            "not = [valid toml\n",
        ]:
            with self.assertRaises(common.InstallError, msg=text):
                installer.merge_config_toml(text, WRAPPER)

    def test_paths_needing_escapes_round_trip(self):
        wrapper = Path('/home/me "quoted" \\ dir/ünïcode/pd-memory-codex')
        merged = installer.merge_config_toml(CONFIG, wrapper)
        self.assertEqual(toml(merged)["mcp_servers"]["pd-memory"]["command"], str(wrapper))


class HookMergeTests(unittest.TestCase):
    def ours(self, event):
        return installer.our_hook(event, WRAPPER)

    def test_preserves_other_hooks_and_is_idempotent(self):
        other = {"type": "command", "command": "echo keep"}
        original = {"description": "mine", "hooks": {
            "Stop": [{"hooks": [other]}],
            "PreToolUse": [{"matcher": "Bash", "hooks": [other]}],
        }}
        merged = installer.merge_hooks(original, WRAPPER, Path("/bin/pd-memory"))
        self.assertEqual(merged["description"], "mine")
        self.assertEqual(merged["hooks"]["PreToolUse"], original["hooks"]["PreToolUse"])
        self.assertEqual(merged["hooks"]["Stop"], [{"hooks": [other]}, {"hooks": [self.ours("Stop")]}])
        for event, timeout in installer.EVENTS.items():
            self.assertEqual(merged["hooks"][event][-1]["hooks"][0]["timeout"], timeout)
        self.assertEqual(merged["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"], 3)
        self.assertEqual(installer.merge_hooks(merged, WRAPPER, Path("/bin/pd-memory")), merged)
        self.assertEqual(
            installer.merge_hooks(merged, WRAPPER, Path("/bin/pd-memory"), install=False), original,
        )

    def test_existing_group_is_replaced_in_place_to_keep_trust_keys(self):
        stale = {"type": "command", "command": shlex.quote(str(WRAPPER)) + " hook", "timeout": 9}
        later = {"hooks": [{"type": "command", "command": "echo added later"}]}
        current = {"hooks": {"Stop": [{"hooks": [stale]}, later]}}
        merged = installer.merge_hooks(current, WRAPPER, Path("/bin/pd-memory"))
        self.assertEqual(merged["hooks"]["Stop"], [{"hooks": [self.ours("Stop")]}, later])

    def test_mixed_group_keeps_foreign_handlers(self):
        mixed = {"matcher": "keep", "hooks": [
            {"type": "command", "command": "env PD_MEMORY_PROVIDER=mock /bin/pd-memory hook"},
            {"type": "command", "command": "echo pd-memory-codex hook --codex"},
        ]}
        merged = installer.merge_hooks({"hooks": {"Stop": [mixed]}}, WRAPPER, Path("/bin/pd-memory"))
        self.assertEqual(merged["hooks"]["Stop"][0], {"matcher": "keep", "hooks": [mixed["hooks"][1]]})
        self.assertEqual(merged["hooks"]["Stop"][1], {"hooks": [self.ours("Stop")]})

    def test_bad_shapes_are_rejected(self):
        for value in [{"hooks": []}, {"hooks": {"Stop": {}}}, {"hooks": {"Stop": [{"hooks": "x"}]}},
                      {"hooks": {"Stop": [{"hooks": ["x"]}]}}]:
            with self.assertRaises(common.InstallError):
                installer.merge_hooks(value, WRAPPER, Path("/bin/pd-memory"))


class InstallationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if not BINARY.is_file():
            raise RuntimeError("Run cargo build -p pd-memory-cli before installer tests")

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="pd-memory-codex-test-")
        self.addCleanup(self.tmp.cleanup)
        self.home = Path(self.tmp.name) / "home with 'quotes' $dollar"
        self.codex = self.home / ".codex"
        self.codex.mkdir(parents=True)
        self.project = self.home / "project"
        self.project.mkdir()
        self.state = self.home / ".local/state/pd-memory/installations/codex"
        self.private = self.home / ".config/pd-memory"
        self.wrapper = self.home / ".local/bin/pd-memory-codex"
        self.env = {k: v for k, v in os.environ.items()
                    if not k.startswith("PD_MEMORY_") and k != "CODEX_HOME"}

    def run_installer(self, *args, ok=True, workspace=True):
        extra = ["--workspace", str(self.project)] if workspace else []
        result = subprocess.run(
            ["bash", str(ENTRY), "--home", str(self.home), "--binary", str(BINARY),
             "--non-interactive", *extra, *args], text=True, capture_output=True,
            env=self.env, timeout=120,
        )
        if ok:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout)
        return result

    def seed(self):
        (self.codex / "config.toml").write_text(CONFIG)
        (self.codex / "config.toml").chmod(0o640)
        (self.codex / "hooks.json").write_text(json.dumps(
            {"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "echo keep"}]}]}}))

    def hooks_list(self):
        return installer.codex_app_server(self.codex, [
            {"jsonrpc": "2.0", "id": 1, "method": "initialize",
             "params": {"clientInfo": {"name": "pd-memory-test", "version": "1"}}},
            {"jsonrpc": "2.0", "method": "initialized"},
            {"jsonrpc": "2.0", "id": 2, "method": "hooks/list", "params": {"cwds": [str(self.project)]}},
        ])[2]["result"]["data"][0]["hooks"]

    def test_preview_creates_nothing(self):
        self.seed()
        before = {p: p.read_bytes() for p in self.home.rglob("*") if p.is_file()}
        self.run_installer("--dry-run", workspace=False)
        self.assertEqual(before, {p: p.read_bytes() for p in self.home.rglob("*") if p.is_file()})

    def test_apply_requires_workspace_and_ignores_workbuddy_binding(self):
        self.private.mkdir(parents=True)
        (self.private / "config.json").write_text(json.dumps({
            "workspace_root": str(self.home), "model": {"model": "shared-model"}}))
        result = self.run_installer("--apply", "--mock", ok=False, workspace=False)
        self.assertIn("--workspace", result.stderr)
        self.assertFalse(self.wrapper.exists())
        self.run_installer("--apply", "--mock")
        config = common.read_object(self.private / "codex.json")
        self.assertEqual(config["workspace_root"], str(self.project))
        self.assertEqual(config["model"]["model"], "shared-model")
        self.assertEqual(common.read_object(self.private / "config.json")["workspace_root"], str(self.home))

    def test_mock_install_preserves_configs_notify_and_is_idempotent(self):
        self.seed()
        self.run_installer("--apply", "--mock")
        text = (self.codex / "config.toml").read_text()
        self.assertTrue(text.startswith(CONFIG))
        self.assertEqual(toml(text)["notify"], toml(CONFIG)["notify"])
        self.assertEqual((self.codex / "config.toml").stat().st_mode & 0o777, 0o640)
        hooks = common.read_object(self.codex / "hooks.json")["hooks"]
        self.assertEqual(hooks["Stop"][0]["hooks"][0]["command"], "echo keep")
        self.assertEqual(sorted(hooks), sorted(installer.EVENTS))
        manifests = len(list(self.state.glob("*/manifest.json")))
        result = self.run_installer("--apply", "--mock")
        self.assertIn("already installed", result.stdout)
        self.assertEqual(len(list(self.state.glob("*/manifest.json"))), manifests)
        self.assertFalse((self.home / ".local/share/pd-memory").exists())
        # Installed wrapper answers a Codex Stop payload with exactly continue.
        stop = subprocess.run([str(self.wrapper), "hook", "--codex"], input=json.dumps({
            "hook_event_name": "Stop", "session_id": "s", "transcript_path": None,
            "cwd": str(self.project), "turn_id": "t", "stop_hook_active": False,
            "last_assistant_message": None, "model": "m", "permission_mode": "default",
        }), capture_output=True, text=True, env=self.env, timeout=30)
        self.assertEqual(stop.stdout, '{"continue":true}\n')

    def test_rollback_restores_bytes_and_conflict_points_to_uninstall(self):
        self.seed()
        originals = {p: common.snapshot(p) for p in self.codex.iterdir()}
        self.run_installer("--apply", "--mock")
        self.run_installer("--rollback", workspace=False)
        for path, original in originals.items():
            self.assertEqual(common.snapshot(path), original)
        self.assertFalse(self.wrapper.exists())
        self.run_installer("--apply", "--mock")
        with (self.codex / "config.toml").open("a") as stream:  # Codex records hook trust here.
            stream.write('\n[hooks.state."x"]\ntrusted_hash = "sha256:1"\n')
        result = self.run_installer("--rollback", ok=False, workspace=False)
        self.assertIn("--uninstall", result.stderr)
        self.assertTrue(self.wrapper.exists())

    def test_uninstall_removes_only_our_entries(self):
        self.seed()
        self.run_installer("--apply", "--mock")
        with (self.codex / "config.toml").open("a") as stream:
            stream.write('\n[hooks.state."x"]\ntrusted_hash = "sha256:1"\n')
        self.run_installer("--uninstall", workspace=False)
        text = (self.codex / "config.toml").read_text()
        self.assertNotIn("pd-memory", text)
        self.assertTrue(text.startswith(CONFIG))
        self.assertEqual(toml(text)["hooks"]["state"]["x"]["trusted_hash"], "sha256:1")
        self.assertEqual(common.read_object(self.codex / "hooks.json"),
                         {"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "echo keep"}]}]}})
        self.assertTrue(self.wrapper.exists(), "uninstall keeps binary, wrapper and memory")

    def test_invalid_codex_files_are_refused_without_writes(self):
        for name, bad in [("config.toml", "model = [\n"), ("hooks.json", '{"hooks":[]}'),
                          ("config.toml", 'mcp_servers = { pd-memory = { command = "x" } }\n')]:
            path = self.codex / name
            path.write_text(bad)
            self.run_installer("--apply", "--mock", ok=False)
            self.assertEqual(path.read_text(), bad)
            self.assertFalse(self.wrapper.exists())
            path.unlink()

    def test_symlinked_config_is_not_replaced(self):
        target = Path(self.tmp.name) / "outside.toml"
        target.write_text(CONFIG)
        (self.codex / "config.toml").symlink_to(target)
        self.run_installer("--apply", "--mock", ok=False)
        self.assertEqual(target.read_text(), CONFIG)

    def test_failed_check_rolls_back(self):
        self.seed()
        before = {p: common.snapshot(p) for p in self.codex.iterdir()}
        fake = Path(self.tmp.name) / "fake-binary"
        fake.write_text('#!/bin/sh\ncase "$1" in --version) echo "pd-memory test";;\n'
                        'hook) echo "--codex";;\n*) exit 1;;\nesac\n')
        fake.chmod(0o755)
        self.run_installer("--apply", "--mock", "--binary", str(fake), ok=False)
        self.assertEqual({p: common.snapshot(p) for p in self.codex.iterdir()}, before)
        self.assertFalse(self.wrapper.exists())

    def test_model_check_lifecycle_with_mock_provider(self):
        self.run_installer("--apply", "--mock")
        real = common.run_json
        def allow_mock(command, payload=None, timeout=30, allow_mock=False):
            return real(command, payload, timeout, True)
        with mock.patch.object(common, "run_json", side_effect=allow_mock), \
                contextlib.redirect_stdout(io.StringIO()):
            installer.verify_model(self.wrapper)

    @unittest.skipUnless(CODEX, "codex CLI not installed")
    def test_real_codex_loads_hooks_and_keeps_trust_across_reinstall(self):
        self.seed()
        self.run_installer("--apply", "--mock")
        ours = [h for h in self.hooks_list() if "pd-memory-codex" in h["command"]]
        self.assertEqual({h["eventName"]: h["timeoutSec"] for h in ours},
                         {"sessionStart": 150, "userPromptSubmit": 300, "stop": 300, "sessionEnd": 3})
        self.assertTrue(all(h["trustStatus"] == "untrusted" for h in ours))
        # Trust them the way /hooks does, then reinstall: hashes and keys must not change.
        with (self.codex / "config.toml").open("a") as stream:
            for hook in ours:
                stream.write(f'\n[hooks.state.{json.dumps(hook["key"])}]\n'
                             f'trusted_hash = "{hook["currentHash"]}"\n')
        self.run_installer("--apply", "--mock")
        after = [h for h in self.hooks_list() if "pd-memory-codex" in h["command"]]
        self.assertEqual([h["key"] for h in after], [h["key"] for h in ours])
        self.assertTrue(all(h["trustStatus"] == "trusted" for h in after), after)


if __name__ == "__main__":
    unittest.main()
