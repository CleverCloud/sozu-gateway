# Gateway API conformance runner

This runner calls the official Gateway API **v1.6.2** suite at
[`ca6c2a65454737236fb7a937bd9b17e42b07e9de`](https://github.com/kubernetes-sigs/gateway-api/tree/ca6c2a65454737236fb7a937bd9b17e42b07e9de/conformance).
Its fixtures and assertions are unchanged. The small Go wrapper sets Kubernetes client
QPS/Burst to **100/200**, validates exact upstream ShortNames and checks published addresses
from the runner Pod before starting the suite. Neither fixture addresses nor Gateway status
addresses are overridden. The image is built with Docker; no Go installation is needed locally
or in CI. `just conformance-image` builds it and tests the wrapper's selection validation.

## Run against an existing test cluster

Requirements: Python 3, kubectl, an explicit context, standard Gateway API v1.6.2 CRDs,
and a GatewayClass accepted by the controller. The controller's publish Service must have real
LoadBalancer addresses reachable from a Pod and expose HTTP port 80. Install the gateway with
`rbac.allowStatusWrites=true`; [values.yaml](values.yaml) lists the HTTP/TCP/UDP ports used by
the suite. Review those values before applying them to an existing deployment.

Use a dedicated test cluster. The official suite deletes entire fixture namespaces, including
its base namespaces. The script refuses any existing `gateway-conformance-*` namespace and
uses a separate `sozu-gateway-conformance` namespace as its runner and concurrency guard.
It checks again immediately before starting the suite. It does not alter an existing gateway
Deployment, Service, exposure configuration or CRDs.

```sh
python3 tests/conformance/run.py \
  --context my-test-cluster --gateway-class sozu \
  --gateway-service sozu-system/sozu-gateway \
  --runner-image my-registry/gateway-conformance:v1.6.2 \
  --version <controller-revision>-sozu-2.2.1 \
  --tests HTTPRouteSimpleSameNamespace,HTTPRouteExactPathMatching \
  --output results/focused-2026-09-10
```

By default the script builds and pushes the runner image. Use `--kind-name NAME` to load the
built image into Kind instead, or `--skip-build --runner-image registry/image@sha256:...` for
an image already built from this Dockerfile. The latter also works without local Docker.
The output directory must not exist; a second run cannot overwrite evidence from the first.

Omit `--tests` to run the complete HTTP/TCP/UDP campaign. The declared extensions are the same
three used by the recorded baseline: response header modification, scheme redirect and method
matching. A targeted run opts into every feature required by its requested tests so a valid name
cannot silently disappear behind feature selection. Unknown, misspelled, empty and duplicate
ShortNames are rejected. Targeted runs are regression checks, not full conformance reports.

## Results and cleanup

Each output directory contains:

- `suite.log`: unfiltered Go output, including final subtest verdicts;
- `report.yaml`: the unmodified upstream report, when the suite reached report generation;
- `metadata.json`: suite SHA, exact command, selected tests, timestamps, Kubernetes version,
  published Service addresses/ports, controller and runner image identities, exit and cleanup status;
- `catalog.json`: names and feature requirements compiled from the pinned upstream suite;
- `runner-resources.json` and `exit-code`: the runner/RBAC manifest and final process status.

A selected FAIL remains a failure. A selected SKIP, missing final verdict, missing report or
missing successful wrapper verdict cannot produce a successful exit. A full campaign currently
has known failures; this script does not waive them or convert an expected partial result into
CI success. Compare individual final Go verdicts with the
[recorded baseline](../../docs/conformance/gateway-http-tcp-udp_crd-v1.6.2_2026-09-10.md)
to distinguish regressions from existing limits. A PASS alone does not prove assertions ran:
upstream `UDPRouteNotAllowedByListeners` can return without them when TLSRoute is absent.

The suite's standard `namespace-annotations` option records an execution UUID without changing
namespace selector labels. Normal fixture cleanup remains upstream. After interruption the
script stops its runner, then deletes only fixture namespaces bearing that UUID; deletion uses
UID preconditions so a replacement object cannot be removed. It also deletes only the runner
namespace and cluster RBAC objects it created. Artifact collection does not request Secrets or kubeconfig credentials. Review `cleanup_errors` after an aborted or disconnected run; cleanup
requires API access and cannot be guaranteed after a hard kill or machine loss.

## CI and local Kind

The workflow runs these baseline checks on every PR and master push:
`HTTPRouteSimpleSameNamespace`, `HTTPRouteExactPathMatching`, `HTTPRouteCrossNamespace`,
`TCPRouteParentRefPortAndSectionName` and `UDPRoute`. Their final Go verdicts passed against
master 431017f in the recorded v1.6.2 campaign. This harness PR does not depend on the separate
controller fixes. Workflow dispatch accepts exact `run_test` names, or `mode=full` with no test
selection. Full mode retains its real failing exit until the implementation meets every selected
assertion. Artifacts are uploaded even on failure.

[Kind 0.32.0](https://github.com/kubernetes-sigs/kind/releases/tag/v0.32.0) runs
Kubernetes 1.36.1 using the release's pinned node-image digest. The workflow installs
[kubectl 1.36.1](https://kubernetes.io/docs/tasks/tools/install-kubectl-linux/) and
[Helm 3.21.4](https://github.com/helm/helm/releases/tag/v3.21.4).
[MetalLB 0.16.0](https://github.com/metallb/metallb/releases/tag/v0.16.0) supplies actual
LoadBalancer VIPs on the Kind Docker network; the runner reaches those published addresses
from a Pod. The [standard CRD bundle](https://github.com/kubernetes-sigs/gateway-api/releases/tag/v1.6.2)
is installed without alteration. Locally, after installing those tools:

```sh
bash tests/conformance/kind.sh conformance-local
python3 tests/conformance/run.py \
  --context kind-conformance-local --gateway-class sozu \
  --gateway-service sozu-system/sozu-gateway \
  --runner-image sozu-gateway-conformance:local --skip-build \
  --version <controller-revision>-sozu-2.2.1 \
  --tests HTTPRouteSimpleSameNamespace --output results/local
kind delete cluster --name conformance-local
```

`kind.sh` refuses an existing cluster name. It builds the repository's controller Dockerfile and
the runner Dockerfile, loads both images and installs the repository chart. The workflow deletes
only its uniquely named Kind cluster. The exposure model currently accepts one HTTP listener:
`TCPRouteInvalidNonTCPListener` requires an additional HTTP 5300 listener and therefore fails
that precondition with the stock chart. Full runs record that limitation rather than editing the
fixture or claiming that its TCP-to-HTTP rejection assertion passed.

`just conformance-test` tests ShortName validation, final verdict classification and namespace
ownership without Docker or a cluster. Docker builds additionally run the Go selection tests.
