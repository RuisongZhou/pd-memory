"""Offline DeepSeek Harness installation tests; run after cargo build -p pd-memory-cli.

Tests that need dsh itself (to compose cordis.patch.yml) skip when neither
`dsh` nor the Desktop app is installed. They only use a temporary DSH_HOME.
"""

import contextlib
import io
import os
import subprocess
import sys
import tempfile
import types
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import common  # noqa: E402
from hosts import deepseek_harness as installer  # noqa: E402

BINARY = Path(os.environ.get("PD_MEMORY_TEST_BINARY", common.ROOT / "target/debug/pd-memory"))
ENTRY = common.ROOT / "scripts/install.sh"
WRAPPER = Path("/home/me/.local/bin/pd-memory-dsh")
DSH = installer.find_dsh(types.SimpleNamespace(dsh_binary=None), types.SimpleNamespace(home=Path.home()))

PATCH = """\
# Your patch layer: settings written by the Desktop app
- id: locale
  name: "@deepseek-ai/dsh-client-locale"
  config:
    preference: zh
- id: agent-default-model
  name: "@deepseek-ai/dsh-agent-default-model"
  config:
    provider: deepseek-account
"""
HAND_WRITTEN = """
# pd-memory, added by hand from the docs
- insert:
    - id: memory-pd-memory
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        serverName: pd-memory
        transport: stdio
        command: /old/pd-memory-dsh
        args: [serve, mcp]
        toolCallTimeoutMs: 300000
"""
LATER = "- id: later\n  name: '@example/later'\n"


def block(wrapper):
    return "".join(line + "\n" for line in installer.block(wrapper))


class PatchMergeTests(unittest.TestCase):
    def test_appends_block_and_keeps_every_other_byte(self):
        merged = installer.merge_patch(PATCH, WRAPPER)
        self.assertEqual(merged, PATCH + block(WRAPPER))
        self.assertEqual(installer.merge_patch(merged, WRAPPER), merged)
        self.assertEqual(installer.merge_patch(merged, None), PATCH)
        self.assertEqual(installer.merge_patch(PATCH, None), PATCH)

    def test_replaces_marked_block_in_place(self):
        old = installer.merge_patch(PATCH, Path("/old/pd-memory-dsh")) + LATER
        merged = installer.merge_patch(old, WRAPPER)
        self.assertEqual(merged, PATCH + block(WRAPPER) + LATER)
        self.assertEqual(installer.merge_patch(merged, None), PATCH + LATER)

    def test_empty_files_stay_yaml_lists(self):
        # dsh refuses to start when the home patch is not a YAML list.
        self.assertEqual(installer.merge_patch("", WRAPPER), block(WRAPPER))
        self.assertEqual(installer.merge_patch("[]\n", WRAPPER), block(WRAPPER))
        self.assertEqual(installer.merge_patch(block(WRAPPER), None), "[]\n")
        merged = installer.merge_patch("# note\n", WRAPPER)
        self.assertEqual(merged, "# note\n" + block(WRAPPER))
        self.assertEqual(installer.merge_patch(merged, None), "# note\n[]\n")

    def test_adopts_the_hand_written_row(self):
        merged = installer.merge_patch(PATCH + HAND_WRITTEN, WRAPPER)
        self.assertNotIn("/old/", merged)
        self.assertEqual(merged.count("memory-pd-memory"), 1)
        kept = PATCH + "\n# pd-memory, added by hand from the docs\n"
        self.assertEqual(merged, kept + block(WRAPPER))
        self.assertEqual(installer.merge_patch(merged, None), kept)
        self.assertEqual(installer.merge_patch(PATCH + HAND_WRITTEN + LATER, None), kept + LATER)

    def test_refuses_rows_it_cannot_own(self):
        shared_item = ("- insert:\n    - id: other\n      name: x\n"
                       "    - id: memory-pd-memory\n      name: y\n")
        group_target = "- id: group\n  insert:\n    - id: memory-pd-memory\n"
        other_server = ("- insert:\n    - id: mine\n      name: '@deepseek-ai/dsh-mcp-client'\n"
                        "      config:\n        serverName: pd-memory\n")
        twice = HAND_WRITTEN + HAND_WRITTEN
        damaged = installer.BEGIN + "\n- insert: []\n"
        for text in (shared_item, group_target, other_server, twice, damaged):
            with self.subTest(text=text):
                with self.assertRaises(common.InstallError):
                    installer.merge_patch(PATCH + text, WRAPPER)
        with self.assertRaises(common.InstallError):
            installer.merge_patch(installer.merge_patch(PATCH, WRAPPER) + HAND_WRITTEN, WRAPPER)

    def test_refuses_layouts_it_cannot_append_to(self):
        for text in ("key: value\n", "[{id: a}]\n", "  - id: a\n", "---\n- id: a\n"):
            with self.subTest(text=text):
                with self.assertRaises(common.InstallError):
                    installer.merge_patch(text, WRAPPER)

    def test_paths_needing_escapes_round_trip(self):
        import json
        wrapper = Path('/home/a "b" \\c/\'d\' $e/中文/pd-memory-dsh')
        line = next(l for l in installer.merge_patch(PATCH, wrapper).splitlines()
                    if l.strip().startswith("command:"))
        self.assertEqual(json.loads(line.split("command:", 1)[1]), str(wrapper))

    def test_crlf_and_missing_final_newline(self):
        crlf = PATCH.replace("\n", "\r\n")
        merged = installer.merge_patch(crlf, WRAPPER)
        self.assertTrue(merged.startswith(crlf))
        self.assertNotIn("\n", merged.replace("\r\n", ""))
        self.assertEqual(installer.merge_patch(merged, None), crlf)
        merged = installer.merge_patch(PATCH.rstrip("\n"), WRAPPER)
        self.assertEqual(merged, PATCH + block(WRAPPER))


class DumpReadTests(unittest.TestCase):
    """`--dump-config` re-serializes values with js-yaml; read them back in every style."""

    ROW = "- id: memory-pd-memory\n  name: '@deepseek-ai/dsh-mcp-client'\n  config:\n    serverName: pd-memory\n"
    NEXT = "- id: after\n  config:\n    command: /not/ours\n"

    def test_plain_quoted_and_folded_values(self):
        long_path = "/var/folders/xy/T/pd-memory-dsh-test/home with 'quotes' $dollar/.local/bin/pd-memory-dsh"
        cases = {
            "    command: /home/me/.local/bin/pd-memory-dsh\n": "/home/me/.local/bin/pd-memory-dsh",
            "    command: '/home/it''s/pd-memory-dsh'\n": "/home/it's/pd-memory-dsh",
            '    command: "/home/a \\"b\\"/pd-memory-dsh"\n': '/home/a "b"/pd-memory-dsh',
            "    command: >-\n      /var/folders/xy/T/pd-memory-dsh-test/home\n"
            "      with 'quotes' $dollar/.local/bin/pd-memory-dsh\n": long_path,
        }
        for command, expected in cases.items():
            with self.subTest(command=command):
                dumped = "# header\n" + self.ROW + command + "    args:\n      - serve\n" + self.NEXT
                self.assertEqual(installer.dumped_command(dumped), expected)
                self.assertTrue(installer.same_command(dumped, Path(expected)))
                self.assertFalse(installer.same_command(dumped, Path("/other/pd-memory-dsh")))

    def test_row_without_command_does_not_borrow_the_next_rows(self):
        self.assertIsNone(installer.dumped_command(self.ROW + self.NEXT))
        self.assertIsNone(installer.dumped_command(self.NEXT))


class InstallationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if not BINARY.is_file():
            raise RuntimeError("Run cargo build -p pd-memory-cli before installer tests")

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="pd-memory-dsh-test-")
        self.addCleanup(self.tmp.cleanup)
        self.home = Path(self.tmp.name) / "home with 'quotes' $dollar"
        self.dsh = self.home / ".dsh"
        self.dsh.mkdir(parents=True)
        self.patch = self.dsh / "cordis.patch.yml"
        self.project = self.home / "project"
        self.project.mkdir()
        self.state = self.home / ".local/state/pd-memory/installations/deepseek-harness"
        self.private = self.home / ".config/pd-memory"
        self.wrapper = self.home / ".local/bin/pd-memory-dsh"
        self.env = {k: v for k, v in os.environ.items()
                    if not k.startswith("PD_MEMORY_") and not k.startswith("DSH_")}

    def run_installer(self, *args, ok=True, workspace=True, check=False):
        extra = ["--workspace", str(self.project)] if workspace else []
        extra += [] if check else ["--skip-dsh-check"]
        result = subprocess.run(
            ["bash", str(ENTRY), "--host", "deepseek-harness", "--home", str(self.home),
             "--binary", str(BINARY), "--non-interactive", *extra, *args],
            text=True, capture_output=True, env=self.env, timeout=300,
        )
        if ok:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout)
        return result

    def seed(self, text=PATCH, mode=0o640):
        self.patch.write_text(text)
        self.patch.chmod(mode)

    def test_preview_creates_nothing(self):
        self.seed()
        before = {p: p.read_bytes() for p in self.home.rglob("*") if p.is_file()}
        self.run_installer("--dry-run", workspace=False)
        self.assertEqual(before, {p: p.read_bytes() for p in self.home.rglob("*") if p.is_file()})

    def test_apply_requires_workspace_and_reuses_only_the_model(self):
        self.private.mkdir(parents=True)
        (self.private / "config.json").write_text(
            '{"workspace_root": "/elsewhere", "model": {"model": "shared-model"}}')
        result = self.run_installer("--apply", "--mock", ok=False, workspace=False)
        self.assertIn("--workspace", result.stderr)
        self.assertFalse(self.wrapper.exists())
        self.run_installer("--apply", "--mock")
        config = common.read_object(self.private / "dsh.json")
        self.assertEqual(config["workspace_root"], str(self.project))
        self.assertEqual(config["model"]["model"], "shared-model")
        self.assertEqual(common.read_object(self.private / "config.json")["workspace_root"], "/elsewhere")

    def test_reuses_a_codex_only_model_config(self):
        self.private.mkdir(parents=True)
        (self.private / "codex.json").write_text('{"model": {"model": "codex-model"}}')
        self.run_installer("--apply", "--mock")
        self.assertEqual(common.read_object(self.private / "dsh.json")["model"]["model"], "codex-model")

    def test_mock_install_preserves_the_patch_and_is_idempotent(self):
        self.seed()
        result = self.run_installer("--apply", "--mock")
        self.assertIn("server/discover", result.stdout)
        self.assertEqual(self.patch.read_text(), PATCH + block(self.wrapper))
        self.assertEqual(self.patch.stat().st_mode & 0o777, 0o640)
        manifests = len(list(self.state.glob("*/manifest.json")))
        result = self.run_installer("--apply", "--mock")
        self.assertIn("already installed", result.stdout)
        self.assertEqual(len(list(self.state.glob("*/manifest.json"))), manifests)
        self.assertFalse((self.home / ".local/share/pd-memory").exists())

    def test_install_adopts_the_hand_written_row(self):
        self.seed(PATCH + HAND_WRITTEN)
        self.run_installer("--apply", "--mock")
        text = self.patch.read_text()
        self.assertNotIn("/old/", text)
        self.assertEqual(text.count("memory-pd-memory"), 1)

    def test_rollback_restores_bytes_and_conflict_points_to_uninstall(self):
        self.seed()
        original = common.snapshot(self.patch)
        self.run_installer("--apply", "--mock")
        self.run_installer("--rollback", workspace=False)
        self.assertEqual(common.snapshot(self.patch), original)
        self.assertFalse(self.wrapper.exists())
        self.run_installer("--apply", "--mock")
        with self.patch.open("a") as stream:
            stream.write(LATER)
        result = self.run_installer("--rollback", ok=False, workspace=False)
        self.assertIn("--uninstall", result.stderr)

    def test_uninstall_removes_only_our_row(self):
        self.seed()
        self.run_installer("--apply", "--mock")
        with self.patch.open("a") as stream:
            stream.write(LATER)
        self.run_installer("--uninstall", workspace=False)
        self.assertEqual(self.patch.read_text(), PATCH + LATER)
        self.assertTrue(self.wrapper.exists(), "uninstall keeps binary, wrapper and memory")
        result = self.run_installer("--uninstall", workspace=False)
        self.assertIn("nothing to remove", result.stdout)

    def test_uninstall_of_a_created_file_leaves_an_empty_list(self):
        self.run_installer("--apply", "--mock")
        self.run_installer("--uninstall", workspace=False)
        self.assertEqual(self.patch.read_text(), "[]\n")

    def test_unsupported_patches_are_refused_without_writes(self):
        for bad in ("key: value\n", "[{id: a}]\n", "  - id: a\n", PATCH + HAND_WRITTEN.replace(
                "- insert:\n", "- insert:\n    - id: other\n      name: x\n")):
            with self.subTest(bad=bad):
                self.seed(bad)
                self.run_installer("--apply", "--mock", ok=False)
                self.assertEqual(self.patch.read_text(), bad)
                self.assertFalse(self.wrapper.exists())

    def test_symlinked_patch_is_not_replaced(self):
        target = Path(self.tmp.name) / "outside.yml"
        target.write_text(PATCH)
        self.patch.symlink_to(target)
        self.run_installer("--apply", "--mock", ok=False)
        self.assertEqual(target.read_text(), PATCH)

    def test_failed_check_rolls_back(self):
        self.seed()
        fake = Path(self.tmp.name) / "fake-binary"
        fake.write_text('#!/bin/sh\ncase "$1" in --version) echo "pd-memory test";;\n*) exit 1;;\nesac\n')
        fake.chmod(0o755)
        self.run_installer("--apply", "--mock", "--binary", str(fake), ok=False)
        self.assertEqual(self.patch.read_text(), PATCH)
        self.assertFalse(self.wrapper.exists())

    def test_model_check_over_mcp_with_mock_provider(self):
        self.run_installer("--apply", "--mock")
        with contextlib.redirect_stdout(io.StringIO()):
            installer.verify_model(self.wrapper, mock=True)

    @unittest.skipUnless(DSH, "neither dsh nor the DeepSeek Harness Desktop app is installed")
    def test_real_dsh_composes_install_and_uninstall(self):
        self.seed()
        result = self.run_installer("--apply", "--mock", "--dsh-binary", str(DSH), check=True)
        self.assertIn("[check] dsh composed cordis.patch.yml", result.stdout)
        self.run_installer("--uninstall", "--dsh-binary", str(DSH), workspace=False, check=True)
        self.assertEqual(self.patch.read_text(), PATCH)
        # A comments-only file stops dsh from starting; installing must leave a list it loads.
        self.seed("# only a comment\n")
        result = self.run_installer("--apply", "--mock", "--dsh-binary", str(DSH), check=True)
        self.assertIn("[check] dsh composed cordis.patch.yml", result.stdout)


if __name__ == "__main__":
    unittest.main()
