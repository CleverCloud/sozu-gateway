#!/usr/bin/env python3
"""Check rendered Gateway instance resources with Helm's own YAML parser."""
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

CHART = Path(__file__).resolve().parents[1] / "charts/sozu-gateway"
TEMPLATE = r"""
{{- $objects := list -}}
{{- range $name := list "service" "deployment" "metrics-service" "pdb" "servicemonitor" -}}
  {{- $rendered := include "sozu-gateway.renderInstances" (dict "root" $ "template" (printf "sozu-gateway.%s" $name)) -}}
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


class GatewayInstances(unittest.TestCase):
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
        output = subprocess.check_output([
            "helm", "template", release, str(self.chart), "--namespace", "test",
            "--values", str(path), "--show-only", "templates/assertions.yaml",
        ], text=True)
        return json.loads(next(line for line in output.splitlines() if line.startswith("{")))["items"]

    def instance(self, name, **settings):
        return {"name": name, "gateway": {"namespace": "demo", "name": name}, **settings}

    def test_explicit_empty_annotations_do_not_inherit_and_siblings_are_unchanged(self):
        resources = self.render(service={"annotations": {"provider.example/key": "release"}},
            gatewayInstances=[self.instance("clear", service={"annotations": {}}), self.instance("inherit")])
        services = {r["metadata"]["name"]: r for r in resources if r["kind"] == "Service"}
        self.assertNotIn("annotations", services["sozu-clear"]["metadata"])
        for name in ["sozu", "sozu-inherit"]:
            self.assertEqual(services[name]["metadata"]["annotations"], {"provider.example/key": "release"})

    def test_a_supplied_annotation_map_replaces_the_release_map(self):
        resources = self.render(service={"annotations": {"parent": "release"}},
            gatewayInstances=[self.instance("child", service={"annotations": {"child": "instance"}})])
        child = next(r for r in resources if r["kind"] == "Service" and r["metadata"]["name"] == "sozu-child")
        self.assertEqual(child["metadata"]["annotations"], {"child": "instance"})

    def test_default_selector_remains_the_historical_pair(self):
        for instances in [[], [self.instance("b")]]:
            resources = self.render(gatewayInstances=instances)
            default = next(r for r in resources if r["kind"] == "Deployment" and r["metadata"]["name"] == "sozu")
            self.assertEqual(default["spec"]["selector"]["matchLabels"],
                {"app.kubernetes.io/name": "sozu-gateway", "app.kubernetes.io/instance": "sozu"})

    def test_all_instance_selectors_select_only_their_own_resources(self):
        resources = self.render(gatewayInstances=[self.instance("a"), self.instance("b")])
        deployments = {r["metadata"]["name"]: r for r in resources if r["kind"] == "Deployment"}
        for resource in resources:
            name = resource["metadata"]["name"]
            kind = resource["kind"]
            selector = resource["spec"]["selector"]
            if kind != "Service":
                selector = selector["matchLabels"]
            if kind == "ServiceMonitor":
                candidates = {r["metadata"]["name"]: r["metadata"]["labels"] for r in resources
                              if r["kind"] == "Service" and r["metadata"]["name"].endswith("-metrics")}
                expected = name + "-metrics"
            else:
                candidates = {n: d["spec"]["template"]["metadata"]["labels"] for n, d in deployments.items()}
                expected = name.removesuffix("-metrics")
            self.assertEqual([n for n, labels in candidates.items() if matches(selector, labels)], [expected], (kind, name))

    def test_release_instance_names_cannot_match_another_releases_default(self):
        first = self.render("sozu", gatewayInstances=[self.instance("b")])
        second = self.render("sozu-b", fullnameOverride="another")
        child = next(r for r in first if r["kind"] == "Deployment" and r["metadata"]["name"] == "sozu-b")
        other = next(r for r in second if r["kind"] == "Service" and r["metadata"]["name"] == "another")
        self.assertFalse(matches(other["spec"]["selector"], child["spec"]["template"]["metadata"]["labels"]))


    def test_instance_selectors_do_not_alias_across_release_name_boundaries(self):
        first = self.render("sozu", gatewayInstances=[self.instance("a-b")])
        second = self.render("sozu-a", fullnameOverride="another", gatewayInstances=[self.instance("b")])
        child = next(r for r in first if r["kind"] == "Deployment" and r["metadata"]["name"] == "sozu-a-b")
        other = next(r for r in second if r["kind"] == "Service" and r["metadata"]["name"] == "another-b")
        self.assertFalse(matches(other["spec"]["selector"], child["spec"]["template"]["metadata"]["labels"]))

    def test_long_valid_release_and_instance_names_keep_valid_label_values(self):
        resources = self.render("a" * 53, fullnameOverride="short", gatewayInstances=[self.instance("b" * 20)])
        for resource in resources:
            self.assertTrue(all(len(value) <= 63 for value in resource["metadata"]["labels"].values()))


if __name__ == "__main__":
    unittest.main()
