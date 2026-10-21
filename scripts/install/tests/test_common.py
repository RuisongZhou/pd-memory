"""Host-independent installer tests: transactions, rollback, merges, entry point."""

import contextlib
import io
import shlex
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import common  # noqa: E402

ENTRY = common.ROOT / "scripts/install.sh"


class TransactionTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.state = self.root / "backups"
        self.state.mkdir()
        self.first = self.root / "first"
        self.second = self.root / "second"
        self.first.write_bytes(b"before")
        self.first.chmod(0o640)
        self.changes = [(self.first, b"after", 0o600), (self.second, b"new", 0o755)]

    def test_verifier_failure_and_interrupt_restore_originals(self):
        for error in (common.InstallError("failed"), KeyboardInterrupt()):
            with self.assertRaises(type(error)), contextlib.redirect_stderr(io.StringIO()):
                common.apply_changes(self.changes, self.state, mock.Mock(side_effect=error))
            self.assertEqual(common.snapshot(self.first), (b"before", 0o640))
            self.assertFalse(self.second.exists())

    def test_partial_write_failure_restores_previous_files(self):
        real_write = common.atomic_write
        def write(path, data, mode):
            if path == self.second:
                raise OSError("injected write failure")
            real_write(path, data, mode)
        with mock.patch.object(common, "atomic_write", side_effect=write):
            with self.assertRaises(OSError), contextlib.redirect_stderr(io.StringIO()):
                common.apply_changes(self.changes, self.state, lambda: None)
        self.assertEqual(common.snapshot(self.first), (b"before", 0o640))
        self.assertFalse(self.second.exists())

    def test_backup_pointer_failure_restores_previous_files(self):
        real_write = common.atomic_write
        def write(path, data, mode):
            if path == self.state / "latest":
                raise OSError("injected metadata failure")
            real_write(path, data, mode)
        with mock.patch.object(common, "atomic_write", side_effect=write):
            with self.assertRaises(OSError), contextlib.redirect_stderr(io.StringIO()):
                common.apply_changes(self.changes, self.state, lambda: None)
        self.assertEqual(common.snapshot(self.first), (b"before", 0o640))
        self.assertFalse(self.second.exists())

    def test_plan_conflict_detected_before_writing(self):
        expected = {p: common.snapshot(p) for p, _, _ in self.changes}
        self.first.write_bytes(b"edited elsewhere")
        with self.assertRaises(common.InstallError):
            common.apply_changes(self.changes, self.state, lambda: None, expected)
        self.assertEqual(self.first.read_bytes(), b"edited elsewhere")
        self.assertFalse(self.second.exists())
        self.assertEqual(list(self.state.iterdir()), [])

    def test_corrupt_backup_prevents_any_rollback(self):
        with contextlib.redirect_stdout(io.StringIO()):
            backup = common.apply_changes(self.changes, self.state, lambda: None)
        manifest = common.read_object(backup / "manifest.json")
        (backup / manifest["files"][0]["backup"]).write_bytes(b"corrupt")
        with self.assertRaises(common.InstallError):
            common.restore(backup, manifest)
        self.assertEqual(self.first.read_bytes(), b"after")
        self.assertEqual(self.second.read_bytes(), b"new")

    def test_concurrent_installer_rejected(self):
        with common.installation_lock(self.state):
            with self.assertRaises(common.InstallError):
                with common.installation_lock(self.state):
                    self.fail("lock unexpectedly acquired")


class MergeHelperTests(unittest.TestCase):
    WRAPPER = Path("/opt/pd/pd-memory-x")

    def owned(self, command, host_id="x"):
        return common.owned_hook({"type": "command", "command": command},
                                 {self.WRAPPER}, host_id)

    def test_owned_hook_matches_flag_alias_and_host_forms(self):
        wrapper = shlex.quote(str(self.WRAPPER))
        for command in [f"{wrapper} hook", f"{wrapper} hook --x", f"{wrapper} hook --host x",
                        f"env PD_MEMORY_PROVIDER=mock {wrapper} --config /c.json hook"]:
            self.assertTrue(self.owned(command), command)
        for command in [f"{wrapper} hook --y", f"{wrapper} hook --host y", "echo pd-memory-x hook",
                        f"{wrapper} serve mcp", "'unterminated", ""]:
            self.assertFalse(self.owned(command), command)
        self.assertFalse(common.owned_hook({"command": None}, {self.WRAPPER}, "x"))
        with self.assertRaises(common.InstallError):
            common.owned_hook("not an object", {self.WRAPPER}, "x")

    def test_merge_hooks_append_and_in_place_strategies(self):
        events = {"Stop": 5}
        ours = lambda event: {"type": "command", "command": "ours"}
        owned = lambda hook: hook.get("command") in ("ours", "old")
        other = {"hooks": [{"command": "keep"}]}
        current = {"hooks": {"Stop": [{"hooks": [{"command": "old"}]}, other]}}
        appended = common.merge_hooks(current, events, ours, owned, in_place=False)
        self.assertEqual(appended["hooks"]["Stop"], [other, {"hooks": [ours("Stop")]}])
        in_place = common.merge_hooks(current, events, ours, owned, in_place=True)
        self.assertEqual(in_place["hooks"]["Stop"], [{"hooks": [ours("Stop")]}, other])
        removed = common.merge_hooks(in_place, events, ours, owned, in_place=True, install=False)
        self.assertEqual(removed["hooks"]["Stop"], [other])

    def test_merge_mcp_servers_keeps_other_servers(self):
        merged = common.merge_mcp_servers({"mcpServers": {"a": {"x": 1}}, "k": 2}, {"command": "w"})
        self.assertEqual(merged, {"mcpServers": {"a": {"x": 1}, "pd-memory": {"command": "w"}}, "k": 2})
        with self.assertRaises(common.InstallError):
            common.merge_mcp_servers({"mcpServers": []}, {})


class EntryPointTests(unittest.TestCase):
    def run_entry(self, *args):
        return subprocess.run(["bash", str(ENTRY), *args], text=True, capture_output=True, timeout=60)

    def test_host_is_required_and_validated(self):
        missing = self.run_entry("--dry-run")
        self.assertEqual(missing.returncode, 2)
        self.assertIn("--host", missing.stderr)
        unknown = self.run_entry("--host", "nope", "--dry-run")
        self.assertEqual(unknown.returncode, 2)
        self.assertIn("invalid choice", unknown.stderr)

    def test_every_registered_host_loads_and_lists_its_options(self):
        for host_id in common.HOSTS:
            module = common.load_host(host_id)
            self.assertEqual(module.ID, host_id)
            if module.HOOK_ARGS is None:  # MCP-only host: no hooks to install
                self.assertFalse(hasattr(module, "EVENTS"))
            else:
                self.assertEqual(module.HOOK_ARGS[0], "hook")
                self.assertEqual(set(module.EVENTS), {"SessionStart", "UserPromptSubmit", "Stop", "SessionEnd"})
            result = self.run_entry("--host", host_id, "--help")
            self.assertEqual(result.returncode, 0, result.stderr)
            # Every host offers the same one-command install surface.
            for option in ("--apply", "--dry-run", "--rollback", "--verify-model",
                           "--non-interactive", "--workspace", "--binary"):
                self.assertIn(option, result.stdout, f"{host_id} lacks {option}")

    def test_host_specific_options_stay_with_their_host(self):
        with tempfile.TemporaryDirectory() as home:
            result = self.run_entry("--host", "workbuddy", "--home", home, "--uninstall")
            self.assertEqual(result.returncode, 2)
            result = self.run_entry("--host", "codex", "--home", home, "--mcp-target", "gui")
            self.assertEqual(result.returncode, 2)
            result = self.run_entry("--host", "deepseek-harness", "--home", home, "--codex-home", home)
            self.assertEqual(result.returncode, 2)
            result = self.run_entry("--host", "codex", "--home", home, "--dsh-home", home)
            self.assertEqual(result.returncode, 2)


if __name__ == "__main__":
    unittest.main()
