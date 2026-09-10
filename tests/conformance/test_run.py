import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("conformance_run", Path(__file__).with_name("run.py"))
run = importlib.util.module_from_spec(spec)
spec.loader.exec_module(run)


class RunnerTests(unittest.TestCase):
    def setUp(self):
        self.catalog = {"tests": [
            {"name": "HTTPRouteWeight", "features": ["Gateway", "HTTPRoute"]},
            {"name": "UDPRoute", "features": ["Gateway", "UDPRoute"]},
            {"name": "UnselectedExtension", "features": ["RequestMirror"]},
        ]}

    def test_unknown_duplicate_and_empty_shortnames_are_rejected(self):
        for raw in ["httprouteweight", "HTTPRouteWeight,", "UDPRoute,UDPRoute", "DoesNotExist"]:
            with self.subTest(raw=raw), self.assertRaises(ValueError):
                run.requested_names(raw, self.catalog)
        self.assertEqual(run.requested_names("UDPRoute", self.catalog), ["UDPRoute"])

    def test_full_selection_excludes_only_unsupported_features(self):
        self.assertEqual(run.expected_names([], self.catalog), {"HTTPRouteWeight", "UDPRoute"})
        self.assertEqual(run.expected_names(["UnselectedExtension"], self.catalog), {"UnselectedExtension"})

    def test_final_go_verdicts_win_over_started_and_nested_assertions(self):
        log = """=== RUN   TestGatewayConformance/UDPRoute
    --- PASS: TestGatewayConformance/UDPRoute/case (0.01s)
    --- FAIL: TestGatewayConformance/UDPRoute (1.00s)
--- FAIL: TestGatewayConformance (1.00s)
FAIL
"""
        code, result = run.check_results(1, log, {"UDPRoute"}, True)
        self.assertEqual(code, 1)
        self.assertEqual(result["failed"], ["UDPRoute"])
        self.assertEqual(result["verdicts"], {"UDPRoute": "FAIL"})

    def test_skipped_missing_report_and_missing_terminal_are_not_successes(self):
        passed = "    --- PASS: TestGatewayConformance/UDPRoute (1.00s)\n--- PASS: TestGatewayConformance (1.00s)\nPASS\n"
        self.assertEqual(run.check_results(0, passed, {"UDPRoute"}, True)[0], 0)
        for log, report in [(passed.replace("/UDPRoute", "/OtherTest"), True),
                            (passed.replace("PASS: TestGatewayConformance/", "SKIP: TestGatewayConformance/"), True),
                            (passed, False), (passed.splitlines()[0], True)]:
            with self.subTest(log=log, report=report):
                self.assertNotEqual(run.check_results(0, log, {"UDPRoute"}, report)[0], 0)

    def test_namespace_ownership_requires_the_exact_run_annotation(self):
        self.assertFalse(run.owned({"metadata": {"name": "gateway-conformance-infra"}}, "current"))
        self.assertFalse(run.owned({"metadata": {"annotations": {run.OWNER: "previous"}}}, "current"))
        self.assertTrue(run.owned({"metadata": {"annotations": {run.OWNER: "current"}}}, "current"))

    def test_runner_uses_separate_namespace_and_unique_rbac(self):
        first = run.resources("a" * 32, "registry/runner@sha256:abc")
        second = run.resources("b" * 32, "registry/runner@sha256:def")
        self.assertTrue(all(run.owned(r, "a" * 32) for r in first))
        self.assertNotEqual(first[2]["metadata"]["name"], second[2]["metadata"]["name"])
        self.assertFalse(first[0]["metadata"]["name"].startswith("gateway-conformance-"))
        pod = first[-1]
        self.assertEqual(pod["spec"]["containers"][0]["image"], "registry/runner@sha256:abc")
        self.assertTrue(pod["spec"]["containers"][0]["securityContext"]["readOnlyRootFilesystem"])

    def test_preexisting_fixture_namespace_prevents_every_mutation(self):
        existing = {"items": [{"metadata": {"name": "gateway-conformance-infra"}}]}
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "results"
            argv = ["run.py", "--context", "explicit", "--gateway-class", "sozu",
                    "--gateway-service", "system/gateway", "--runner-image", "runner:test",
                    "--skip-build", "--version", "test", "--output", str(output)]
            with patch("sys.argv", argv), patch.object(run, "command", return_value=json.dumps(existing)) as command:
                self.assertEqual(run.main(), 2)
                for call in command.call_args_list:
                    self.assertNotIn("create", call.args[0])
                    self.assertNotIn("delete", call.args[0])
                    self.assertEqual(call.args[0][:3], ["kubectl", "--context", "explicit"])
            self.assertEqual((output / "exit-code").read_text(), "2\n")


if __name__ == "__main__":
    unittest.main()
