#!/usr/bin/env python3
"""Check automatic Gateway infrastructure with Helm's own YAML parser."""
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

CHART = Path(__file__).resolve().parents[1] / "charts/sozu-gateway"
TEMPLATE = r"""
{{- $objects := list -}}
{{- range $name := list "service" "deployment" "configmap" "metrics-service" "pdb" "servicemonitor" "rbac" "gateway-provisioner" "gateway-provisioner-rbac" -}}
  {{- $rendered := include (printf "sozu-gateway.%s" $name) $ -}}
  {{- range $document := splitList "\n---\n" $rendered -}}
    {{- if trim $document -}}
      {{- $object := fromYaml $document -}}
      {{- if hasKey $object "Error" -}}{{- fail (get $object "Error") -}}{{- end -}}
      {{- $objects = append $objects $object -}}
    {{- end -}}
  {{- end -}}
{{- end -}}
{{ toJson (dict "apiVersion" "v1" "kind" "List" "items" $objects) }}
"""


def matches(selector, labels):
    return all(labels.get(key) == value for key, value in selector.items())


def env(deployment, container="controller"):
    containers = deployment["spec"]["template"]["spec"]["containers"]
    selected = next(c for c in containers if c["name"] == container)
    return {e["name"]: e.get("value") for e in selected["env"]}


def select(resources, kind, name):
    return next(r for r in resources if r["kind"] == kind and r["metadata"]["name"] == name)


def config(resources, name="sozu-gateway-template"):
    return json.loads(select(resources, "ConfigMap", name)["data"]["template.json"])


class GatewayProvisioning(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory()
        cls.chart = Path(cls.temporary.name) / "chart"
        shutil.copytree(CHART, cls.chart)
        (cls.chart / "templates/assertions.yaml").write_text(TEMPLATE)

    @classmethod
    def tearDownClass(cls):
        cls.temporary.cleanup()

    def render(self, release="sozu", **values):
        values.setdefault("fullnameOverride", release)
        values.setdefault("metrics", {"enabled": True, "serviceMonitor": {"enabled": True}})
        path = self.chart / "test-values.json"
        path.write_text(json.dumps(values))
        output = subprocess.run([
            "helm", "template", release, str(self.chart), "--namespace", "test",
            "--values", str(path), "--show-only", "templates/assertions.yaml",
        ], text=True, capture_output=True, check=True).stdout
        return json.loads(next(line for line in output.splitlines() if line.startswith("{")))["items"]

    def test_disabled_preserves_default_routing_and_service_selectors(self):
        implicit = self.render()
        explicit = self.render(gatewayProvisioning={"enabled": False})
        self.assertEqual(implicit, explicit)
        default = select(explicit, "Deployment", "sozu")
        self.assertEqual(default["spec"]["selector"]["matchLabels"],
            {"app.kubernetes.io/name": "sozu-gateway", "app.kubernetes.io/instance": "sozu"})
        self.assertNotIn("SOZU_GW_GATEWAY_SCOPE", env(default))
        self.assertNotIn("SOZU_GW_INGRESS_ONLY", env(default))
        self.assertEqual(len([r for r in explicit if r["kind"] == "Deployment"]), 1)
        self.assertFalse(any(r["kind"] in ("Role", "RoleBinding") for r in explicit))

    def test_automatic_mode_needs_no_gateway_names_or_rendered_gateway_deployments(self):
        resources = self.render(gatewayProvisioning={"enabled": True})
        self.assertEqual({r["metadata"]["name"] for r in resources if r["kind"] == "Deployment"},
                         {"sozu", "sozu-provisioner"})
        self.assertEqual(env(select(resources, "Deployment", "sozu"))["SOZU_GW_INGRESS_ONLY"], "true")
        template = config(resources)
        self.assertEqual(template["namespace"], "test")
        self.assertEqual(template["template_config_map"], "sozu-gateway-template")
        self.assertEqual(set(template), {"namespace", "template_config_map", "deployment", "service", "config_map",
                                        "pod_disruption_budget", "metrics_service", "service_monitor"})
        self.assertNotIn("SOZU_GW_PROVISION_TEMPLATE", env(template["deployment"]))
        self.assertNotIn("SOZU_GW_INGRESS_ONLY", env(template["deployment"]))
        self.assertEqual(env(template["deployment"])["SOZU_GW_GATEWAY_SCOPE"], "template/template")
        self.assertEqual(env(template["deployment"])["SOZU_GW_PUBLISH_SERVICE"], "test/sozu-template")

    def test_default_gateway_and_provisioner_selectors_do_not_overlap(self):
        resources = self.render(gatewayProvisioning={"enabled": True})
        template = config(resources)
        default = select(resources, "Deployment", "sozu")
        manager = select(resources, "Deployment", "sozu-provisioner")
        labels = {"default": default["spec"]["template"]["metadata"]["labels"],
                  "gateway": template["deployment"]["spec"]["template"]["metadata"]["labels"],
                  "provisioner": manager["spec"]["template"]["metadata"]["labels"]}
        for key in ("service", "metrics_service"):
            self.assertEqual([n for n, value in labels.items() if matches(template[key]["spec"]["selector"], value)], ["gateway"])
        for key in ("deployment", "pod_disruption_budget"):
            self.assertEqual([n for n, value in labels.items() if matches(template[key]["spec"]["selector"]["matchLabels"], value)], ["gateway"])
        self.assertEqual([n for n, value in labels.items() if matches(select(resources, "Service", "sozu")["spec"]["selector"], value)], ["default"])
        monitor = template["service_monitor"]
        self.assertTrue(matches(monitor["spec"]["selector"]["matchLabels"], template["metrics_service"]["metadata"]["labels"]))
        self.assertFalse(matches(monitor["spec"]["selector"]["matchLabels"], select(resources, "Service", "sozu-metrics")["metadata"]["labels"]))

    def test_gateway_service_overrides_preserve_default_and_clear_annotations(self):
        resources = self.render(service={"annotations": {"provider.example/key": "release"}},
            gatewayProvisioning={"enabled": True, "service": {"type": "ClusterIP", "annotations": {}}})
        self.assertEqual(select(resources, "Service", "sozu")["metadata"]["annotations"], {"provider.example/key": "release"})
        self.assertEqual(select(resources, "Service", "sozu")["spec"]["type"], "LoadBalancer")
        child = config(resources)["service"]
        self.assertNotIn("annotations", child["metadata"])
        self.assertEqual(child["spec"]["type"], "ClusterIP")
        self.assertNotIn("externalTrafficPolicy", child["spec"])

    def test_gateway_annotation_map_replaces_parent_map(self):
        resources = self.render(service={"annotations": {"parent": "release"}},
            gatewayProvisioning={"enabled": True, "service": {"annotations": {"child": "gateway"}}})
        self.assertEqual(config(resources)["service"]["metadata"]["annotations"], {"child": "gateway"})

    def test_generated_resources_inherit_release_configuration(self):
        resources = self.render(gatewayProvisioning={"enabled": True}, replicaCount=3,
            image={"controller": {"repository": "registry.example/controller", "digest": "sha256:" + "a" * 64}},
            resources={"sozu": {"requests": {"cpu": "350m"}}, "controller": {"limits": {"memory": "256Mi"}}},
            sozu={"workerCount": 4, "timeouts": {"connect": 8}, "drain": {"delaySeconds": 8, "gracePeriodSeconds": 45}},
            nodeSelector={"pool": "gateways"}, tolerations=[{"key": "gateway", "operator": "Exists"}])
        template = config(resources)
        parent = select(resources, "Deployment", "sozu")
        worker = template["deployment"]
        self.assertEqual(worker["spec"]["replicas"], 3)
        self.assertEqual(template["config_map"]["data"], select(resources, "ConfigMap", "sozu-sozu")["data"])
        parent_spec = parent["spec"]["template"]["spec"]
        worker_spec = worker["spec"]["template"]["spec"]
        for field in ["securityContext", "nodeSelector", "tolerations", "terminationGracePeriodSeconds", "serviceAccountName"]:
            self.assertEqual(worker_spec[field], parent_spec[field], field)
        self.assertEqual(worker_spec["containers"][0], parent_spec["containers"][0])
        self.assertEqual(worker_spec["containers"][1]["image"], "registry.example/controller@sha256:" + "a" * 64)
        self.assertEqual(template["service"]["spec"]["ports"], select(resources, "Service", "sozu")["spec"]["ports"])
        for constraint in worker_spec["topologySpreadConstraints"]:
            self.assertEqual(constraint["labelSelector"]["matchLabels"], worker["spec"]["selector"]["matchLabels"])

    def test_single_replica_gateway_omits_only_its_own_budget(self):
        resources = self.render(replicaCount=2, gatewayProvisioning={"enabled": True, "replicaCount": 1})
        self.assertNotIn("pod_disruption_budget", config(resources))
        self.assertEqual(config(resources)["deployment"]["spec"]["replicas"], 1)
        self.assertIsNotNone(select(resources, "PodDisruptionBudget", "sozu"))

    def test_metrics_switches_control_generated_scrape_resources(self):
        for metrics, expected in [({"enabled": False}, set()),
                                  ({"enabled": True, "serviceMonitor": {"enabled": False}}, {"metrics_service"}),
                                  ({"enabled": True, "serviceMonitor": {"enabled": True}}, {"metrics_service", "service_monitor"})]:
            resources = self.render(gatewayProvisioning={"enabled": True}, metrics=metrics)
            self.assertEqual(set(config(resources)) & {"metrics_service", "service_monitor"}, expected)

    def test_only_provisioner_has_namespaced_workload_permissions(self):
        resources = self.render(gatewayProvisioning={"enabled": True})
        roles = [r for r in resources if r["kind"] == "Role"]
        self.assertEqual(len(roles), 1)
        self.assertEqual(roles[0]["metadata"]["namespace"], "test")
        binding = select(resources, "RoleBinding", "sozu-provisioner")
        self.assertEqual(binding["subjects"], [{"kind": "ServiceAccount", "name": "sozu-provisioner", "namespace": "test"}])
        manager_role = select(resources, "ClusterRole", "sozu-provisioner")
        self.assertEqual(manager_role["rules"], [{"apiGroups": ["gateway.networking.k8s.io"],
            "resources": ["gatewayclasses", "gateways"], "verbs": ["get", "list", "watch"]}])
        for role in [r for r in resources if r["kind"] == "ClusterRole"]:
            for rule in role["rules"]:
                self.assertFalse(set(rule["resources"]) & {"deployments", "configmaps", "poddisruptionbudgets", "servicemonitors"})
        manager = select(resources, "Deployment", "sozu-provisioner")
        self.assertEqual(manager["spec"]["strategy"]["type"], "Recreate")
        self.assertEqual(manager["spec"]["replicas"], 1)
        manager_spec = manager["spec"]["template"]["spec"]
        self.assertEqual(manager_spec["serviceAccountName"], "sozu-provisioner")
        self.assertEqual([c["name"] for c in manager_spec["containers"]], ["provisioner"])
        self.assertEqual(config(resources)["deployment"]["spec"]["template"]["spec"]["serviceAccountName"], "sozu")
        self.assertFalse(manager_spec["automountServiceAccountToken"])
        self.assertNotIn("SOZU_GW_SOCKET", env(manager, "provisioner"))
        self.assertEqual(env(manager, "provisioner")["SOZU_GW_RESYNC_SECS"], "60")
        self.assertEqual(env(manager, "provisioner")["SOZU_GW_HEALTH_LISTEN"], "0.0.0.0:8081")
        self.assertEqual(manager_spec["containers"][0]["readinessProbe"]["httpGet"], {"path": "/readyz", "port": "health"})
        self.assertEqual(manager_spec["containers"][0]["livenessProbe"]["httpGet"], {"path": "/healthz", "port": "health"})

    def test_disabled_monitors_keep_cleanup_but_not_creation_permissions(self):
        resources = self.render(gatewayProvisioning={"enabled": True}, metrics={"enabled": False})
        rules = select(resources, "Role", "sozu-provisioner")["rules"]
        self.assertEqual([r for r in rules if "servicemonitors" in r["resources"]],
            [{"apiGroups": ["monitoring.coreos.com"], "resources": ["servicemonitors"], "verbs": ["get", "list", "delete"]}])

    def test_template_changes_restart_the_provisioner(self):
        first = self.render(gatewayProvisioning={"enabled": True})
        changed = self.render(gatewayProvisioning={"enabled": True, "service": {"type": "ClusterIP"}})
        def checksum(resources):
            return select(resources, "Deployment", "sozu-provisioner")["spec"]["template"]["metadata"]["annotations"]["checksum/template"]
        self.assertNotEqual(checksum(first), checksum(changed))

    def test_old_static_entries_and_invalid_options_fail_clearly(self):
        cases = [({"gatewayInstances": [{"name": "legacy"}]}, "gatewayInstances has been replaced"),
                 ({"gatewayProvisioning": {"enabled": "true"}}, "must be a boolean"),
                 ({"gatewayProvisioning": {"enabled": True, "replicaCount": 0}}, "positive integer"),
                 ({"gatewayProvisioning": {"enabled": True, "service": []}}, "must be a mapping"),
                 ({"gatewayProvisioning": {"enabled": True}, "fullnameOverride": "a" * 47}, "shorter than 47")]
        for values, message in cases:
            with self.subTest(values=values):
                with self.assertRaises(subprocess.CalledProcessError) as error:
                    self.render(**values)
                self.assertIn(message, error.exception.stderr)

    def test_long_release_names_keep_selectors_and_labels_valid(self):
        resources = self.render("a" * 53, fullnameOverride="short", gatewayProvisioning={"enabled": True})
        template = config(resources, "short-gateway-template")
        for resource in resources + [v for v in template.values() if isinstance(v, dict)]:
            self.assertTrue(all(len(value) <= 63 for value in resource["metadata"].get("labels", {}).values()))
            if resource["kind"] == "Deployment":
                self.assertTrue(all(len(value) <= 63 for value in resource["spec"]["selector"]["matchLabels"].values()))


if __name__ == "__main__":
    unittest.main()
