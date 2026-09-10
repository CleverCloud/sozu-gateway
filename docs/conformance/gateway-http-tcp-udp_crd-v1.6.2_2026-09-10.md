# Gateway API conformance, 2026-09-10

The official Gateway API **v1.6.2** suite was run against sozu-gateway
**431017f81045d5ca04be289a1016dcfaedf1c1b2** and **Sōzu 2.2.1**, using the
HTTP, TCP and UDP profiles. The results below describe this implementation
and deployment; they are not a certification or a controlled comparison of
Sōzu releases.

| Profile | Core passed / total | Core failed | Extended passed / total | Selected skips |
| --- | --- | --- | --- | --- |
| GATEWAY-HTTP | **18 / 37** | 19 | 1 / 3 | 0 |
| GATEWAY-TCP | **14 / 19** | 5 | not selected | 0 |
| GATEWAY-UDP | **16 / 20** | 4 | not selected | 0 |

**None of the three profiles passes.** Across their union, **31 of 57 distinct tests pass**, 26 fail and 0 are skipped.
The process completed normally with exit code 1 after 46.4 minutes
(2026-09-10T10:27:24.499980+00:00 to 2026-09-10T11:13:47.630248+00:00); the failures are test verdicts,
not an interrupted run. All 57 selected final Go verdicts agree with the
[unaltered upstream YAML](gateway-http-tcp-udp_crd-v1.6.2_2026-09-10.yaml). Its SHA-256 is
`d9d3a91f865425194ea5a087c4abc9c1371d087327f7c654fcd934447643f07f`.

The profiles share 11 Gateway tests. Their denominators must not be added
to obtain the number of distinct tests. The suite registered 153 tests:
57 were selected, while 96 required features outside this run's selection.
The three HTTP extensions were explicitly selected to measure partial
behavior; `GatewayClass.status.supportedFeatures` remains empty.

All TCPRoute- and UDPRoute-specific tests except
`TCPRouteMultipleRoutesAttachment` are marked **Provisional** upstream in
v1.6.2. They still contribute to the official core statistics; the 12
successful route-specific tests are listed in `succeededProvisionalTests`.
That upstream maturity label is separate from whether this run has finished.

One reported UDP pass needs a qualification: `UDPRouteNotAllowedByListeners`
returns from its body without behavioral assertions when TLSRoute support
is absent, as it is here. Its official result is preserved without treating
it as evidence that listener rejection was tested.

## Environment and scope

| Item | Value |
| --- | --- |
| Kubernetes | v1.36.3, three amd64 nodes, two gateway replicas |
| Cluster | `sozu-gateway-upgrade` |
| Suite and standard CRDs | v1.6.2, upstream revision `ca6c2a65454737236fb7a937bd9b17e42b07e9de` |
| Generated controller API types | v1.6.1 |
| GatewayClass | `sozu`, controller `sozu.io/gateway-controller` |
| HTTP extensions | Response header modification, scheme redirect, method matching |
| Kubernetes client limits | QPS 100, burst 200 |
| Scheduling | `--disable-parallel-tests=true`; upstream nested parallel subtests unchanged |
| Timeouts | Upstream defaults; outer Go timeout 150 minutes |
| Runner | Pod inside the cluster, targeting the advertised Gateway VIPs |

The suite's tests, fixtures and assertions were unmodified. A small wrapper
calls `conformance.DefaultOptions`, rebuilds its clients with the limits above,
and delegates to `conformance.RunConformanceWithOptions`. No test was manually
skipped, no shared Gateway was excluded from readiness, and no implementation
change was made during execution. Stock upstream annotations on negative-test
fixtures were retained.

The HTTP/Gateway test files, base manifests, feature definitions and profile
membership are identical between v1.6.1 and v1.6.2. Standard CRD schemas are
unchanged apart from descriptions and release metadata. The stable release
was chosen for a reproducible reference rather than the moving upstream main
branch. See the [v1.6.2 release](https://github.com/kubernetes-sigs/gateway-api/releases/tag/v1.6.2).

The Service exposed HTTP 80 → 8080, HTTPS 443 → 8443, TCP ports
9300–9302, 9310–9313 and 9320–9321, and UDP ports 5300–5302 and 5310–5312.
Gateway addresses were the Service's advertised VIPs, `82.47.250.4` and
`82.47.250.5`; they were not rewritten to Pod addresses. Requests originated
inside the cluster, so this does not validate the complete path from an
external client through the provider's load balancer.

Docker was unavailable. The release controller was rebuilt with `--locked`
from the revision above and packaged on Ubuntu 24.04 with public CA
certificates and uid/gid 1000. The live run therefore does not validate the
shipped Debian bookworm Dockerfile. The Sōzu image was unmodified. Image
digests and the controller binary hash were checked on both running Pods:

| Artifact | SHA-256 |
| --- | --- |
| Controller image | `060e0832b9e184689a200d8c2ab8baaf9ce39fa6592a87ee787904054327c98e` |
| Controller binary | `0bcdea52d44fc815ac200c5ac3be24d2851facf5b9e8fa3eb5ad7bef4f55ad21` |
| Sōzu runtime image | `7ba9ef2f63aa53766ed5d66f7f756bedf1bf39232a2024368b8081f9c933a096` |
| Runner image | `6f6a4db3c8955f31fb13958de55b46ca80daa4f372b5ee98934522ffd363a5a7` |
| Runner binary | `dffc00496b202d9ab25286a7256976b4c75ee0a5f8e9469c5df3197eb12ebbee` |

## What the failures establish

### Gateway status

`GatewayListenerUnsupportedProtocol` fails both top-level Accepted-condition
expectations: all-invalid listeners should make the Gateway unaccepted, and
a mixed valid/invalid set needs reason `ListenersNotValid`. The controller
instead publishes `True/Accepted` in both cases.
`GatewayInvalidParametersRef` likewise gets `True/Accepted` instead of
`False/InvalidParameters` for a nonexistent infrastructure reference.
The builder's unconditional Gateway acceptance explains these current
status gaps. Those code paths predate the Sōzu 2.2.1 upgrade.

The terminal rate-limiter deadline messages follow minutes of successful
reads of persistently wrong conditions. They do not establish that Kubernetes
throttling caused these failures.

### HTTP matching and TLS

Header predicates are explicitly rejected by the builder. All eight positive
`HTTPRouteHeaderMatching` cases return 404 instead of 200; three negative
cases succeed. Header predicates also account for the failures in the
method-matching extension: nine of its twelve subcases succeed, and the
three failures all require headers. This is not evidence that method matching
is wholly unsupported. `HTTPRouteMatchingAcrossRoutes` passes seven of
eight traffic cases; its only failing case also requires a header predicate.

Hostname intersection passes 31 of 33 traffic cases. The two failures require
a wildcard to match multiple labels. One is excluded by the controller's
one-label intersection helper; the other preserves the wildcard but encounters
the data plane's one-label tree lookup. Two of eight listener-hostname cases
fail for the same wildcard depth. These cases do not require a shared-address
collision to explain their failures.

For `HTTPRouteMatching`, three requests under `/v2` select backend v1 instead
of the more specific v2 rule; a fourth failure requires a header predicate.
The hostname-less frontend is placed in Sōzu's POST list, whose first-match
ordering makes command insertion order relevant. The compiler's ordering
does not preserve path specificity there. This is a source-backed explanation;
the exact installed command-state order was not captured during this test.

`HTTPRouteHTTPSListener` serves a certificate without DNS names for two
hosts, while the explicit
`second-example.org` listener successfully completes TLS and reaches its
backend. The failing hosts therefore receive a different certificate from
the shared Secret that works for the named listener. That Secret appears
on one hostname-less listener and three
named listeners. Merging their certificate names turns an empty list, which
requests CN/SAN inference, into a nonempty override that omits the two failing
names. This identifies a concrete compiler problem. The presented certificate
was not captured, so selection of Sōzu's default certificate remains an
inference. The result does not establish generic lack of HTTPS support.

`HTTPRoutePathMatchOrder` passes five of six traffic cases; the longer
`/match/prefix/one` rule loses to the shorter v1 prefix. This independently
agrees with the POST ordering explanation above.

`HTTPRouteMultipleGateways` is an actual shared-listener collision: two
hostname-less `/` routes need different backends at the same address and
port. The controller reports `RouteCollision` for one. Two requests through
the accepted Gateway pass; the other branch fails its Accepted prerequisite
and sends neither of its traffic requests.

Header modification passes four of seven request cases and four of eight
response cases. Every failure encounters an existing header value preserved
by `set` instead of replaced. Sōzu 2.2.1 explicitly converts the protobuf
Header to `HeaderEditMode::Append`; both the builder's `set` and `add` use
that form. Adding to an existing header passes. The older description that
Sōzu has no append and treats `add` as `set` is contradicted by this source
and runtime evidence. Failures in mixed/case-insensitive cases establish
the replacement problem, not an independent defect in every other operation.

`HTTPRouteWeight` rejects multiple backendRefs and fails its
`ResolvedRefs=True` prerequisite. It sends no HTTP request and reaches
neither distribution nor zero-weight assertions.

### HTTP backend references

Four wholly invalid-reference tests publish the expected `ResolvedRefs=False`
reason but serve **404 instead of the required 500**. The invalid rule is
omitted from the forwarding graph. `HTTPRoutePartiallyInvalidViaInvalidReferenceGrant`
instead serves **200** on the invalid `/v2` path while its valid sibling passes.
The code permits the remaining valid root-prefix route to match this request;
the backend identity for `/v2` was not captured, so this does not demonstrate
access to the forbidden backend.

`HTTPRouteNoBackendRefs` fails its status prerequisite
(`False/BackendNotFound` instead of `ResolvedRefs=True`); no traffic assertion
executes. `HTTPRouteReferenceGrant` forwards before deletion, then observes
one initial 200 followed by 29 responses of 404 where the test requires 500.
It observes a change after revocation and fails on the required HTTP result.
All seven backend-reference tests were already reported failed in August.

### TCP

Invalid backend references pass their status assertions. ParentRef selection
passes across four attach-all listeners and three port/sectionName variants,
including verified responses from the expected backends. ReferenceGrant passes
forwarding while granted and the denied-reference status after deletion;
upstream sends no traffic after that deletion.

`TCPRouteMultipleRoutesAttachment` fails because the newer route is
`Accepted=False/RouteConflict` and the listener counts one attached route;
upstream requires both routes Accepted and an attached count of two. Its
separate traffic assertion succeeds, including 100 connections to the older
route's backend.

`TCPRouteInvalidNonTCPListener` cannot pass its 300-second setup gate: its
fixture needs **HTTP on port 5300**, while the chart supports exactly one
static HTTP listener, already on port 80. Exposing UDP 5300 does not satisfy
that requirement. The snapshot shows the TCPRoute rejected with
`NotAllowedByListeners`, but the official behavior assertions never execute.
This is an exposure-model limitation, not a measured failure of that rejection.

Weighted TCP configuration is explicitly rejected. The route reports
`ResolvedRefs=False/BackendNotFound` with a weighted-split-unsupported message;
the correctly exposed port refuses connections. The fatal availability probe
prevents the handshake, weighted sampler and zero-weight assertions from
running. No 70/30 distribution was measured.

### UDP

Invalid backend references and ParentRef selection pass their status checks;
the latter also verifies the expected backend on all six tested listener
ports. ReferenceGrant verifies forwarding before deletion and denial status
afterward, with no post-deletion traffic assertion.

`UDPRouteMultipleRoutesAttachment` has the same two status mismatches as TCP,
plus a distinct traffic failure. Four replies from the expected older backend
are inferable from the log, but the test never obtains its required 20
consecutive replies because reads time out. Both backends are Ready, the winner is Accepted, the
port is exposed, and no controller configuration change is recorded during
the losses. A following ParentRef check succeeds on that port but requires
only one reply. The cause of this UDP reliability failure is **not established**.

Weighted UDP configuration is also explicitly rejected. Its initial echo
probe fails nonfatally, so the distribution function still executes all ten
permitted batches. Each batch returns a request-sending error before the
proportion comparisons. This establishes failure to collect a usable sample,
not an observed inaccurate 70/30 split or traffic to a zero-weight backend.

## Comparison with the August HTTP report

The immutable [2026-08-11 report](gateway-http_crd-v1.6.1_2026-08-11.yaml)
records 20/37 core and 1/3 extended. Its v1.6.1 reporter stored the immediate
boolean returned by `t.Run`; for a parallel test that can precede every
assertion. The v1.6.2 reporter records the final outcome in a cleanup after
all descendants finish. Ten historical HTTP passes are exposed to this defect.
This does not prove that all ten, or any particular historical pass, were false:
the old execution log and exact runner modifications are unavailable.

The only changed HTTP verdicts are `GatewayInvalidParametersRef`, `GatewayListenerUnsupportedProtocol`,
both formerly reported PASS and now FAIL. Both used the susceptible
parallel wrapper. The remaining 38 HTTP verdicts are unchanged. Under the
documented historical runner, those parallel results were recorded before
assertions; the historical YAML cannot establish what those assertions did.

The current test and controller evidence supports preexisting Gateway-status
gaps. The campaigns also differ in cluster, controller revision, data-plane
version and scheduling. Their score difference cannot isolate an effect of
the Sōzu upgrade. The historical broad attribution of hostname, matching,
method and TLS failures to one shared load balancer is not supported by this
run's individual assertions; the distinctions above supersede it for this run.

## Reproduction and retained evidence

Use the pinned v1.6.2 source, the deployed ports listed above, an Accepted
`sozu` GatewayClass and `rbac.allowStatusWrites=true`. The command executed
inside the runner Pod was:

```sh
/usr/local/bin/gateway-api.test -test.v \
  -test.run '^TestGatewayConformance$' -test.timeout 150m \
  --gateway-class=sozu \
  --conformance-profiles=GATEWAY-HTTP,GATEWAY-TCP,GATEWAY-UDP \
  --supported-features=HTTPRouteResponseHeaderModification,HTTPRouteSchemeRedirect,HTTPRouteMethodMatching \
  --kube-api-qps=100 --kube-api-burst=200 \
  --disable-parallel-tests=true \
  --organization=clevercloud --project=sozu-gateway \
  --url=https://github.com/CleverCloud/sozu-gateway \
  --contact=https://github.com/CleverCloud/sozu-gateway/issues \
  --version=431017f81045d5ca04be289a1016dcfaedf1c1b2-sozu-2.2.1 \
  --report-output=/results/gateway-http-tcp-udp.yaml
```

The QPS/burst flags belong to the local wrapper, not upstream. Local evidence
is retained under `.scratch/conformance-2026-09-10/`: the wrapper source,
runner resources, build/packaging scripts, image metadata, exact run command
and timestamps, full Go log, controller logs, 30-second object snapshots,
per-failure analyses with source/log references, and the checked result summary.
These local files are gitignored; a fresh checkout does not contain them.
The published selection of baseline evidence is preserved in the
[campaign archive](gateway-http-tcp-udp_crd-v1.6.2_2026-09-10_after-fixes.md#evidence-archive),
including the complete Go log, unchanged YAML and execution metadata.
The controller and runner images use temporary 24-hour registry tags, so
reproduction requires rebuilding them after expiry.

The existing three nodes were sufficient; no node was added. At the end
of the run both gateway Pods were 2/2 Ready, all four containers had zero
restarts, and both controllers reported zero reconcile failures and zero
shadow resets. Sampled node usage was about 1% CPU and 7% memory. These
health signals do not imply protocol conformance.

Suite fixtures and runner permissions were removed. Original exposure
settings and the saved demo Gateway/HTTPRoutes were restored; the header
route returned 200 with the expected headers, and the redirect returned 301
with its expected Location. The rebuilt controller and Sōzu 2.2.1 remain
deployed, with CRDs v1.6.2 retained. Both restored gateway Pods are 2/2 Ready.


### Runner wrapper

In a fresh clone at the pinned upstream revision, save this as
`conformance/localrunner/runner_test.go`, then build from `conformance/`
with `CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go test -c -o gateway-api.test ./localrunner`.
The test fixtures are embedded by upstream. Run the binary with the command
above in a Pod with the conformance resource permissions and network access
to the advertised addresses.

```go
package localrunner

import (
	"flag"
	"testing"

	"github.com/stretchr/testify/require"
	clientset "k8s.io/client-go/kubernetes"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/gateway-api/conformance"
)

var (
	kubeAPIQPS   = flag.Float64("kube-api-qps", 100, "Kubernetes client requests per second")
	kubeAPIBurst = flag.Int("kube-api-burst", 200, "Kubernetes client request burst")
)

func TestGatewayConformance(t *testing.T) {
	opts := conformance.DefaultOptions(t)
	opts.RestConfig.QPS = float32(*kubeAPIQPS)
	opts.RestConfig.Burst = *kubeAPIBurst
	opts.ClientOptions.Scheme = opts.Client.Scheme()

	var err error
	opts.Client, err = client.New(opts.RestConfig, opts.ClientOptions)
	require.NoError(t, err, "initialize Kubernetes client with configured request limits")
	opts.Clientset, err = clientset.NewForConfig(opts.RestConfig)
	require.NoError(t, err, "initialize Kubernetes clientset with configured request limits")
	t.Logf("Kubernetes API QPS=%g burst=%d", opts.RestConfig.QPS, opts.RestConfig.Burst)

	conformance.RunConformanceWithOptions(t, opts)
}
```

### Failed tests

The lists below count each failed test once; shared Gateway failures also
contribute to all three profile reports. The YAML is authoritative.

| Test | Profile membership |
| --- | --- |
| `GatewayInvalidParametersRef` | GATEWAY-HTTP core, GATEWAY-TCP core, GATEWAY-UDP core |
| `GatewayListenerUnsupportedProtocol` | GATEWAY-HTTP core, GATEWAY-TCP core, GATEWAY-UDP core |
| `HTTPRouteHTTPSListener` | GATEWAY-HTTP core |
| `HTTPRouteHeaderMatching` | GATEWAY-HTTP core |
| `HTTPRouteHostnameIntersection` | GATEWAY-HTTP core |
| `HTTPRouteInvalidBackendRefUnknownKind` | GATEWAY-HTTP core |
| `HTTPRouteInvalidCrossNamespaceBackendRef` | GATEWAY-HTTP core |
| `HTTPRouteInvalidNonExistentBackendRef` | GATEWAY-HTTP core |
| `HTTPRouteInvalidReferenceGrant` | GATEWAY-HTTP core |
| `HTTPRouteListenerHostnameMatching` | GATEWAY-HTTP core |
| `HTTPRouteMatching` | GATEWAY-HTTP core |
| `HTTPRouteMatchingAcrossRoutes` | GATEWAY-HTTP core |
| `HTTPRouteMethodMatching` | GATEWAY-HTTP extended |
| `HTTPRouteMultipleGateways` | GATEWAY-HTTP core |
| `HTTPRouteNoBackendRefs` | GATEWAY-HTTP core |
| `HTTPRoutePartiallyInvalidViaInvalidReferenceGrant` | GATEWAY-HTTP core |
| `HTTPRoutePathMatchOrder` | GATEWAY-HTTP core |
| `HTTPRouteReferenceGrant` | GATEWAY-HTTP core |
| `HTTPRouteRequestHeaderModifier` | GATEWAY-HTTP core |
| `HTTPRouteResponseHeaderModifier` | GATEWAY-HTTP extended |
| `HTTPRouteWeight` | GATEWAY-HTTP core |
| `TCPRouteInvalidNonTCPListener` | GATEWAY-TCP core |
| `TCPRouteMultipleRoutesAttachment` | GATEWAY-TCP core |
| `TCPRouteWeightedRouting` | GATEWAY-TCP core |
| `UDPRouteMultipleRoutesAttachment` | GATEWAY-UDP core |
| `UDPRouteWeightedRouting` | GATEWAY-UDP core |
