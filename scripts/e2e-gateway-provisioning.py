#!/usr/bin/env python3
"""Exercise automatic Gateway creation and cleanup on an existing installation."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import threading
import time
import traceback
import uuid
from datetime import datetime, timezone
from urllib.parse import quote

ECHO_IMAGE = "registry.k8s.io/gateway-api/echo-basic:v1.5.1"
INSTALLATION = "sozu.io/installation-uid"
GATEWAY_UID = "sozu.io/gateway-uid"
RESOURCE_KINDS = "deployments,services,configmaps,poddisruptionbudgets,pods,replicasets"
PROBE = r"""
import http.client, json, sys, time
settings = json.loads(sys.argv[1])
rows = []
for target in settings["targets"]:
    for sample in range(settings["count"]):
        row = {"target": target["name"], "sample": sample, "time_ns": time.time_ns()}
        connection = http.client.HTTPConnection(target["ip"], target["port"], timeout=2)
        try:
            connection.request("GET", "/", headers={"Connection": "close"})
            response = connection.getresponse()
            body = response.read(65537)
            row.update(status=response.status, body=body.decode("utf-8", "replace"))
            try:
                decoded = json.loads(body)
                row.update(pod=decoded.get("pod"), namespace=decoded.get("namespace"))
            except (ValueError, AttributeError):
                pass
        except Exception as error:
            row["error"] = str(error)
        finally:
            connection.close()
        rows.append(row)
print(json.dumps(rows))
"""


def now():
    return datetime.now(timezone.utc).isoformat()


def metadata(obj):
    item = obj["metadata"]
    return {key: item[key] for key in ("name", "namespace", "uid", "generation", "resourceVersion",
            "deletionTimestamp", "labels", "ownerReferences") if key in item}


def ready(obj, condition="Ready"):
    return any(c.get("type") == condition and c.get("status") == "True"
               for c in obj.get("status", {}).get("conditions", []))


class TrafficMonitor:
    """Sample the surviving Gateway continuously while its neighbour changes."""
    def __init__(self, run, target):
        self.run, self.target = run, target
        self.stop = threading.Event()
        self.started = threading.Event()
        self.rows, self.errors = [], []
        self.thread = threading.Thread(target=self.sample, daemon=True)

    def sample(self):
        while not self.stop.is_set():
            try:
                rows = self.run.probe([self.target], 2)
                self.rows.extend(rows)
                self.run.event("survivor-sample", rows=rows)
            except Exception as error:
                self.errors.append(str(error))
            self.started.set()
            self.stop.wait(0.25)

    def __enter__(self):
        self.thread.start()
        if not self.started.wait(30):
            self.stop.set()
            raise RuntimeError("surviving Gateway monitor did not start")
        return self

    def __exit__(self, kind, error, tb):
        self.stop.set()
        self.thread.join(30)
        self.run.save("surviving-gateway-traffic.json", {"rows": self.rows, "errors": self.errors})
        if kind is None:
            if self.thread.is_alive() or self.errors or not self.rows:
                raise AssertionError(f"surviving Gateway monitor incomplete: {self.errors}")
            self.run.validate_traffic(self.rows, [self.target])


class Run:
    def __init__(self, args):
        self.args = args
        self.output = Path(args.output)
        self.output.mkdir(parents=True, exist_ok=False)
        self.run_id = uuid.uuid4().hex[:12]
        self.test_namespace = "sozu-provision-e2e-" + self.run_id
        self.namespace_uid = None
        self.gateways = {}
        self.installation_uid = None
        self.runner = args.runner.split("/", 1) if args.runner else None
        self.lock = threading.Lock()
        self.summary = {"started": now(), "context": args.context, "installation_namespace": args.namespace,
                        "gateway_class": args.gateway_class, "test_namespace": self.test_namespace,
                        "result": "RUNNING", "steps": [], "cleanup_errors": []}

    def save(self, name, data):
        (self.output / name).write_text(json.dumps(data, indent=2) + "\n")

    def event(self, event_type, **details):
        entry = {"time": now(), "event": event_type, **details}
        with self.lock:
            with (self.output / "events.jsonl").open("a") as stream:
                stream.write(json.dumps(entry) + "\n")

    def step(self, name, **details):
        entry = {"name": name, "time": now(), **details}
        self.summary["steps"].append(entry)
        self.event("step", **entry)
        print(f"{entry['time']} {name}", flush=True)

    def kubectl(self, *arguments, input=None, check=True, timeout=45):
        command = ["kubectl", "--context", self.args.context, "--request-timeout=20s", *arguments]
        result = subprocess.run(command, input=input, capture_output=True, text=True, timeout=timeout)
        self.event("kubectl", arguments=list(arguments), exit_code=result.returncode,
                   stderr=result.stderr[-4000:] if result.returncode else "")
        if check and result.returncode:
            raise RuntimeError(f"kubectl {' '.join(arguments[:4])}: {result.stderr.strip()}")
        return result

    def get(self, resource, name=None, namespace=None, selector=None, optional=False):
        arguments = ["get", resource]
        if name:
            arguments.append(name)
        if namespace:
            arguments += ["--namespace", namespace]
        if selector:
            arguments += ["--selector", selector]
        arguments += ["-o", "json"]
        result = self.kubectl(*arguments, check=False)
        if result.returncode:
            if optional and ("(NotFound)" in result.stderr or "doesn't have a resource type" in result.stderr):
                return None
            raise RuntimeError(result.stderr.strip())
        return json.loads(result.stdout)

    def create(self, resource):
        result = self.kubectl("create", "--filename", "-", "-o", "json", input=json.dumps(resource))
        return json.loads(result.stdout)

    def wait(self, description, check, timeout=None):
        deadline = time.monotonic() + (timeout or self.args.timeout)
        last_error = None
        while time.monotonic() < deadline:
            try:
                value = check()
                if value:
                    return value
            except (RuntimeError, subprocess.TimeoutExpired) as error:
                last_error = str(error)
            time.sleep(1)
        raise TimeoutError(f"{description} did not converge; last error: {last_error}")

    def delete_uid(self, resource, name, uid, namespace=None):
        if resource == "namespaces":
            path = "/api/v1/namespaces/" + quote(name, safe="")
        elif resource == "gateways":
            path = "/apis/gateway.networking.k8s.io/v1/namespaces/" + quote(namespace, safe="") + "/gateways/" + quote(name, safe="")
        else:
            raise ValueError(f"unsupported cleanup kind {resource}")
        body = {"apiVersion": "v1", "kind": "DeleteOptions", "preconditions": {"uid": uid},
                "propagationPolicy": "Background"}
        result = self.kubectl("delete", "--raw", path, "--filename", "-", input=json.dumps(body), check=False)
        self.event("uid-delete", resource=resource, namespace=namespace, name=name, uid=uid,
                   exit_code=result.returncode, response=result.stdout[-4000:])
        if result.returncode and "(NotFound)" not in result.stderr:
            raise RuntimeError(f"UID-guarded cleanup refused {resource}/{name}: {result.stderr.strip()}")

    def find_installation(self):
        maps = self.get("configmaps", namespace=self.args.namespace)["items"]
        candidates = []
        for item in maps:
            encoded = item.get("data", {}).get("template.json")
            if not encoded:
                continue
            template = json.loads(encoded)
            if template.get("namespace") == self.args.namespace and template.get("template_config_map") == item["metadata"]["name"]:
                candidates.append((item, template))
        if len(candidates) != 1:
            raise RuntimeError(f"expected one automatic provisioning template in {self.args.namespace}, found {len(candidates)}")
        anchor, self.template = candidates[0]
        self.anchor = metadata(anchor)
        self.installation_uid = self.anchor["uid"]
        if self.template["service"]["spec"]["type"] != "ClusterIP":
            raise RuntimeError("this test requires gatewayProvisioning.service.type=ClusterIP to avoid allocating external addresses")
        containers = self.template["deployment"]["spec"]["template"]["spec"]["containers"]
        controller = next(c for c in containers if c["name"] == "controller")
        env = {e["name"]: e.get("value") for e in controller["env"]}
        http = [e for e in json.loads(env["SOZU_GW_EXPOSURE"]) if e["protocol"] == "HTTP"]
        if len(http) != 1:
            raise RuntimeError("this test requires exactly one configured HTTP exposure")
        self.http_port = http[0]["port"]
        self.controller_name = env["SOZU_GW_CONTROLLER"]
        klass = self.get("gatewayclasses", self.args.gateway_class)
        if klass["spec"]["controllerName"] != self.controller_name:
            raise RuntimeError("GatewayClass is not owned by this installation's controller")
        self.summary.update(installation=self.anchor, controller_image=controller["image"],
            sozu_image=next(c["image"] for c in containers if c["name"] == "sozu"),
            template_sha256=hashlib.sha256(anchor.get("data", {}).get("template.json", "").encode()).hexdigest())
        self.step("existing-installation-verified", anchor=self.anchor, gateway_class=metadata(klass))

    def setup(self):
        namespace = self.create({"apiVersion": "v1", "kind": "Namespace", "metadata": {
            "name": self.test_namespace, "labels": {"sozu.io/provisioning-e2e": self.run_id}}})
        self.namespace_uid = namespace["metadata"]["uid"]
        self.summary["test_namespace_uid"] = self.namespace_uid
        for name in ["backend-a", "backend-b"]:
            labels = {"app": name, "sozu.io/provisioning-e2e": self.run_id}
            self.create({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {
                "name": name, "namespace": self.test_namespace}, "spec": {"replicas": 1,
                "selector": {"matchLabels": labels}, "template": {"metadata": {"labels": labels}, "spec": {
                    "automountServiceAccountToken": False,
                    "containers": [{"name": "echo", "image": ECHO_IMAGE, "ports": [{"containerPort": 3000}],
                        "env": [{"name": "POD_NAME", "valueFrom": {"fieldRef": {"fieldPath": "metadata.name"}}},
                                {"name": "NAMESPACE", "valueFrom": {"fieldRef": {"fieldPath": "metadata.namespace"}}}],
                        "readinessProbe": {"httpGet": {"path": "/", "port": 3000}, "periodSeconds": 1},
                        "resources": {"requests": {"cpu": "10m", "memory": "32Mi"},
                                      "limits": {"cpu": "100m", "memory": "128Mi"}}}]}}}})
            self.create({"apiVersion": "v1", "kind": "Service", "metadata": {"name": name, "namespace": self.test_namespace},
                "spec": {"selector": labels, "ports": [{"name": "http", "port": 3000, "targetPort": 3000}]}})
        self.wait("backend Pods ready", lambda: len([p for p in self.get("pods", namespace=self.test_namespace)["items"] if ready(p)]) == 2)
        backend_pods = self.get("pods", namespace=self.test_namespace)["items"]
        self.save("backend-pods.json", [{"metadata": metadata(p), "status": p.get("status", {})} for p in backend_pods])
        def ready_endpoints():
            slices = self.get("endpointslices", namespace=self.test_namespace)["items"]
            for name in ["backend-a", "backend-b"]:
                expected = {p["status"]["podIP"] for p in backend_pods if p["metadata"]["labels"]["app"] == name}
                actual = {address for item in slices if item["metadata"]["labels"].get("kubernetes.io/service-name") == name
                          for endpoint in item.get("endpoints", []) if endpoint.get("conditions", {}).get("ready", True)
                          for address in endpoint.get("addresses", [])}
                if expected != actual:
                    return None
            return slices
        slices = self.wait("backend EndpointSlices contain ready Pod IPs", ready_endpoints)
        self.save("backend-endpointslices.json", slices)
        if self.runner is None:
            self.create({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "probe", "namespace": self.test_namespace},
                "spec": {"automountServiceAccountToken": False, "restartPolicy": "Never", "containers": [{
                    "name": "probe", "image": "python:3.13-alpine", "command": ["python3", "-c", "import time; time.sleep(3600)"],
                    "resources": {"requests": {"cpu": "10m", "memory": "24Mi"}, "limits": {"cpu": "100m", "memory": "96Mi"}}}]}})
            self.runner = [self.test_namespace, "probe"]
        self.wait("probe Pod ready", lambda: ready(self.get("pods", self.runner[1], namespace=self.runner[0])))
        self.kubectl("exec", "--namespace", self.runner[0], self.runner[1], "--", "python3", "--version")
        runner = self.get("pods", self.runner[1], namespace=self.runner[0])
        self.save("runner-pod.json", {"metadata": metadata(runner), "status": runner.get("status", {})})
        self.step("temporary-backends-and-probe-ready", runner="/".join(self.runner))

    def gateway(self, name):
        created = self.create({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "Gateway", "metadata": {
            "name": name, "namespace": self.test_namespace}, "spec": {"gatewayClassName": self.args.gateway_class,
            "listeners": [{"name": "http", "protocol": "HTTP", "port": self.http_port}]}})
        uid = created["metadata"]["uid"]
        self.gateways[uid] = name
        self.event("gateway-created", identity=metadata(created))
        return uid

    def route(self, name, gateway, backend):
        return self.create({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute", "metadata": {
            "name": name, "namespace": self.test_namespace}, "spec": {"parentRefs": [{"name": gateway}],
            "rules": [{"matches": [{"path": {"type": "PathPrefix", "value": "/"}}],
                       "backendRefs": [{"name": backend, "port": 3000}]}]}})

    def status_ready(self, name, uid, route):
        gateway = self.get("gateways", name, namespace=self.test_namespace)
        if gateway["metadata"]["uid"] != uid:
            raise RuntimeError("Gateway UID changed unexpectedly")
        generation = gateway["metadata"]["generation"]
        conditions = gateway.get("status", {}).get("conditions", [])
        if not all(any(c.get("type") == kind and c.get("status") == "True" and c.get("observedGeneration") == generation
                       for c in conditions) for kind in ["Accepted", "Programmed"]):
            return False
        obj = self.get("httproutes", route, namespace=self.test_namespace)
        for parent in obj.get("status", {}).get("parents", []):
            if parent.get("controllerName") != self.controller_name or parent.get("parentRef", {}).get("name") != name:
                continue
            if all(any(c.get("type") == kind and c.get("status") == "True" and c.get("observedGeneration") == obj["metadata"]["generation"]
                       for c in parent.get("conditions", [])) for kind in ["Accepted", "ResolvedRefs"]):
                self.event("ready-status", gateway={"metadata": metadata(gateway), "status": gateway["status"]},
                           route={"metadata": metadata(obj), "status": obj["status"]})
                return True
        return False

    def instance(self, gateway_name, uid, route, backend):
        self.wait(f"Gateway {gateway_name} current status", lambda: self.status_ready(gateway_name, uid, route))
        selector = f"{INSTALLATION}={self.installation_uid},{GATEWAY_UID}={uid}"
        services = self.get("services", namespace=self.args.namespace, selector=selector)["items"]
        traffic = [s for s in services if s["metadata"].get("labels", {}).get("sozu.io/metrics-service") != "true"]
        if len(traffic) != 1:
            raise AssertionError(f"Gateway {gateway_name} has {len(traffic)} traffic Services")
        service = traffic[0]
        if service["spec"]["type"] != "ClusterIP" or service["spec"]["clusterIP"] == "None":
            raise AssertionError("generated Service did not retain ClusterIP exposure")
        expected = {INSTALLATION: self.installation_uid, GATEWAY_UID: uid}
        if service["spec"]["selector"] != expected:
            raise AssertionError("generated Service does not select this exact installation and Gateway UID")
        def ready_pods():
            pods = self.get("pods", namespace=self.args.namespace, selector=selector)["items"]
            return pods if any(ready(p) for p in pods) else None
        pods = self.wait(f"Gateway {gateway_name} ready instance Pods", ready_pods)
        deployments = self.get("deployments", namespace=self.args.namespace, selector=selector)["items"]
        if len(deployments) != 1:
            raise AssertionError("expected one dedicated Deployment per Gateway")
        pod_spec = deployments[0]["spec"]["template"]["spec"]
        self.event("instance", gateway_name=gateway_name, gateway_uid=uid, service={"metadata": metadata(service), "spec": service["spec"]},
                   deployment=metadata(deployments[0]), pods=[{"metadata": metadata(p), "status": p.get("status", {})} for p in pods])
        return {"name": gateway_name, "ip": service["spec"]["clusterIP"], "port": self.http_port,
                "backend": backend, "service_uid": service["metadata"]["uid"], "gateway_uid": uid,
                "deployment_uid": deployments[0]["metadata"]["uid"], "service_account": pod_spec["serviceAccountName"]}

    def probe(self, targets, count):
        settings = json.dumps({"targets": targets, "count": count})
        result = self.kubectl("--request-timeout=0", "exec", "--namespace", self.runner[0], self.runner[1], "--",
                              "python3", "-c", PROBE, settings, timeout=max(30, 5 * count * len(targets) + 10))
        return json.loads(result.stdout)

    def validate_traffic(self, rows, targets):
        expected = {target["name"]: target["backend"] for target in targets}
        if not rows:
            raise AssertionError("traffic sample is empty")
        bad = [row for row in rows if row.get("status") != 200 or row.get("namespace") != self.test_namespace
               or not str(row.get("pod", "")).startswith(expected[row["target"]] + "-")]
        if bad:
            raise AssertionError(f"{len(bad)}/{len(rows)} incorrect responses; first: {bad[0]}")

    def traffic(self, name, targets):
        # Readiness and the local routing cache can briefly race. Preserve every
        # attempted batch, then require twenty fresh connections per Gateway.
        def converged():
            rows = self.probe(targets, 3)
            self.event("traffic-convergence", stage=name, rows=rows)
            try:
                self.validate_traffic(rows, targets)
                return True
            except AssertionError:
                return False
        self.wait(f"{name} traffic convergence", converged, timeout=60)
        rows = self.probe(targets, 20)
        self.save(name + "-traffic.json", rows)
        self.validate_traffic(rows, targets)
        self.step(name, requests=len(rows), failures=0)

    def owned_resources(self, uid):
        selector = f"{INSTALLATION}={self.installation_uid},{GATEWAY_UID}={uid}"
        items = self.get(RESOURCE_KINDS, namespace=self.args.namespace, selector=selector)["items"]
        monitors = self.get("servicemonitors.monitoring.coreos.com", namespace=self.args.namespace, selector=selector, optional=True)
        if monitors:
            items.extend(monitors["items"])
        return [{"kind": item["kind"], "metadata": metadata(item)} for item in items]

    def collected(self, uid):
        remaining = self.owned_resources(uid)
        self.event("owned-resource-cleanup", gateway_uid=uid, remaining=remaining)
        return not remaining

    def rbac(self, targets):
        accounts = sorted({t["service_account"] for t in targets})
        results = []
        for account in accounts:
            for namespace in [self.args.namespace, self.test_namespace]:
                result = self.kubectl("auth", "can-i", "create", "deployments.apps", "--namespace", namespace,
                    "--as", f"system:serviceaccount:{self.args.namespace}:{account}", check=False)
                row = {"account": account, "namespace": namespace, "exit_code": result.returncode,
                       "answer": result.stdout.strip(), "stderr": result.stderr.strip()}
                results.append(row)
                if row["answer"] != "no" or result.returncode != 1:
                    raise AssertionError(f"worker must not create Deployments: {row}")
        self.save("worker-rbac.json", results)
        self.step("workers-cannot-create-deployments", checks=len(results))

    def exercise(self):
        self.find_installation()
        self.setup()
        first = self.gateway("gateway-a")
        second = self.gateway("gateway-b")
        self.route("route-a", "gateway-a", "backend-a")
        self.route("route-b", "gateway-b", "backend-b")
        a = self.instance("gateway-a", first, "route-a", "backend-a")
        b = self.instance("gateway-b", second, "route-b", "backend-b")
        if a["ip"] == b["ip"] or a["service_uid"] == b["service_uid"] or a["deployment_uid"] == b["deployment_uid"]:
            raise AssertionError("Gateways did not receive independent Services and Deployments")
        self.traffic("independent-default-routes", [a, b])
        self.rbac([a, b])
        with TrafficMonitor(self, b):
            self.delete_uid("gateways", "gateway-a", first, self.test_namespace)
            self.wait("first Gateway deletion", lambda: self.get("gateways", "gateway-a", namespace=self.test_namespace, optional=True) is None)
            self.wait("first instance garbage collection", lambda: self.collected(first))
            self.step("deleted-gateway-instance-collected", gateway_uid=first)
            recreated = self.gateway("gateway-a")
            if recreated == first:
                raise AssertionError("recreated Gateway reused its predecessor's UID")
            replacement = self.instance("gateway-a", recreated, "route-a", "backend-a")
            if replacement["service_uid"] == a["service_uid"] or replacement["deployment_uid"] == a["deployment_uid"]:
                raise AssertionError("recreated Gateway reused resources owned by the deleted UID")
            current_b = self.instance("gateway-b", second, "route-b", "backend-b")
            if current_b["service_uid"] != b["service_uid"] or current_b["deployment_uid"] != b["deployment_uid"]:
                raise AssertionError("surviving Gateway resources were recreated")
            self.traffic("same-name-recreation", [replacement, current_b])
        route = self.get("httproutes", "route-a", namespace=self.test_namespace)
        patch = [{"op": "test", "path": "/metadata/uid", "value": route["metadata"]["uid"]},
                 {"op": "replace", "path": "/spec/rules/0/backendRefs/0/name", "value": "backend-b"}]
        self.kubectl("patch", "httproutes", "route-a", "--namespace", self.test_namespace, "--type=json", "--patch", json.dumps(patch))
        replacement = self.instance("gateway-a", recreated, "route-a", "backend-b")
        self.traffic("route-backend-update", [replacement, b])
        anchor = self.get("configmaps", self.anchor["name"], namespace=self.args.namespace)
        if anchor["metadata"]["uid"] != self.installation_uid or json.loads(anchor["data"]["template.json"]) != self.template:
            raise AssertionError("installation template changed during the lifecycle test")
        self.step("installation-template-unchanged", anchor=metadata(anchor))

    def diagnostics(self):
        try:
            snapshots = {uid: self.owned_resources(uid) for uid in self.gateways}
            self.save("remaining-owned-resources.json", snapshots)
            if self.namespace_uid:
                self.save("test-events.json", self.get("events", namespace=self.test_namespace))
                routes = self.get("gateways,httproutes", namespace=self.test_namespace)["items"]
                self.save("routing-status.json", [{"kind": r["kind"], "metadata": metadata(r), "status": r.get("status", {})} for r in routes])
        except Exception as error:
            self.event("diagnostics-error", error=str(error))

    def cleanup(self):
        for uid, name in self.gateways.items():
            try:
                live = self.get("gateways", name, namespace=self.test_namespace, optional=True)
                if live is not None and live["metadata"]["uid"] == uid:
                    self.delete_uid("gateways", name, uid, self.test_namespace)
            except Exception as error:
                self.summary["cleanup_errors"].append(str(error))
        for uid in self.gateways:
            try:
                self.wait(f"cleanup for Gateway {uid}", lambda uid=uid: self.collected(uid))
            except Exception as error:
                self.summary["cleanup_errors"].append(str(error))
        if self.namespace_uid:
            try:
                self.delete_uid("namespaces", self.test_namespace, self.namespace_uid)
                self.wait("temporary namespace deletion", lambda: self.get("namespaces", self.test_namespace, optional=True) is None)
            except Exception as error:
                self.summary["cleanup_errors"].append(str(error))
        self.event("cleanup-complete", errors=self.summary["cleanup_errors"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--context", required=True, help="explicit Kubernetes context")
    parser.add_argument("--namespace", required=True, help="existing installation namespace, never deleted")
    parser.add_argument("--gateway-class", required=True, help="existing GatewayClass for this installation")
    parser.add_argument("--output", required=True, help="new evidence directory; existing paths are rejected")
    parser.add_argument("--runner", help="existing namespace/pod with python3; otherwise create a temporary probe")
    parser.add_argument("--timeout", type=int, default=240, help="per-condition convergence timeout in seconds")
    args = parser.parse_args()
    if args.runner and (args.runner.count("/") != 1 or not all(args.runner.split("/"))):
        parser.error("--runner must be namespace/pod")
    if args.timeout < 1:
        parser.error("--timeout must be positive")
    run = Run(args)
    try:
        run.exercise()
        run.summary["result"] = "PASS"
    except BaseException as error:
        run.summary.update(result="FAIL", error=str(error), traceback=traceback.format_exc())
        print(f"FAIL: {error}", file=sys.stderr, flush=True)
    finally:
        run.diagnostics()
        run.cleanup()
        if run.summary["cleanup_errors"]:
            run.summary["result"] = "FAIL"
        run.summary["finished"] = now()
        run.save("summary.json", run.summary)
    print(json.dumps(run.summary, indent=2), flush=True)
    return 0 if run.summary["result"] == "PASS" else 1


if __name__ == "__main__":
    raise SystemExit(main())
