"""Regression checks that resolved parity differences can no longer be waived."""
import copy
import json
import unittest

import compare


class StrictParityTests(unittest.TestCase):
    def setUp(self):
        self.step = {"ws": "A", "id": "recall", "op": "retrieve", "expect_dev": "dev-recall-envelope"}
        self.left = {"workspaces": {"A": {"results": {"recall": {
            "intent": "project", "selectedFileIds": ["Project/a.md"],
            "systemContext": "## Project Memory\n\nA", "modelRequests": []
        }}}}}
        self.right = {"workspaces": {"A": {"results": {"recall": {
            "stdout": {"route": "project", "selected_ids": ["Project/a.md"],
                "hookSpecificOutput": {"additionalContext": "## Project Memory\n\nA"}},
            "modelRequests": []
        }}}}}

    def verdicts(self):
        return {aspect.name: aspect.verdict for aspect in
            compare.compare_retrievals(self.left, self.right, [self.step])}

    def test_identical_results_pass(self):
        self.assertEqual(set(self.verdicts().values()), {"PASS"})

    def test_changed_system_context_is_a_diff_even_with_old_waiver(self):
        self.right["workspaces"]["A"]["results"]["recall"]["stdout"]["hookSpecificOutput"]["additionalContext"] += " changed"
        self.assertEqual(self.verdicts()["retrieve/recall/system-context"], "DIFF")

    def test_selection_order_is_observable(self):
        self.left["workspaces"]["A"]["results"]["recall"]["selectedFileIds"] = ["Project/a.md", "Project/b.md"]
        self.right["workspaces"]["A"]["results"]["recall"]["stdout"]["selected_ids"] = ["Project/b.md", "Project/a.md"]
        self.assertEqual(self.verdicts()["retrieve/recall/selected"], "DIFF")

    def test_explicit_search_route_cannot_be_waived(self):
        self.step["op"] = "search"
        self.right["workspaces"]["A"]["results"]["recall"]["stdout"]["route"] = "mix"
        self.assertEqual(self.verdicts()["search/recall/route"], "DIFF")

    def test_prefilter_candidate_loss_is_a_diff(self):
        left = self.left["workspaces"]["A"]["results"]["recall"]
        right = self.right["workspaces"]["A"]["results"]["recall"]
        gate = {"purpose": "recall_gate", "user": "{}"}
        selection = {"purpose": "recall_select", "user": json.dumps({"manifest": [{"id": "Project/a.md"}, {"id": "Project/b.md"}]})}
        left["modelRequests"] = [gate, selection]
        right["modelRequests"] = copy.deepcopy(left["modelRequests"])
        right["modelRequests"][1]["user"] = json.dumps({"manifest": [{"id": "Project/a.md"}]})
        verdicts = {aspect.name: aspect.verdict for aspect in compare.compare_model_calls(self.left, self.right, [self.step])}
        self.assertEqual(verdicts["model/recall/manifest-1"], "DIFF")

    def test_missing_reverse_snapshot_is_a_diff(self):
        self.step.update(op="rollback", expect_snapshot=True)
        self.left["workspaces"]["A"]["results"]["recall"]["dreamSnapshot"] = {
            "sourceAction": "rollback", "before": {"runtimeState": {}}, "after": {"runtimeState": {}}}
        self.right["workspaces"]["A"]["results"]["recall"]["dreamSnapshot"] = None
        verdicts = {aspect.name: aspect.verdict for aspect in compare.compare_dream_snapshots(self.left, self.right, [self.step])}
        self.assertEqual(verdicts["snapshot/recall"], "DIFF")
        self.assertEqual(verdicts["snapshot/recall/expected"], "DIFF")

    def test_manifest_link_prefix_is_not_normalized_or_waived(self):
        left_file = {"path": "MEMORY.md", "content": "- [identity](../../../global/UserIdentityNotes/n.md)"}
        right_file = {"path": "MEMORY.md", "content": "- [identity](global/UserIdentityNotes/n.md)"}
        self.left["workspaces"]["A"]["files"] = [left_file]
        self.right["workspaces"]["A"]["files"] = [right_file]
        self.left["globalFiles"] = [left_file]
        self.right["globalFiles"] = [right_file]
        self.step.update(op="files", expect_dev="dev-user-link")
        self.left["workspaces"]["A"]["results"]["recall"]["files"] = [left_file]
        self.right["workspaces"]["A"]["results"]["recall"]["files"] = [right_file]
        comparisons = {
            "files": compare.compare_files(self.left, self.right),
            "manifest": compare.compare_manifest(self.left, self.right),
            "global": compare.compare_global_files(self.left, self.right),
            "step-files": compare.compare_step_files(self.left, self.right, [self.step]),
        }
        for name, aspects in comparisons.items():
            with self.subTest(comparator=name):
                self.assertIn("DIFF", [aspect.verdict for aspect in aspects])

    def test_duplicate_capture_flag_difference_cannot_be_waived(self):
        messages = [{"role": "user", "content": "remember this"}]
        self.step.update(op="capture", expect_dev="dev-capture-duplicate")
        self.left["workspaces"]["A"]["results"]["recall"] = {
            "captured": True, "sessionKey": "session", "normalizedMessages": messages}
        self.right["workspaces"]["A"]["results"]["recall"] = {"stdout": {
            "captured": False, "session_key": "session", "normalized_messages": messages}}
        aspects = compare.compare_captures(self.left, self.right, [self.step])
        self.assertEqual([aspect.verdict for aspect in aspects], ["DIFF"])




class RuntimeParityTests(unittest.TestCase):
    def fixtures(self):
        event = {"op": "recall", "cacheHit": True, "pending": 0}
        step = {"ws": "A", "id": "runtime", "op": "runtime", "actions": [{"op": "recall", "expect": {"cacheHit": True}}]}
        left = {"workspaces": {"A": {"results": {"runtime": {"events": [event], "modelRequests": []}}}}}
        right = {"workspaces": {"A": {"results": {"runtime": {"stdout": {"events": [copy.deepcopy(event)]}, "modelRequests": []}}}}}
        return step, left, right

    def test_cache_hit_mismatch_is_a_diff(self):
        step, left, right = self.fixtures()
        right["workspaces"]["A"]["results"]["runtime"]["stdout"]["events"][0]["cacheHit"] = False
        aspects = compare.compare_runtime(left, right, [step])
        self.assertIn("DIFF", [aspect.verdict for aspect in aspects])

    def test_shared_regression_fails_explicit_expectation(self):
        step, left, right = self.fixtures()
        left["workspaces"]["A"]["results"]["runtime"]["events"][0]["cacheHit"] = False
        right["workspaces"]["A"]["results"]["runtime"]["stdout"]["events"][0]["cacheHit"] = False
        aspects = compare.compare_runtime(left, right, [step])
        self.assertEqual(aspects[-1].verdict, "DIFF")

    def test_extra_repair_request_is_a_diff(self):
        step, left, right = self.fixtures()
        right["workspaces"]["A"]["results"]["runtime"]["modelRequests"] = [{"purpose": "repair"}]
        aspects = compare.compare_model_calls(left, right, [step])
        self.assertEqual(aspects[0].verdict, "DIFF")

    def test_json_mode_fallback_mismatch_is_a_diff(self):
        step, left, right = self.fixtures()
        left["workspaces"]["A"]["results"]["runtime"]["modelRequests"] = [{"purpose": "recall_gate", "status": 200, "jsonMode": False}]
        right["workspaces"]["A"]["results"]["runtime"]["modelRequests"] = [{"purpose": "recall_gate", "status": 200, "jsonMode": True}]
        aspects = compare.compare_model_calls(left, right, [step])
        self.assertEqual(aspects[1].verdict, "DIFF")


if __name__ == "__main__":
    unittest.main()
