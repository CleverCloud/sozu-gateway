from copy import deepcopy
import importlib.util
import io
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

    def test_missing_verdicts_with_exec_error_are_infrastructure_failures(self):
        partial = "    --- PASS: TestGatewayConformance/UDPRoute (1.00s)\n"
        code, result = run.check_results(1, partial, {"UDPRoute", "HTTPRouteWeight"}, False)
        self.assertEqual(code, 2)
        self.assertEqual(result["failed"], [])
        self.assertEqual(result["missing"], ["HTTPRouteWeight"])
        self.assertFalse(result["complete"])
        # A completed assertion failure remains distinct even if exec then drops.
        failed = partial + "    --- FAIL: TestGatewayConformance/HTTPRouteWeight (1.00s)\n"
        self.assertEqual(run.check_results(1, failed, {"UDPRoute", "HTTPRouteWeight"}, False)[0], 1)

    def test_report_identity_uses_built_revision_and_observed_sozu_image(self):
        for image in ["clevercloud/sozu:2.2.2", "registry:5000/sozu@sha256:" + "c" * 64]:
            with self.subTest(image=image):
                pod = {"metadata": {"name": "gateway-1"},
                       "spec": {"containers": [{"name": "sozu", "image": image},
                                                {"name": "controller", "image": "controller:test"}]},
                       "status": {"containerStatuses": [{"name": "sozu", "imageID": "sha256:resolved"}]}}
                identity = run.implementation_identity("a" * 40, [pod])
                self.assertEqual(identity["implementation_version"], "a" * 40 + f" ({image})")
                self.assertEqual(identity["sozu_image"], image)
                self.assertEqual(identity["gateway_images"][0]["containers"][0]["imageID"], "sha256:resolved")
                changed = deepcopy(pod)
                changed["spec"]["containers"][0]["image"] = "clevercloud/sozu:another-version"
                with self.assertRaises(RuntimeError):
                    run.implementation_identity("a" * 40, [pod, changed])
        with self.assertRaises(RuntimeError):
            run.implementation_identity("a" * 40, [])

    def test_focused_reports_cannot_be_mistaken_for_full_campaign_files(self):
        self.assertEqual(run.report_name("HTTPRouteWeight"), "focused-report.yaml")
        self.assertEqual(run.report_name(""), "report.yaml")

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
                    "--skip-build", "--controller-revision", "a" * 40, "--output", str(output)]
            with patch("sys.argv", argv), patch.object(run, "command", return_value=json.dumps(existing)) as command:
                self.assertEqual(run.main(), 2)
                for call in command.call_args_list:
                    self.assertNotIn("create", call.args[0])
                    self.assertNotIn("delete", call.args[0])
                    self.assertEqual(call.args[0][:3], ["kubectl", "--context", "explicit"])
            self.assertEqual((output / "exit-code").read_text(), "2\n")

    def test_preexisting_fixture_gatewayclass_prevents_every_mutation(self):
        existing = {"items": [{"metadata": {"name": "gatewayclass-observed-generation-bump", "uid": "existing"}}]}
        def reply(args, **kwargs):
            if args[3:5] == ["get", "namespaces"]:
                return json.dumps({"items": []})
            if args[3:5] == ["get", "gatewayclasses"]:
                return json.dumps(existing)
            self.fail(f"unexpected command: {args}")
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "results"
            argv = ["run.py", "--context", "explicit", "--gateway-class", "sozu",
                    "--gateway-service", "system/gateway", "--runner-image", "runner:test",
                    "--skip-build", "--controller-revision", "a" * 40, "--pr-head-revision", "b" * 40,
                    "--output", str(output)]
            with patch("sys.argv", argv), patch.object(run, "command", side_effect=reply) as command:
                self.assertEqual(run.main(), 2)
                for call in command.call_args_list:
                    self.assertNotIn("create", call.args[0])
                    self.assertNotIn("delete", call.args[0])
                    self.assertEqual(call.args[0][:3], ["kubectl", "--context", "explicit"])
            metadata = json.loads((output / "metadata.json").read_text())
            self.assertIn("refusing existing GatewayClasses", metadata["error"])
            self.assertEqual(metadata["controller_revision"], "a" * 40)
            self.assertEqual(metadata["pr_head_revision"], "b" * 40)
            self.assertEqual(metadata["exit_code"], 2)

    def test_completed_run_retains_report_and_only_cleans_owned_resources(self):
        for retained in [False, True]:
            with self.subTest(retained=retained), tempfile.TemporaryDirectory() as directory:
                output = Path(directory) / "results"
                state = {"namespace_reads": 0, "class_reads": 0, "run_id": None}
                raw_deletes = []
                report = "implementation:\n  project: sozu-gateway\n"
                def reply(args, **kwargs):
                    if args[0] == "git":
                        return "c" * 40
                    self.assertEqual(args[:3], ["kubectl", "--context", "explicit"])
                    parts = args[3:]
                    if parts[:2] == ["get", "namespaces"]:
                        state["namespace_reads"] += 1
                        items = []
                        if state["namespace_reads"] == 3:
                            items = [{"kind": "Namespace", "metadata": {"name": "gateway-conformance-infra",
                                      "uid": "owned-fixture", "annotations": {run.OWNER: state["run_id"]}}},
                                     {"kind": "Namespace", "metadata": {"name": "gateway-conformance-foreign",
                                      "uid": "foreign-fixture", "annotations": {run.OWNER: "another-run"}}}]
                        return json.dumps({"items": items})
                    if parts[:2] == ["get", "gatewayclasses"]:
                        state["class_reads"] += 1
                        items = [{"metadata": {"name": "gatewayclass-observed-generation-bump", "uid": "unowned-class"}}]
                        return json.dumps({"items": items if retained and state["class_reads"] == 3 else []})
                    if parts[:2] == ["get", "gatewayclass"]:
                        return "{}"
                    if parts[:2] == ["get", "service"]:
                        return json.dumps({"status": {"loadBalancer": {"ingress": [{"ip": "192.0.2.1"}]}},
                                           "spec": {"ports": [{"port": 80}], "selector": {"app": "gateway"}}})
                    if parts[:2] == ["get", "pods"]:
                        return json.dumps({"items": [{"metadata": {"name": "gateway"}, "spec": {
                            "containers": [{"name": "sozu", "image": "clevercloud/sozu:2.2.2"}]}}]})
                    if parts[:2] == ["get", "pod"]:
                        return json.dumps({"status": {"containerStatuses": [{"imageID": "sha256:runner"}]}})
                    if parts[0] == "create":
                        obj = json.loads(kwargs["input"])
                        state["run_id"] = obj["metadata"]["annotations"][run.OWNER]
                        obj["metadata"]["uid"] = obj["kind"] + "-uid"
                        return json.dumps(obj)
                    if parts[0] == "delete":
                        self.assertEqual(parts[1], "--raw")
                        raw_deletes.append((parts[2], json.loads(kwargs["input"])["preconditions"]["uid"]))
                        return "{}"
                    if parts[0] == "exec":
                        if parts[-1] == "--catalog":
                            return json.dumps({"revision": run.UPSTREAM, **self.catalog})
                        if parts[-2:] == ["cat", "/results/report.yaml"]:
                            return report
                    if parts[0] in {"wait", "version"}:
                        return "{}"
                    self.fail(f"unexpected command: {args}")
                class Process:
                    stdout = io.StringIO("    --- PASS: TestGatewayConformance/UDPRoute (1.00s)\n"
                                         "--- PASS: TestGatewayConformance (1.00s)\nPASS\n")
                    def wait(self):
                        return 0
                    def poll(self):
                        return 0
                argv = ["run.py", "--context", "explicit", "--gateway-class", "sozu",
                        "--gateway-service", "system/gateway", "--runner-image", "runner:test",
                        "--skip-build", "--controller-revision", "a" * 40, "--pr-head-revision", "b" * 40,
                        "--tests", "UDPRoute", "--output", str(output)]
                with patch("sys.argv", argv), patch.object(run, "command", side_effect=reply), \
                        patch.object(run.subprocess, "Popen", return_value=Process()) as popen:
                    self.assertEqual(run.main(), 2 if retained else 0)
                self.assertEqual((output / "focused-report.yaml").read_text(), report)
                self.assertFalse((output / "report.yaml").exists())
                metadata = json.loads((output / "metadata.json").read_text())
                self.assertEqual(metadata["repository_revision"], "c" * 40)
                self.assertEqual(metadata["pr_head_revision"], "b" * 40)
                self.assertIn("--version=" + "a" * 40 + " (clevercloud/sozu:2.2.2)", popen.call_args.args[0])
                self.assertEqual(bool(metadata["retained_fixture_gateway_classes"]), retained)
                self.assertEqual(bool(metadata["cleanup_errors"]), retained)
                self.assertIn(("/api/v1/namespaces/gateway-conformance-infra", "owned-fixture"), raw_deletes)
                self.assertTrue(all("gatewayclasses" not in path and "foreign" not in path for path, _ in raw_deletes))


if __name__ == "__main__":
    unittest.main()
