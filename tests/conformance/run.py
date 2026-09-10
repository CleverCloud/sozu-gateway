#!/usr/bin/env python3
"""Run the unmodified Gateway API suite from a Pod on an explicit cluster."""

import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
import re
import subprocess
import sys
import uuid

ROOT = Path(__file__).resolve().parents[2]
UPSTREAM = "ca6c2a65454737236fb7a937bd9b17e42b07e9de"
RUN_NAMESPACE = "sozu-gateway-conformance"
OWNER = "conformance.sozu.io/run-id"
# The only cluster-scoped fixture in the pinned suite. Namespace annotations
# do not establish ownership of it, so outer cleanup must never delete it.
FIXTURE_GATEWAY_CLASSES = {"gatewayclass-observed-generation-bump"}
PROFILES = "GATEWAY-HTTP,GATEWAY-TCP,GATEWAY-UDP"
EXTENDED = ["HTTPRouteResponseHeaderModification", "HTTPRouteSchemeRedirect", "HTTPRouteMethodMatching"]
CORE = {"Gateway", "HTTPRoute", "ReferenceGrant", "TCPRoute", "UDPRoute"}
VERDICT = re.compile(r"^\s*--- (PASS|FAIL|SKIP): TestGatewayConformance/([^/\s]+) \(")


def now():
    return datetime.now(timezone.utc).isoformat()


def command(args, **kwargs):
    return subprocess.run(args, text=True, check=True, capture_output=True, **kwargs).stdout


def requested_names(raw, catalog):
    names = raw.split(",") if raw else []
    known = {entry["name"] for entry in catalog["tests"]}
    if len(names) != len(set(names)):
        raise ValueError("duplicate ShortName in --tests")
    unknown = set(names) - known
    if unknown:
        raise ValueError("unknown upstream ShortName(s): " + ", ".join(sorted(unknown)))
    return names


def expected_names(names, catalog):
    if names:
        return set(names)
    supported = CORE | set(EXTENDED)
    return {e["name"] for e in catalog["tests"] if set(e["features"]) <= supported}


def verdicts(log):
    return {m[2]: m[1] for line in log.splitlines() if (m := VERDICT.match(line))}


def check_results(exit_code, log, expected, report_exists):
    actual = verdicts(log)
    missing = sorted(expected - actual.keys())
    skipped = sorted(name for name in expected if actual.get(name) == "SKIP")
    failed = sorted(name for name in expected if actual.get(name) == "FAIL")
    wrapper_pass = bool(re.search(r"^--- PASS: TestGatewayConformance \(", log, re.MULTILINE))
    complete = not missing and report_exists
    # A zero exit without the selected assertions/report is never a green run.
    code = 1 if failed else 2 if exit_code or missing or skipped or not report_exists or not wrapper_pass else 0
    return code, {"complete": complete, "verdicts": actual, "missing": missing,
                  "selected_skipped": skipped, "failed": failed, "wrapper_pass": wrapper_pass}


def implementation_identity(controller_revision, pods):
    images = []
    for pod in pods:
        statuses = {c["name"]: c for c in pod.get("status", {}).get("containerStatuses", [])}
        images.append({"pod": pod["metadata"]["name"], "containers": [
            {"name": c["name"], "image": c["image"], "imageID": statuses.get(c["name"], {}).get("imageID")}
            for c in pod["spec"]["containers"]]})
    sozu_images = {c["image"] for pod in images for c in pod["containers"] if c["name"] == "sozu"}
    if len(sozu_images) != 1:
        raise RuntimeError("the publish Service must select Pods with exactly one Sōzu image version")
    sozu_image = sozu_images.pop()
    return {"controller_revision": controller_revision, "sozu_image": sozu_image,
            "implementation_version": f"{controller_revision} ({sozu_image})", "gateway_images": images}


def fixture_gateway_classes(classes):
    return [obj for obj in classes if obj["metadata"]["name"] in FIXTURE_GATEWAY_CLASSES]


def report_name(selected):
    return "focused-report.yaml" if selected else "report.yaml"


def owned(obj, run_id):
    return obj.get("metadata", {}).get("annotations", {}).get(OWNER) == run_id


def resources(run_id, image):
    annotations = {OWNER: run_id}
    name = "sozu-conformance-" + run_id[:12]
    def obj(api, kind, resource_name, **extra):
        return {"apiVersion": api, "kind": kind,
                "metadata": {"name": resource_name, "annotations": annotations}, **extra}
    ns = obj("v1", "Namespace", RUN_NAMESPACE)
    sa = obj("v1", "ServiceAccount", "runner")
    sa["metadata"]["namespace"] = RUN_NAMESPACE
    rw = ["get", "list", "watch", "create", "update", "patch", "delete"]
    role = obj("rbac.authorization.k8s.io/v1", "ClusterRole", name, rules=[
        {"apiGroups": [""], "resources": ["namespaces", "services", "secrets", "configmaps", "pods"], "verbs": rw},
        {"apiGroups": [""], "resources": ["pods/log"], "verbs": ["get"]},
        {"apiGroups": [""], "resources": ["pods/exec"], "verbs": ["get", "create"]},
        {"apiGroups": ["apps"], "resources": ["deployments"], "verbs": rw},
        {"apiGroups": ["discovery.k8s.io"], "resources": ["endpointslices"], "verbs": rw},
        {"apiGroups": ["gateway.networking.k8s.io"], "resources": ["gatewayclasses", "gateways", "httproutes", "tcproutes", "udproutes", "grpcroutes", "tlsroutes", "referencegrants", "backendtlspolicies", "listenersets"], "verbs": rw},
        {"apiGroups": ["apiextensions.k8s.io"], "resources": ["customresourcedefinitions"], "verbs": ["get", "list", "watch"]},
    ])
    binding = obj("rbac.authorization.k8s.io/v1", "ClusterRoleBinding", name,
                  roleRef={"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": name},
                  subjects=[{"kind": "ServiceAccount", "name": "runner", "namespace": RUN_NAMESPACE}])
    pod = obj("v1", "Pod", "runner", spec={
        "serviceAccountName": "runner", "restartPolicy": "Never", "terminationGracePeriodSeconds": 5,
        "securityContext": {"runAsUser": 1000, "runAsGroup": 1000, "fsGroup": 1000, "runAsNonRoot": True},
        "containers": [{"name": "runner", "image": image, "imagePullPolicy": "IfNotPresent",
                        "command": ["/bin/sh", "-c", "sleep 604800"],
                        "securityContext": {"allowPrivilegeEscalation": False, "readOnlyRootFilesystem": True,
                                            "capabilities": {"drop": ["ALL"]}},
                        "resources": {"requests": {"cpu": "500m", "memory": "512Mi"},
                                      "limits": {"cpu": "2", "memory": "2Gi"}},
                        "volumeMounts": [{"name": "results", "mountPath": "/results"},
                                         {"name": "tmp", "mountPath": "/tmp"}]}],
        "volumes": [{"name": "results", "emptyDir": {}}, {"name": "tmp", "emptyDir": {}}],
    })
    pod["metadata"]["namespace"] = RUN_NAMESPACE
    return [ns, sa, role, binding, pod]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--context", required=True, help="explicit kubectl context")
    parser.add_argument("--gateway-class", required=True)
    parser.add_argument("--gateway-service", required=True, help="namespace/name of the published LoadBalancer Service")
    parser.add_argument("--tests", default="", help="comma-separated exact upstream ShortNames; omit for the full HTTP/TCP/UDP campaign")
    parser.add_argument("--runner-image", required=True, help="image tag to build, or an existing digest with --skip-build")
    parser.add_argument("--skip-build", action="store_true", help="use an image built from tests/conformance/Dockerfile")
    parser.add_argument("--kind-name", help="load the built image into this Kind cluster instead of pushing it")
    parser.add_argument("--output", required=True, type=Path, help="new directory for immutable run artifacts")
    parser.add_argument("--controller-revision", required=True, help="Git SHA of the checkout used to build the deployed controller")
    parser.add_argument("--pr-head-revision", default="", help="PR head SHA for traceability; does not replace the built checkout SHA")
    args = parser.parse_args()
    if not re.fullmatch(r"[a-z0-9][a-z0-9.-]*/[a-z0-9][a-z0-9.-]*", args.gateway_service):
        parser.error("--gateway-service must be namespace/name")
    if not re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", args.controller_revision):
        parser.error("--controller-revision must be the full Git SHA of the built checkout")
    if args.pr_head_revision and not re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", args.pr_head_revision):
        parser.error("--pr-head-revision must be a full Git SHA")
    args.output.mkdir(parents=True, exist_ok=False)
    run_id = uuid.uuid4().hex
    kube = ["kubectl", "--context", args.context]
    metadata = {"started_at": now(), "run_id": run_id, "upstream_revision": UPSTREAM,
                "suite_version": "v1.6.2", "context": args.context, "gateway_class": args.gateway_class,
                "gateway_service": args.gateway_service, "controller_revision": args.controller_revision,
                "pr_head_revision": args.pr_head_revision or None,
                "report_scope": "focused" if args.tests else "full", "report_file": report_name(args.tests),
                "runner_image": args.runner_image, "tests": args.tests, "qps": 100, "burst": 200,
                "profiles": PROFILES.split(","), "extended_features": EXTENDED, "exit_code": 2}
    created = []
    code = 2
    process = None
    def save():
        (args.output / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    def get(*parts):
        return json.loads(command(kube + list(parts) + ["-o", "json"]))
    def raw_delete(obj):
        kind = obj["kind"]
        name = obj["metadata"]["name"]
        uid = obj["metadata"]["uid"]
        if kind == "Namespace":
            path = "/api/v1/namespaces/" + name
        else:
            resource = {"ClusterRole": "clusterroles", "ClusterRoleBinding": "clusterrolebindings"}[kind]
            path = "/apis/rbac.authorization.k8s.io/v1/" + resource + "/" + name
        body = json.dumps({"apiVersion": "v1", "kind": "DeleteOptions", "preconditions": {"uid": uid}})
        command(kube + ["delete", "--raw", path, "-f", "-"], input=body)
    def check_fixture_classes():
        existing = fixture_gateway_classes(get("get", "gatewayclasses")["items"])
        if existing:
            raise RuntimeError("suite cleanup deletes its GatewayClass fixture; refusing existing GatewayClasses: "
                               + ", ".join(obj["metadata"]["name"] for obj in existing))
    save()
    try:
        existing = get("get", "namespaces")["items"]
        conflicts = [n["metadata"]["name"] for n in existing
                     if n["metadata"]["name"].startswith("gateway-conformance-")
                     or n["metadata"]["name"] == RUN_NAMESPACE]
        if conflicts:
            raise RuntimeError("suite cleanup deletes whole namespaces; refusing existing namespaces: " + ", ".join(conflicts))
        check_fixture_classes()
        metadata["gateway_class_status"] = get("get", "gatewayclass", args.gateway_class).get("status", {})
        ns, name = args.gateway_service.split("/")
        svc = get("get", "service", name, "-n", ns)
        addresses = [p.get("ip") or p.get("hostname") for p in svc.get("status", {}).get("loadBalancer", {}).get("ingress", [])]
        if not addresses or any(not a for a in addresses):
            raise RuntimeError("the publish Service has no actual LoadBalancer addresses")
        if not any(p["port"] == 80 and p.get("protocol", "TCP") == "TCP" for p in svc["spec"]["ports"]):
            raise RuntimeError("the publish Service must expose the standard HTTP port 80")
        metadata["published_service_addresses"] = addresses
        metadata["service_ports"] = svc["spec"]["ports"]
        metadata["kubernetes"] = json.loads(command(kube + ["version", "-o", "json"]))
        metadata["repository_revision"] = command(["git", "-C", str(ROOT), "rev-parse", "HEAD"]).strip()
        selector = ",".join(f"{k}={v}" for k, v in sorted(svc["spec"].get("selector", {}).items()))
        if not selector:
            raise RuntimeError("the publish Service must have a Pod selector to identify the Sōzu image")
        pods = get("get", "pods", "-n", ns, "-l", selector)["items"]
        metadata.update(implementation_identity(args.controller_revision, pods))
        if not args.skip_build:
            subprocess.run(["docker", "build", "--tag", args.runner_image, str(ROOT / "tests/conformance")], check=True)
            if args.kind_name:
                subprocess.run(["kind", "load", "docker-image", "--name", args.kind_name, args.runner_image], check=True)
            else:
                subprocess.run(["docker", "push", args.runner_image], check=True)
        manifest = resources(run_id, args.runner_image)
        (args.output / "runner-resources.json").write_text(json.dumps({"apiVersion": "v1", "kind": "List", "items": manifest}, indent=2) + "\n")
        for resource in manifest:
            result = json.loads(command(kube + ["create", "-f", "-", "-o", "json"], input=json.dumps(resource)))
            if resource["kind"] in {"Namespace", "ClusterRole", "ClusterRoleBinding"}:
                created.append(result)
        command(kube + ["wait", "-n", RUN_NAMESPACE, "--for=condition=Ready", "pod/runner", "--timeout=300s"])
        pod = get("get", "pod", "runner", "-n", RUN_NAMESPACE)
        metadata["runner_image_id"] = pod["status"]["containerStatuses"][0]["imageID"]
        remote = kube + ["exec", "-n", RUN_NAMESPACE, "runner", "--"]
        catalog = json.loads(command(remote + ["/usr/local/bin/gateway-api.test", "--catalog"]))
        if catalog["revision"] != UPSTREAM:
            raise RuntimeError("runner catalog does not match the pinned upstream revision")
        names = requested_names(args.tests, catalog)
        # Image builds can take minutes. Recheck after acquiring the runner
        # namespace, immediately before the suite can create/delete fixtures.
        existing = get("get", "namespaces")["items"]
        conflicts = [n["metadata"]["name"] for n in existing
                     if n["metadata"]["name"].startswith("gateway-conformance-")]
        if conflicts:
            raise RuntimeError("fixture namespaces appeared before execution: " + ", ".join(conflicts))
        check_fixture_classes()
        expected = expected_names(names, catalog)
        metadata["selected_tests"] = sorted(expected)
        (args.output / "catalog.json").write_text(json.dumps(catalog, indent=2) + "\n")
        invocation = remote + ["/usr/local/bin/gateway-api.test", "-test.v", "-test.run=^TestGatewayConformance$",
            "-test.timeout=150m", "--gateway-class=" + args.gateway_class,
            "--conformance-profiles=" + PROFILES, "--supported-features=" + ",".join(EXTENDED),
            "--disable-parallel-tests=true", "--cleanup-base-resources=true", "--cleanup-test-resources=true",
            "--namespace-annotations=" + OWNER + "=" + run_id, "--selected-tests=" + args.tests,
            "--probe-addresses=" + ",".join(addresses), "--organization=clevercloud", "--project=sozu-gateway",
            "--url=https://github.com/CleverCloud/sozu-gateway", "--contact=https://github.com/CleverCloud/sozu-gateway/issues",
            "--version=" + metadata["implementation_version"], "--report-output=/results/report.yaml"]
        metadata["command"] = invocation
        save()
        with (args.output / "suite.log").open("w") as log:
            process = subprocess.Popen(invocation, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            for line in process.stdout:
                log.write(line)
                log.flush()
                if line.strip().startswith(("=== RUN", "--- PASS", "--- FAIL", "--- SKIP", "PASS", "FAIL", "panic:")):
                    print(line.rstrip(), flush=True)
            code = process.wait()
        metadata["suite_exit_code"] = code
        try:
            report = command(remote + ["cat", "/results/report.yaml"])
            (args.output / metadata["report_file"]).write_text(report)
        except subprocess.CalledProcessError:
            pass
        code, metadata["results"] = check_results(code, (args.output / "suite.log").read_text(), expected,
                                                  (args.output / metadata["report_file"]).exists())
    except (ValueError, RuntimeError, OSError, subprocess.CalledProcessError) as error:
        code = 2
        metadata["error"] = str(error)
        print(str(error), file=sys.stderr)
    except KeyboardInterrupt:
        code = 130
        metadata["error"] = "interrupted"
    finally:
        if process is not None and process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        cleanup_errors = []
        # Stop the runner before removing fixture namespaces after an interrupted
        # exec. UID preconditions prevent deleting a replacement resource.
        runner_stopped = True
        for resource in created:
            if resource["kind"] != "Namespace":
                continue
            try:
                raw_delete(resource)
                command(kube + ["wait", "--for=delete", "namespace/" + RUN_NAMESPACE, "--timeout=120s"])
            except subprocess.CalledProcessError as error:
                runner_stopped = False
                cleanup_errors.append(str(error))
        try:
            namespaces = get("get", "namespaces")["items"] if runner_stopped else []
            for namespace in namespaces:
                if owned(namespace, run_id) and namespace["metadata"]["name"].startswith("gateway-conformance-"):
                    raw_delete(namespace)
                    command(kube + ["wait", "--for=delete", "namespace/" + namespace["metadata"]["name"], "--timeout=120s"])
        except subprocess.CalledProcessError as error:
            cleanup_errors.append(str(error))
        if created and runner_stopped:
            try:
                remaining = fixture_gateway_classes(get("get", "gatewayclasses")["items"])
                metadata["retained_fixture_gateway_classes"] = [
                    {"name": obj["metadata"]["name"], "uid": obj["metadata"]["uid"]} for obj in remaining]
                if remaining:
                    cleanup_errors.append("GatewayClass fixtures remain; inspect their recorded UIDs before manual cleanup")
            except subprocess.CalledProcessError as error:
                cleanup_errors.append(str(error))
        for resource in reversed(created):
            if resource["kind"] == "Namespace":
                continue
            try:
                raw_delete(resource)
            except subprocess.CalledProcessError as error:
                cleanup_errors.append(str(error))
        metadata["cleanup_errors"] = cleanup_errors
        if cleanup_errors and code == 0:
            code = 2
        metadata.update(finished_at=now(), exit_code=code)
        (args.output / "exit-code").write_text(str(code) + "\n")
        save()
    return code


if __name__ == "__main__":
    sys.exit(main())
