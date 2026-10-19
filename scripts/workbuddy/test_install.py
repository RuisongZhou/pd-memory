"""Offline installation tests; run after cargo build -p pd-memory-cli."""

import contextlib
import importlib.util
import io
import json
import os
import pty
import select
import shlex
import subprocess
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest import mock

SPEC = importlib.util.spec_from_file_location("installer", Path(__file__).with_name("install.py"))
installer = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(installer)
BINARY = Path(os.environ.get("PD_MEMORY_TEST_BINARY", installer.ROOT / "target/debug/pd-memory"))
ENTRY = installer.ROOT / "scripts/workbuddy/install.sh"


class MergeTests(unittest.TestCase):
    def test_mcp_preserves_other_servers_and_only_enables_ours(self):
        original = {"mcpServers": {"other": {"env": {"SECRET": "keep"}}},
                    "disabledMcpServers": ["other", "pd-memory"], "custom": 3}
        merged = installer.merge_mcp(original, Path("/bin/pd-memory-workbuddy"), cli=True)
        self.assertEqual(merged["mcpServers"]["other"], original["mcpServers"]["other"])
        self.assertEqual(merged["disabledMcpServers"], ["other"])
        self.assertEqual(merged["custom"], 3)
        self.assertNotIn("pd-memory", original["mcpServers"])

    def test_hooks_preserve_mixed_groups_and_adopt_manual_install(self):
        wrapper = Path("/path with spaces/pd-memory-workbuddy")
        own = {"type": "command", "command": shlex.quote(str(wrapper)) + " hook"}
        workbuddy = {"type": "command", "command": shlex.quote(str(wrapper)) + " hook --workbuddy"}
        other = {"type": "command", "command": "echo pd-memory-workbuddy hook"}
        original = {"custom": True, "hooks": {
            "Stop": [{"matcher": "keep", "hooks": [own, workbuddy, other]}],
            "PreCompact": [{"hooks": [other]}],
        }}
        merged = installer.merge_hooks(original, wrapper, Path("/bin/pd-memory"))
        self.assertEqual(merged["hooks"]["Stop"][0], {"matcher": "keep", "hooks": [other]})
        self.assertEqual(merged["hooks"]["PreCompact"], original["hooks"]["PreCompact"])
        self.assertEqual(merged, installer.merge_hooks(merged, wrapper, Path("/bin/pd-memory")))

    def test_bad_json_shapes_are_rejected(self):
        for value in [{"mcpServers": []}, {"disabledMcpServers": {}}]:
            with self.assertRaises(installer.InstallError):
                installer.merge_mcp(value, Path("/wrapper"), cli=True)
        for value in [{"hooks": []}, {"hooks": {"Stop": {}}},
                      {"hooks": {"Stop": [{"hooks": "bad"}]}}]:
            with self.assertRaises(installer.InstallError):
                installer.merge_hooks(value, Path("/wrapper"), Path("/binary"))

    def test_upgrade_removes_old_mock_hooks_including_optional_events(self):
        command = (
            "env PD_MEMORY_PROVIDER=mock PD_MEMORY_DATA_ROOT=/old/data "
            + shlex.quote(str(installer.ROOT / "target/debug/pd-memory"))
            + " --config /old/config.json hook"
        )
        old_hook = {"type": "command", "command": command}
        current = {"hooks": {event: [{"hooks": [old_hook]}]
                             for event in ["Stop", "SessionEnd", "PreCompact"]}}
        merged = installer.merge_hooks(current, Path("/wrapper"), Path("/binary"))
        self.assertEqual(len(merged["hooks"]["Stop"]), 1)
        self.assertEqual(len(merged["hooks"]["SessionEnd"]), 1)
        self.assertEqual(merged["hooks"]["PreCompact"], [])
        self.assertNotIn("PD_MEMORY_PROVIDER=mock", json.dumps(merged))
        for event, timeout in installer.EVENTS.items():
            hook = merged["hooks"][event][-1]["hooks"][0]
            self.assertTrue(hook["command"].endswith(" hook --workbuddy"))
            self.assertEqual(hook["timeout"], timeout)


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
        for error in (installer.InstallError("failed"), KeyboardInterrupt()):
            with self.assertRaises(type(error)), contextlib.redirect_stderr(io.StringIO()):
                installer.apply_changes(self.changes, self.state, mock.Mock(side_effect=error))
            self.assertEqual(installer.snapshot(self.first), (b"before", 0o640))
            self.assertFalse(self.second.exists())

    def test_partial_write_failure_restores_previous_files(self):
        real_write = installer.atomic_write
        def write(path, data, mode):
            if path == self.second:
                raise OSError("injected write failure")
            real_write(path, data, mode)
        with mock.patch.object(installer, "atomic_write", side_effect=write):
            with self.assertRaises(OSError), contextlib.redirect_stderr(io.StringIO()):
                installer.apply_changes(self.changes, self.state, lambda: None)
        self.assertEqual(installer.snapshot(self.first), (b"before", 0o640))
        self.assertFalse(self.second.exists())

    def test_backup_pointer_failure_restores_previous_files(self):
        real_write = installer.atomic_write
        def write(path, data, mode):
            if path == self.state / "latest":
                raise OSError("injected metadata failure")
            real_write(path, data, mode)
        with mock.patch.object(installer, "atomic_write", side_effect=write):
            with self.assertRaises(OSError), contextlib.redirect_stderr(io.StringIO()):
                installer.apply_changes(self.changes, self.state, lambda: None)
        self.assertEqual(installer.snapshot(self.first), (b"before", 0o640))
        self.assertFalse(self.second.exists())

    def test_plan_conflict_detected_before_writing(self):
        expected = {p: installer.snapshot(p) for p, _, _ in self.changes}
        self.first.write_bytes(b"edited elsewhere")
        with self.assertRaises(installer.InstallError):
            installer.apply_changes(self.changes, self.state, lambda: None, expected)
        self.assertEqual(self.first.read_bytes(), b"edited elsewhere")
        self.assertFalse(self.second.exists())
        self.assertEqual(list(self.state.iterdir()), [])

    def test_corrupt_backup_prevents_any_rollback(self):
        with contextlib.redirect_stdout(io.StringIO()):
            backup = installer.apply_changes(self.changes, self.state, lambda: None)
        manifest = installer.read_object(backup / "manifest.json")
        (backup / manifest["files"][0]["backup"]).write_bytes(b"corrupt")
        with self.assertRaises(installer.InstallError):
            installer.restore(backup, manifest)
        self.assertEqual(self.first.read_bytes(), b"after")
        self.assertEqual(self.second.read_bytes(), b"new")

    def test_concurrent_installer_rejected(self):
        with installer.installation_lock(self.state):
            with self.assertRaises(installer.InstallError):
                with installer.installation_lock(self.state):
                    self.fail("lock unexpectedly acquired")


class InstallationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if not BINARY.is_file():
            raise RuntimeError("Run cargo build -p pd-memory-cli before installer tests")

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="pd-memory-install-test-")
        self.addCleanup(self.tmp.cleanup)
        # Spaces, quotes and shell expansion characters exercise wrapper quoting.
        self.home = Path(self.tmp.name) / "home with 'quotes' $dollar"
        self.wb = self.home / ".workbuddy"
        (self.wb / "workspace").mkdir(parents=True)
        self.state = self.home / ".local/state/pd-memory/installations"
        self.private = self.home / ".config/pd-memory"
        self.wrapper = self.home / ".local/bin/pd-memory-workbuddy"
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("PD_MEMORY_")}

    def run_installer(self, *args, ok=True, env=None):
        result = subprocess.run(
            ["bash", str(ENTRY), "--home", str(self.home), "--binary", str(BINARY),
             "--non-interactive", *args], text=True, capture_output=True,
            env=env or self.env, timeout=60,
        )
        if ok:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0)
        return result

    def seed_configs(self):
        (self.wb / "settings.json").write_text('{"custom":true,"hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo keep"}]}]}}')
        (self.wb / "mcp.json").write_text('{"mcpServers":{"other":{"command":"keep"}},"custom":true}')
        (self.wb / ".mcp.json").write_text('{"mcpServers":{"cli-other":{"command":"keep"}},"disabledMcpServers":["cli-other","pd-memory"]}')

    def test_preview_creates_nothing_and_needs_no_credentials(self):
        before = sorted(self.home.rglob("*"))
        self.run_installer("--dry-run")
        self.assertEqual(before, sorted(self.home.rglob("*")))

    def test_missing_key_fails_before_any_write(self):
        before = sorted(self.home.rglob("*"))
        result = self.run_installer("--apply", "--model", "qwen-plus",
                                    "--base-url", "https://api.example.invalid/v1", ok=False)
        self.assertIn("API key required", result.stderr)
        self.assertEqual(before, sorted(self.home.rglob("*")))

    def test_interactive_install_hides_key_input(self):
        master, slave = pty.openpty()
        process = subprocess.Popen(
            ["bash", str(ENTRY), "--home", str(self.home), "--binary", str(BINARY), "--apply"],
            stdin=slave, stdout=slave, stderr=slave, env=self.env,
        )
        os.close(slave)
        prompts = [
            (b"Model [qwen-plus]: ", b"\n"),
            (b"API base URL: ", b"https://api.example.invalid/v1\n"),
            (b"Memory model API key (hidden): ", b"test-hidden-prompt-key\n"),
        ]
        output = b""
        pending = b""
        try:
            deadline = time.monotonic() + 30
            while process.poll() is None and time.monotonic() < deadline:
                if not select.select([master], [], [], 0.1)[0]:
                    continue
                try:
                    block = os.read(master, 65536)
                except OSError:
                    break
                output += block
                pending += block
                if prompts and prompts[0][0] in pending:
                    os.write(master, prompts.pop(0)[1])
                    pending = b""
            self.assertEqual(process.wait(timeout=5), 0, output.decode(errors="replace"))
            self.assertEqual(prompts, [])
            self.assertNotIn(b"test-hidden-prompt-key", output)
            self.assertEqual(installer.credential_from_file(self.private / "model.env"),
                             "test-hidden-prompt-key")
        finally:
            os.close(master)
            if process.poll() is None:
                process.kill()
                process.wait()

    def test_mock_install_preserves_configs_and_is_idempotent(self):
        self.seed_configs()
        self.run_installer("--apply", "--mock")
        manifest_count = len(list(self.state.glob("*/manifest.json")))
        self.run_installer("--apply", "--mock")
        self.assertEqual(len(list(self.state.glob("*/manifest.json"))), manifest_count)
        settings = installer.read_object(self.wb / "settings.json")
        self.assertTrue(settings["custom"])
        self.assertEqual(settings["hooks"]["Stop"][0]["hooks"][0]["command"], "echo keep")
        self.assertEqual(len(settings["hooks"]["Stop"]), 2)
        gui = installer.read_object(self.wb / "mcp.json")
        self.assertEqual(gui["mcpServers"]["other"]["command"], "keep")
        self.assertEqual(gui["mcpServers"]["pd-memory"]["command"], str(self.wrapper))
        cli = installer.read_object(self.wb / ".mcp.json")
        self.assertEqual(cli["disabledMcpServers"], ["cli-other"])
        self.assertFalse((self.wb / "connectors").exists())
        self.assertFalse((self.home / ".local/share/pd-memory").exists())

    def test_real_install_does_not_leak_key_or_inherit_ambient_overrides(self):
        secret = "test-key-'-$()-do-not-print"
        env = dict(self.env, TEST_KEY=secret)
        result = self.run_installer("--apply", "--model", "qwen-plus",
            "--base-url", "https://api.example.invalid/v1", "--api-key-env", "TEST_KEY", env=env)
        self.assertNotIn(secret, result.stdout + result.stderr)
        self.assertNotIn(secret, self.wrapper.read_text())
        self.assertNotIn(secret, (self.private / "config.json").read_text())
        self.assertEqual(installer.credential_from_file(self.private / "model.env"), secret)
        self.assertEqual((self.private / "model.env").stat().st_mode & 0o777, 0o600)
        self.assertEqual(self.private.stat().st_mode & 0o777, 0o700)
        result = subprocess.run([str(self.wrapper), "doctor"], capture_output=True, text=True,
            env=dict(env, PD_MEMORY_PROVIDER="mock", PD_MEMORY_MODEL="wrong",
                     PD_MEMORY_WORKSPACE_ID="wrong", PD_MEMORY_DATA_ROOT="/invalid"))
        self.assertEqual(result.returncode, 0)
        self.assertIn("provider: ok (openai-chat)", result.stdout)
        self.assertIn("model: qwen-plus", result.stdout)
        self.assertNotIn("workspace id: wrong", result.stdout)
        self.run_installer("--apply")  # Reuse stored credentials and model.
        self.assertEqual(installer.credential_from_file(self.private / "model.env"), secret)

    def test_key_file_and_anthropic_configuration(self):
        key_file = Path(self.tmp.name) / "key"
        key_file.write_text("test-file-key\n")
        self.run_installer("--apply", "--api-type", "anthropic", "--model", "test-model",
                           "--base-url", "https://api.example.invalid/v1", "--key-file", str(key_file))
        config = installer.read_object(self.private / "config.json")
        self.assertEqual(config["model"]["api_type"], "anthropic")
        self.assertEqual(installer.credential_from_file(self.private / "model.env"), "test-file-key")

    def test_existing_relative_paths_keep_cli_resolution(self):
        (self.private / "project").mkdir(parents=True)
        (self.private / "config.json").write_text(json.dumps({
            "data_root": "memory-data", "workspace_root": "project",
        }))
        self.run_installer("--apply", "--mock")
        config = installer.read_object(self.private / "config.json")
        self.assertEqual(config["data_root"], str(self.private / "memory-data"))
        self.assertEqual(config["workspace_root"], str(self.private / "project"))

    def test_rollback_restores_bytes_modes_and_keeps_memory(self):
        self.seed_configs()
        originals = {p: installer.snapshot(p) for p in self.wb.glob("*.json")}
        self.run_installer("--apply", "--mock")
        data = self.home / ".local/share/pd-memory"
        data.mkdir(parents=True)
        (data / "keep.md").write_text("user memory")
        self.run_installer("--rollback")
        for path, original in originals.items():
            self.assertEqual(installer.snapshot(path), original)
        self.assertFalse(self.wrapper.exists())
        self.assertFalse((self.private / "config.json").exists())
        self.assertEqual((data / "keep.md").read_text(), "user memory")

    def test_rollback_conflict_restores_nothing(self):
        self.run_installer("--apply", "--mock")
        (self.wb / "settings.json").write_text('{"edited":true}')
        result = self.run_installer("--rollback", ok=False)
        self.assertIn("Changed since installation", result.stderr)
        self.assertTrue(self.wrapper.exists())
        self.assertTrue((self.private / "config.json").exists())
        self.assertEqual((self.wb / "settings.json").read_text(), '{"edited":true}')

    def test_rollback_permission_change_is_a_conflict(self):
        self.run_installer("--apply", "--mock")
        (self.wb / "settings.json").chmod(0o640)
        self.run_installer("--rollback", ok=False)
        self.assertTrue(self.wrapper.exists())
        self.assertEqual((self.wb / "settings.json").stat().st_mode & 0o777, 0o640)

    def test_rollback_without_installation_has_clear_error(self):
        result = self.run_installer("--rollback", ok=False)
        self.assertIn("No installation backup found", result.stderr)
        self.assertFalse(self.state.exists())

    def test_invalid_config_refused_without_overwrite(self):
        for bad in ('{"hooks":null}', '{"hooks":{"Stop":"bad"}}', '[1,2]', '{'):
            path = self.wb / "settings.json"
            path.write_text(bad)
            self.run_installer("--apply", "--mock", ok=False)
            self.assertEqual(path.read_text(), bad)
            self.assertFalse(self.wrapper.exists())

    def test_post_install_check_failure_automatically_rolls_back(self):
        self.seed_configs()
        before = (self.wb / "settings.json").read_bytes()
        fake = Path(self.tmp.name) / "fake-binary"
        fake.write_text('#!/bin/sh\nif [ "$1" = "--version" ]; then echo "pd-memory test"; else exit 1; fi\n')
        fake.chmod(0o755)
        self.run_installer("--apply", "--mock", "--binary", str(fake), ok=False)
        self.assertEqual((self.wb / "settings.json").read_bytes(), before)
        self.assertFalse(self.wrapper.exists())
        self.assertFalse((self.private / "config.json").exists())
        manifests = list(self.state.glob("*/manifest.json"))
        self.assertEqual(len(manifests), 1)
        self.assertEqual(installer.read_object(manifests[0])["status"], "rolled-back")

    def test_failed_cloud_check_does_not_leak_http_body_or_leave_install(self):
        secret = "test-secret-that-must-not-leak"
        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                self.rfile.read(int(self.headers.get("Content-Length", "0")))
                self.send_response(401)
                self.end_headers()
                self.wfile.write(secret.encode())
            def log_message(self, *args):
                pass
        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        result = self.run_installer("--apply", "--verify-model", "--model", "qwen-plus",
            "--base-url", f"http://127.0.0.1:{server.server_port}/v1",
            "--api-key-env", "TEST_KEY", env=dict(self.env, TEST_KEY=secret), ok=False)
        self.assertNotIn(secret, result.stdout + result.stderr)
        self.assertFalse(self.wrapper.exists())
        self.assertFalse((self.private / "model.env").exists())

    def test_gui_only_does_not_write_cli_config(self):
        self.run_installer("--apply", "--mock", "--mcp-target", "gui")
        self.assertTrue((self.wb / "mcp.json").exists())
        self.assertFalse((self.wb / ".mcp.json").exists())

    def test_config_symlink_is_not_overwritten(self):
        target = Path(self.tmp.name) / "outside.json"
        target.write_text('{"custom":true}')
        (self.wb / "settings.json").symlink_to(target)
        self.run_installer("--apply", "--mock", ok=False)
        self.assertEqual(target.read_text(), '{"custom":true}')
        self.assertTrue((self.wb / "settings.json").is_symlink())
        self.assertFalse(self.wrapper.exists())


if __name__ == "__main__":
    unittest.main()
