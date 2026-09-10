# sozu-gateway — developer + release tasks.
#
# Container image + chart artifacts are published to ghcr.io under the
# CleverCloud org (see .github/workflows/release.yml). Override variables on the
# command line, e.g. `just IMAGE=my/repo TAG=v0.2.0 image`.

IMAGE := "ghcr.io/clevercloud/sozu-gateway-controller"
TAG := "dev"
# Helm chart SemVer derived from TAG (v0.2.0 -> 0.2.0; dev -> dev).
CHART_VERSION := trim_start_match(TAG, "v")
CHART := "charts/sozu-gateway"
HELM_RELEASE := "sozu-gateway"
HELM_NS := "sozu-system"

# List the available recipes.
default:
    @just --list

# Build + test.
all: build test

# Build the whole workspace.
build:
    cargo build --workspace

# Unit + golden/snapshot tests.
test:
    cargo test --workspace

# CI gate: fmt check + clippy -D warnings.
lint: fmt-check clippy

# Format the workspace (write).
fmt:
    cargo fmt

# Check formatting without writing.
fmt-check:
    cargo fmt --check

# Clippy with warnings denied.
clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# Build the controller container image ({{IMAGE}}:{{TAG}}).
image:
    docker build -t {{IMAGE}}:{{TAG}} .

# Lint + render the Helm chart (also with rbac.allowStatusWrites=true, both
# sides of the metrics switch + the ServiceMonitor path, and a digest-pinned
# image).
chart-lint:
    helm lint {{CHART}}
    helm template {{HELM_RELEASE}} {{CHART}} > /dev/null
    helm template {{HELM_RELEASE}} {{CHART}} --set rbac.allowStatusWrites=true > /dev/null
    helm template {{HELM_RELEASE}} {{CHART}} --set replicaCount=1 > /dev/null
    # Timeouts: non-default values, and the HSTS sub-table they share a file with.
    helm template {{HELM_RELEASE}} {{CHART}} --set sozu.timeouts.front=45 --set sozu.timeouts.request=8 > /dev/null
    helm template {{HELM_RELEASE}} {{CHART}} --set sozu.hardening.hsts.enabled=true > /dev/null
    helm template {{HELM_RELEASE}} {{CHART}} --set sozu.timeouts=null > /dev/null
    # A timeout Sōzu could not load must fail the render, not the proxy's boot.
    ! helm template {{HELM_RELEASE}} {{CHART}} --set sozu.timeouts.conect=1 > /dev/null 2>&1
    ! helm template {{HELM_RELEASE}} {{CHART}} --set sozu.timeouts.front=true > /dev/null 2>&1
    ! helm template {{HELM_RELEASE}} {{CHART}} --set sozu.timeouts.connect=5000000000 > /dev/null 2>&1
    ! helm template {{HELM_RELEASE}} {{CHART}} --set sozu.timeouts=5 > /dev/null 2>&1
    helm template {{HELM_RELEASE}} {{CHART}} --set metrics.serviceMonitor.enabled=true > /dev/null
    # Both sides of the metrics switch, asserted rather than merely rendered.
    helm template {{HELM_RELEASE}} {{CHART}} | grep -q SOZU_GW_METRICS_LISTEN
    ! helm template {{HELM_RELEASE}} {{CHART}} --set metrics.enabled=false | grep -q SOZU_GW_METRICS_LISTEN
    # The Pod's own ports are not up for grabs by an exposure entry.
    ! helm template {{HELM_RELEASE}} {{CHART}} --set-json 'exposure=[{"name":"http","port":80,"bind":8080,"protocol":"HTTP","transport":"TCP"},{"name":"https","port":443,"bind":8443,"protocol":"HTTPS","transport":"TCP"},{"name":"pg","port":9100,"bind":9100,"protocol":"TCP","transport":"TCP"}]' > /dev/null 2>&1
    ! helm template {{HELM_RELEASE}} {{CHART}} --set-json 'exposure=[{"name":"metrics","port":9999,"bind":9999,"protocol":"TCP","transport":"TCP"},{"name":"http","port":80,"bind":8080,"protocol":"HTTP","transport":"TCP"},{"name":"https","port":443,"bind":8443,"protocol":"HTTPS","transport":"TCP"}]' > /dev/null 2>&1
    helm template {{HELM_RELEASE}} {{CHART}} --set replicaCount=2 > /dev/null
    helm template {{HELM_RELEASE}} {{CHART}} --set replicaCount=3 > /dev/null
    helm template {{HELM_RELEASE}} {{CHART}} --set rbac.allowGatewayStatusWrites=false > /dev/null
    helm template {{HELM_RELEASE}} {{CHART}} --set-json 'exposure=[{"name":"http","port":80,"bind":8080,"protocol":"HTTP","transport":"TCP"},{"name":"https","port":443,"bind":8443,"protocol":"HTTPS","transport":"TCP"},{"name":"pg","port":5432,"bind":5432,"protocol":"TCP","transport":"TCP"},{"name":"dns","port":5353,"bind":5353,"protocol":"UDP","transport":"UDP"}]' > /dev/null
    # Explicit Gateway instances render their own data plane and reject
    # ambiguous ownership or names that would collide after truncation.
    helm template {{HELM_RELEASE}} {{CHART}} --set-json 'gatewayInstances=[{"name":"public","gateway":{"namespace":"apps","name":"public"}},{"name":"internal","gateway":{"namespace":"apps","name":"internal"},"replicaCount":1,"service":{"type":"ClusterIP"}}]' --set metrics.serviceMonitor.enabled=true > /dev/null
    ! helm template {{HELM_RELEASE}} {{CHART}} --set-json 'gatewayInstances=[{"name":"a","gateway":{"namespace":"apps","name":"one"}},{"name":"a","gateway":{"namespace":"apps","name":"two"}}]' > /dev/null 2>&1
    ! helm template {{HELM_RELEASE}} {{CHART}} --set-json 'gatewayInstances=[{"name":"a","gateway":{"namespace":"apps","name":"one"}},{"name":"b","gateway":{"namespace":"apps","name":"one"}}]' > /dev/null 2>&1
    ! helm template {{HELM_RELEASE}} {{CHART}} --set-json 'gatewayInstances=[{"name":"Bad_Name","gateway":{"namespace":"apps","name":"one"}}]' > /dev/null 2>&1
    ! helm template {{HELM_RELEASE}} {{CHART}} --set-json 'gatewayInstances=[{"name":"a","gateway":{"namespace":"bad/ns","name":"one"}}]' > /dev/null 2>&1
    ! helm template {{HELM_RELEASE}} {{CHART}} --set-json 'gatewayInstances=[{"name":"a","gateway":{"namespace":"apps","name":"one"}}]' --set fullnameOverride=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa > /dev/null 2>&1
    ! helm template {{HELM_RELEASE}} {{CHART}} --set-json 'gatewayInstances=[{"name":"metrics","gateway":{"namespace":"apps","name":"one"}}]' > /dev/null 2>&1
    ! helm template {{HELM_RELEASE}} {{CHART}} --set-json 'gatewayInstances=[{"name":"a","gateway":{"namespace":"apps","name":"one"}},{"name":"a-metrics","gateway":{"namespace":"apps","name":"two"}}]' > /dev/null 2>&1
    # A budget with neither bound is accepted by the apiserver and then blocks
    # every drain, so it must fail the render instead.
    ! helm template {{HELM_RELEASE}} {{CHART}} --set pdb.maxUnavailable=null > /dev/null 2>&1
    # A drain that cannot fit inside the grace period, or that is not a whole
    # number of seconds, must fail the render rather than the Pod's shutdown.
    helm template {{HELM_RELEASE}} {{CHART}} --set sozu.drain.enabled=false > /dev/null
    ! helm template {{HELM_RELEASE}} {{CHART}} --set sozu.drain.delaySeconds=40 > /dev/null 2>&1
    ! helm template {{HELM_RELEASE}} {{CHART}} --set sozu.drain.delaySeconds=-1 > /dev/null 2>&1
    ! helm template {{HELM_RELEASE}} {{CHART}} --set sozu.drain.gracePeriodSeconds=30s > /dev/null 2>&1
    # An exposure table that cannot work must fail the render, not the apiserver.
    ! helm template {{HELM_RELEASE}} {{CHART}} --set-json 'exposure=[{"name":"https","port":443,"bind":8443,"protocol":"HTTPS","transport":"TCP"},{"name":"pass","port":443,"bind":9443,"protocol":"TCP","transport":"TCP"}]' > /dev/null 2>&1
    ! helm template {{HELM_RELEASE}} {{CHART}} --set-json 'exposure=[{"name":"http","port":80,"bind":8080,"protocol":"HTTP","transport":"TCP"},{"name":"pg","port":5432,"bind":8080,"protocol":"TCP","transport":"TCP"}]' > /dev/null 2>&1
    ! helm template {{HELM_RELEASE}} {{CHART}} --set-json 'exposure=[{"name":"http","port":80,"bind":80,"protocol":"HTTP","transport":"TCP"}]' > /dev/null 2>&1
    ! helm template {{HELM_RELEASE}} {{CHART}} --set-json 'exposure=[{"name":"http","port":80,"bind":8080,"protocol":"HTTP","transport":"TCP"},{"name":"https","port":443,"bind":8443,"protocol":"HTTPS","transport":"TCP"},{"name":"https2","port":9443,"bind":9444,"protocol":"HTTPS","transport":"TCP"}]' > /dev/null 2>&1
    helm template {{HELM_RELEASE}} {{CHART}} --set image.controller.digest=sha256:0000000000000000000000000000000000000000000000000000000000000000 > /dev/null

    python3 scripts/test_gateway_instances.py

# Package the Helm chart into dist/ (use TAG=v<semver>).
chart-package:
    mkdir -p dist
    helm package {{CHART}} --version {{CHART_VERSION}} --app-version {{TAG}} --destination dist

# Full in-cluster end-to-end on the current kube-context (build+push image,
# install the add-on, deploy the demo app, verify HTTP/HTTPS through Sōzu).
# Defaults to an ephemeral ttl.sh image so no registry credentials are needed.
e2e:
    bash scripts/e2e.sh

# Gateway API + HTTPRoute filters (header / rewrite / redirect) end-to-end.
e2e-gateway:
    bash scripts/e2e-gateway.sh

# Layer-4 routing through the Gateway API (TCPRoute + UDPRoute) end-to-end.
e2e-l4-routes:
    bash scripts/e2e-l4-routes.sh

# Run every e2e suite, sharing one freshly-built, digest-pinned image.
e2e-all:
    #!/usr/bin/env bash
    set -euo pipefail
    source scripts/e2e-lib.sh
    ensure_image
    bash scripts/e2e.sh
    bash scripts/e2e-gateway.sh
    bash scripts/e2e-l4-routes.sh

# Tear down e2e resources + cargo clean.
clean:
    -helm uninstall {{HELM_RELEASE}} -n {{HELM_NS}}
    -kubectl delete -f examples/ingress/demo-app.yaml
    rm -rf dist
    cargo clean
