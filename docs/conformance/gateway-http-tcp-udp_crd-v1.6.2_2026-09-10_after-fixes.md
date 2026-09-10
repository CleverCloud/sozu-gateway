# Gateway API v1.6.2: conformance fixes, 2026-09-10

This is a historical experiment on the recorded combined revisions. It includes
the workarounds from #66, #67, #74 and #75, subsequently closed in favor of
native Sōzu work, and the unmerged wildcard workaround in #69. **The 53/57
result does not describe the current default branch.** The measured verdicts
remain unchanged; the final [upstream YAML](gateway-http-tcp-udp_crd-v1.6.2_2026-09-10_after-fixes.yaml)
is retained alongside this report.

The [evidence archive](#evidence-archive) preserves the original logs, metadata,
probes and unsuccessful attempts. Paths beginning with `artifacts/` below
refer to files inside that archive, relative to its `docs/conformance/` directory.

The final combined implementation completes **53/57 distinct selected tests**, compared with **31/57** at baseline and **52/57** in the first combined run. It preserves all 31 baseline PASS and changes 22 FAIL to PASS; four HTTP tests still fail. All three full results remain retained unchanged. Focused tests and manual measurements are reported separately and are never added together to create a profile score.

## Scope and implementation identity

All full campaigns use the official Gateway API **v1.6.2** suite at revision `ca6c2a65454737236fb7a937bd9b17e42b07e9de`, standard v1.6.2 CRDs and Sōzu **2.2.1**. They select GATEWAY-HTTP, GATEWAY-TCP and GATEWAY-UDP, plus the response-header modification, scheme-redirect and method-matching extensions. The same 57 distinct tests are selected from 153 registered tests; 96 other tests are excluded by profile/features. Eleven Gateway tests belong to all three profiles, so profile denominators must not be summed.

The runner uses Kubernetes QPS 100/burst 200 and `--disable-parallel-tests=true`, with unmodified upstream fixtures and assertions. The cluster server is v1.36.3. Requests originate in a Pod on the `sozu-gateway-upgrade` test cluster. A public Service address exercised from that Pod does not establish the complete external-client network path.

| Identity | Baseline | First combined | Final candidate |
| --- | --- | --- | --- |
| Controller Git revision | `431017f81045d5ca04be289a1016dcfaedf1c1b2` | `9dc057c9b534ae2e33c2c43f959dbbd4ad92f02e` | `9cb7e7f2ba7281520645180cbb46cd20d427454a` |
| Controller image digest | See baseline execution metadata (`artifacts/2026-09-10-fixes/baseline/run-metadata.json`) | `sha256:f25bfeab1c53c29600b328dd3beb90c7850068e79d61ee84f33fde73b91fc91c` | `sha256:264f202defbdeffdc97b743575965c992da1e4ea0bb2e4a996db8cee319b8174` |
| Build provenance | Ubuntu 24.04 package build; did not test the repository Dockerfile | Dockerfile build (`artifacts/2026-09-10-fixes/builds/combined-final/run.json`) | Dockerfile build (`artifacts/2026-09-10-fixes/builds/combined-endpoints-tls-retry/run.json`) |
| Execution evidence | Log (`artifacts/2026-09-10-fixes/baseline/suite.log`), unchanged report (`artifacts/2026-09-10-fixes/baseline/report.yaml`) | Log (`artifacts/2026-09-10-fixes/runs/combined-full/suite.log`), metadata (`artifacts/2026-09-10-fixes/runs/combined-full/metadata.json`), unchanged report (`artifacts/2026-09-10-fixes/runs/combined-full/report.yaml`) | Log (`artifacts/2026-09-10-fixes/runs/combined-final-full/suite.log`), metadata (`artifacts/2026-09-10-fixes/runs/combined-final-full/metadata.json`), unchanged report (`artifacts/2026-09-10-fixes/runs/combined-final-full/report.yaml`) |
| Run interval UTC | 10:27:24–11:13:47 | Runner 13:20:28; report 13:27:35; cleanup finished 13:28:30 | Runner 14:14:24; report 14:21:10; cleanup finished 14:21:47 |
| Process/wrapper verdict | FAIL, exit 1 | FAIL, exit 1 | FAIL, exit 1 |

Both combined images use Sōzu runtime digest `sha256:7ba9ef2f63aa53766ed5d66f7f756bedf1bf39232a2024368b8081f9c933a096`. Their runner image is `sha256:03e2fa4d78b5f9bfa8a86fc36a9c581a781c25708e77ca5e99e57c93ca237378`, built from harness `b1c0860`; orchestration checkout is `eee8be1e7260525fedd5812d3cde931f3f1475bb`. These harness revisions are separate from the controller being tested. The metadata records actual runtime image IDs and exact argv; temporary registry tags may expire, so retained digests identify the artifacts.

The initial combined implementation covers Gateway conditions/parameters ([#62](https://github.com/CleverCloud/sozu-gateway/pull/62), [#64](https://github.com/CleverCloud/sozu-gateway/pull/64)), UDP flow ownership and L4 attachment counts ([#63](https://github.com/CleverCloud/sozu-gateway/pull/63), [#72](https://github.com/CleverCloud/sozu-gateway/pull/72)), header edits, path and route precedence ([#65](https://github.com/CleverCloud/sozu-gateway/pull/65), [#66](https://github.com/CleverCloud/sozu-gateway/pull/66), [#73](https://github.com/CleverCloud/sozu-gateway/pull/73)), certificates and wildcard hostnames ([#67](https://github.com/CleverCloud/sozu-gateway/pull/67), [#68](https://github.com/CleverCloud/sozu-gateway/pull/68), [#69](https://github.com/CleverCloud/sozu-gateway/pull/69)), listener exposure and Gateway instances ([#70](https://github.com/CleverCloud/sozu-gateway/pull/70), [#76](https://github.com/CleverCloud/sozu-gateway/pull/76)), invalid backends and weights ([#74](https://github.com/CleverCloud/sozu-gateway/pull/74), [#75](https://github.com/CleverCloud/sozu-gateway/pull/75)). The final candidate adds the missing-publish-Service condition fix, validation of inferred certificate names and immediate EndpointSlice notifications ([#77](https://github.com/CleverCloud/sozu-gateway/pull/77)). Focused execution records (`artifacts/2026-09-10-fixes/runs/README.md`) preserve their tested revisions and results; the reproducible runner is [#71](https://github.com/CleverCloud/sozu-gateway/pull/71).

Later signoff corrections changed commit identifiers without changing their corresponding trees. Branch mappings (`artifacts/2026-09-10-fixes/signoff-rewrite-20260910.json`) and the combined mapping (`artifacts/2026-09-10-fixes/combined-signoff-rewrite.json`) preserve that relationship. The historical run remains attributed to the actually deployed 9dc, whose unchanged tree also exists at `1fb5c7ff97d293d83c85dee1be840810320cae8c`; it is not relabelled as testing the later fixes.

## Full profile results

| Profile | Baseline PASS / FAIL / selected SKIP | First combined 9dc | Final 9cb |
| --- | --- | --- | --- |
| HTTP core, 37 selected | 18 / 19 / 0 | 34 / 3 / 0 | 34 / 3 / 0 |
| HTTP extended, 3 selected | 1 / 2 / 0 | 2 / 1 / 0 | 2 / 1 / 0 |
| TCP core, 19 selected | 14 / 5 / 0 | 19 / 0 / 0 | 19 / 0 / 0 |
| UDP core, 20 selected | 16 / 4 / 0 | 19 / 1 / 0 | 20 / 0 / 0 |
| Distinct union, 57 selected | 31 / 26 / 0 | 52 / 5 / 0 | 53 / 4 / 0 |

The final report has TCP and UDP core `success`; HTTP core and extended remain `failure`. The first combined report retains TCP core `success` and HTTP/UDP `failure`. Both have zero missing selected verdicts and zero selected SKIP. `GatewayClass.status.supportedFeatures` remains empty; the three extended feature flags select tests and do not themselves assert successful support. This report makes no overall Gateway API conformance claim.

| Official ShortName | Profiles | Baseline | First combined 9dc | Final 9cb |
| --- | --- | --- | --- | --- |
| `GatewayClassObservedGenerationBump` | HTTP core, TCP core, UDP core | PASS | PASS | PASS |
| `GatewayInvalidParametersRef` | HTTP core, TCP core, UDP core | FAIL | PASS | PASS |
| `GatewayInvalidRouteKind` | HTTP core, TCP core, UDP core | PASS | PASS | PASS |
| `GatewayInvalidTLSConfiguration` | HTTP core, TCP core, UDP core | PASS | PASS | PASS |
| `GatewayListenerUnsupportedProtocol` | HTTP core, TCP core, UDP core | FAIL | PASS | PASS |
| `GatewayModifyListeners` | HTTP core, TCP core, UDP core | PASS | PASS | PASS |
| `GatewayObservedGenerationBump` | HTTP core, TCP core, UDP core | PASS | PASS | PASS |
| `GatewaySecretInvalidReferenceGrant` | HTTP core, TCP core, UDP core | PASS | PASS | PASS |
| `GatewaySecretMissingReferenceGrant` | HTTP core, TCP core, UDP core | PASS | PASS | PASS |
| `GatewaySecretReferenceGrantAllInNamespace` | HTTP core, TCP core, UDP core | PASS | PASS | PASS |
| `GatewaySecretReferenceGrantSpecific` | HTTP core, TCP core, UDP core | PASS | PASS | PASS |
| `GatewayWithAttachedRoutes` | HTTP core | PASS | PASS | PASS |
| `HTTPRouteCrossNamespace` | HTTP core | PASS | PASS | PASS |
| `HTTPRouteExactPathMatching` | HTTP core | PASS | PASS | PASS |
| `HTTPRouteHTTPSListener` | HTTP core | FAIL | PASS | PASS |
| `HTTPRouteHeaderMatching` | HTTP core | FAIL | FAIL | FAIL |
| `HTTPRouteHostnameIntersection` | HTTP core | FAIL | PASS | PASS |
| `HTTPRouteInvalidBackendRefUnknownKind` | HTTP core | FAIL | PASS | PASS |
| `HTTPRouteInvalidCrossNamespaceBackendRef` | HTTP core | FAIL | PASS | PASS |
| `HTTPRouteInvalidCrossNamespaceParentRef` | HTTP core | PASS | PASS | PASS |
| `HTTPRouteInvalidNonExistentBackendRef` | HTTP core | FAIL | PASS | PASS |
| `HTTPRouteInvalidParentRefNotMatchingSectionName` | HTTP core | PASS | PASS | PASS |
| `HTTPRouteInvalidReferenceGrant` | HTTP core | FAIL | PASS | PASS |
| `HTTPRouteListenerHostnameMatching` | HTTP core | FAIL | PASS | PASS |
| `HTTPRouteMatching` | HTTP core | FAIL | FAIL | FAIL |
| `HTTPRouteMatchingAcrossRoutes` | HTTP core | FAIL | FAIL | FAIL |
| `HTTPRouteMethodMatching` | HTTP ext | FAIL | FAIL | FAIL |
| `HTTPRouteMultipleGateways` | HTTP core | FAIL | PASS | PASS |
| `HTTPRouteNoBackendRefs` | HTTP core | FAIL | PASS | PASS |
| `HTTPRouteObservedGenerationBump` | HTTP core | PASS | PASS | PASS |
| `HTTPRoutePartiallyInvalidViaInvalidReferenceGrant` | HTTP core | FAIL | PASS | PASS |
| `HTTPRoutePathMatchOrder` | HTTP core | FAIL | PASS | PASS |
| `HTTPRouteRedirectHostAndStatus` | HTTP core | PASS | PASS | PASS |
| `HTTPRouteRedirectScheme` | HTTP ext | PASS | PASS | PASS |
| `HTTPRouteReferenceGrant` | HTTP core | FAIL | PASS | PASS |
| `HTTPRouteRequestHeaderModifier` | HTTP core | FAIL | PASS | PASS |
| `HTTPRouteResponseHeaderModifier` | HTTP ext | FAIL | PASS | PASS |
| `HTTPRouteServiceTypes` | HTTP core | PASS | PASS | PASS |
| `HTTPRouteSimpleSameNamespace` | HTTP core | PASS | PASS | PASS |
| `HTTPRouteWeight` | HTTP core | FAIL | PASS | PASS |
| `TCPRouteInvalidBackendRefNonexistent` | TCP core | PASS | PASS | PASS |
| `TCPRouteInvalidCrossNamespaceBackendRef` | TCP core | PASS | PASS | PASS |
| `TCPRouteInvalidNonTCPListener` | TCP core | FAIL | PASS | PASS |
| `TCPRouteMultipleRoutesAttachment` | TCP core | FAIL | PASS | PASS |
| `TCPRouteParentRefAttachAll` | TCP core | PASS | PASS | PASS |
| `TCPRouteParentRefPortAndSectionName` | TCP core | PASS | PASS | PASS |
| `TCPRouteReferenceGrant` | TCP core | PASS | PASS | PASS |
| `TCPRouteWeightedRouting` | TCP core | FAIL | PASS | PASS |
| `UDPRoute` | UDP core | PASS | PASS | PASS |
| `UDPRouteInvalidBackendRefNonexistent` | UDP core | PASS | PASS | PASS |
| `UDPRouteInvalidCrossNamespaceBackendRef` | UDP core | PASS | PASS | PASS |
| `UDPRouteMultipleRoutesAttachment` | UDP core | FAIL | PASS | PASS |
| `UDPRouteNotAllowedByListeners` | UDP core | PASS* | PASS* | PASS* |
| `UDPRouteParentRefAttachAllListeners` | UDP core | PASS | PASS | PASS |
| `UDPRouteParentRefPortAndSectionName` | UDP core | PASS | PASS | PASS |
| `UDPRouteReferenceGrant` | UDP core | PASS | PASS | PASS |
| `UDPRouteWeightedRouting` | UDP core | FAIL | FAIL | PASS |

`PASS*`: `UDPRouteNotAllowedByListeners` returns before its behavioral assertions when TLSRoute support is absent. This [upstream guard](https://github.com/kubernetes-sigs/gateway-api/blob/ca6c2a65454737236fb7a937bd9b17e42b07e9de/conformance/tests/udproute-not-allowed-by-listeners.go#L47-L50) applies to the selected feature set. In 9dc, log lines 3369–3375 show apply→test body→cleanup, with no condition/count subtests; line 4134 reports PASS. The final log repeats that sequence at lines 3337–3343 and records PASS at line 4068. Preserve the official result, but do not count it as demonstrated listener rejection. Upstream “Provisional” labels on TCP/UDP tests describe test maturity, not an unfinished verdict.

## Remaining failures and UDP convergence

### HTTP header predicates

The same four HTTP tests fail in both combined runs, with identical verdicts for all their subcases: thirteen traffic cases fail. The builder skips header/query predicates and emits `HeaderOrQueryMatchUnsupported`; the failing cases here use headers. Query matching is not selected and is not an observed cause. The first-run case audit (`artifacts/2026-09-10-fixes/combined-full-audit.json`), final log (`artifacts/2026-09-10-fixes/runs/combined-final-full/suite.log`) and three-run audit (`artifacts/2026-09-10-fixes/conformance-comparison.json`) retain their observations and verdicts. Each failing case records 30 failed expectation checks and a terminal 30-second FAIL. The responses below are observed in both combined runs.

`v1`, `v2` and `v3` below identify the corresponding `infra-backend` Service. Indices refer to the official case arrays.

| Test / failing indices | Request predicate | Expected | Observed |
| --- | --- | --- | --- |
| HeaderMatching 0, 1 | GET `/`, `Version: one` / `two` | 200 from v1 / v2 | 404 |
| HeaderMatching 2, 3 | GET `/`, `Version: two` plus `Color: orange` / `blue` | 200 from v1 / v2 | 404 |
| HeaderMatching 6, 7, 8, 9 | GET `/`, `Color: blue`, `green`, `red`, `yellow` | 200 from v1, v1, v2, v2 | 404 |
| Matching 5 | GET `/`, `Version: two` | 200 from v2 | 200 from v1 |
| MatchingAcrossRoutes 7 | GET `example.com/`, `Version: two` | 200 from v2 | 200 from v1 |
| MethodMatching 4 | PUT `/`, `version: one` | 200 from v2 | 404 |
| MethodMatching 5 | POST `/path2`, `version: two` | 200 from v3 | 200 from v1 |
| MethodMatching 7 | DELETE `/path4`, `version: three` | 200 from v1 | 404 |

The successful subcases narrow those findings. [HeaderMatching](https://github.com/kubernetes-sigs/gateway-api/blob/ca6c2a65454737236fb7a937bd9b17e42b07e9de/conformance/tests/httproute-header-matching.go) passes only its three expected-404 cases, 3/11. [Matching](https://github.com/kubernetes-sigs/gateway-api/blob/ca6c2a65454737236fb7a937bd9b17e42b07e9de/conformance/tests/httproute-matching.go) passes 8/9, including the previously failing `/v2`, `/v2/example` and `/v2/`. [MatchingAcrossRoutes](https://github.com/kubernetes-sigs/gateway-api/blob/ca6c2a65454737236fb7a937bd9b17e42b07e9de/conformance/tests/httproute-matching-across-routes.go) passes 7/8. [MethodMatching](https://github.com/kubernetes-sigs/gateway-api/blob/ca6c2a65454737236fb7a937bd9b17e42b07e9de/conformance/tests/httproute-method-matching.go) passes 9/12, including ordinary method/path selection and negative cases without required headers. Passing requests that also carry a header may still use a matching fallback or method-only route; they do not establish header matching. The first-combined log records those subcases at lines 3880–3891 and 3967–3998; the final log records them at lines 3815–3825 and 3902–3932. Final response errors occur at lines 661–1146 for HeaderMatching, 1627–1665 for MatchingAcrossRoutes, 1698–1751 for Matching and 1797–1978 for MethodMatching.

Some request subtests use `t.Parallel()` even when suite-level parallel tests are disabled. Short parent durations exclude time waiting for those children. The completed v1.6.2 report agrees with the final Go verdicts, including all child failures.

### UDP weighted convergence

Baseline `UDPRouteWeightedRouting` passed its readiness/acceptance gates, but the initial echo and all ten sample-collection batches failed; it did not measure an incorrect ratio. The baseline analysis (`artifacts/2026-09-10-fixes/baseline/lot6-udp-analysis.md`) distinguishes those sending errors from distribution failures.

On 9dc, readiness passes at 13:27:20.114 UTC, route conditions/parents match at .210 and the initial echo succeeds at .232. All ten distribution attempts from .293 to .848 receive only v2: expected shares are v1 0.7, v2 0.3 and v3 zero, with absolute tolerance 0.05. The [upstream test](https://github.com/kubernetes-sigs/gateway-api/blob/ca6c2a65454737236fb7a937bd9b17e42b07e9de/conformance/tests/udproute-weighted-routing.go#L54-L87) runs ten immediate attempts; its [helper](https://github.com/kubernetes-sigs/gateway-api/blob/ca6c2a65454737236fb7a937bd9b17e42b07e9de/conformance/utils/weight/weight.go#L58-L125) sends 500 datagrams per attempt. The ten completed batches without send errors imply **5,000 replies, all v2**, during a 0.62-second distribution subtest; the retained log is per batch and does not contain one entry per datagram. This is a recorded failure after successful prerequisites, even though the zero-weight backend receives nothing. Evidence: log lines 3580–3619 and terminal verdicts 4151–4152.

The timing is consistent with data-plane convergence after readiness. The retained 9dc run lacks contemporaneous EndpointSlice-arrival or Sōzu apply timestamps, so it does not prove the causal chain. The first failure remains retained alongside later measurements.

Three separate official `UDPRouteWeightedRouting` invocations on 9cb all complete with PASS and exit zero. Their successful terminal verdicts retain important intermediate observations:

| Focused invocation | Initial echo | Distribution |
| --- | --- | --- |
| 1 (`artifacts/2026-09-10-fixes/runs/udp-weighted-9cb-1/metadata.json`) | One two-second timeout, then successful retry | First distribution attempt succeeds |
| 2 (`artifacts/2026-09-10-fixes/runs/udp-weighted-9cb-2/metadata.json`) | Succeeds | First seven attempts fail; eighth succeeds |
| 3 (`artifacts/2026-09-10-fixes/runs/udp-weighted-9cb-3/metadata.json`) | Succeeds | First attempt succeeds |

In focused run 2, the first six batches receive only v1, and batch seven reports 90.4% v1 / 9.6% v2 before batch eight succeeds. The successful batches do not log their exact ratios. The focused comparison (`artifacts/2026-09-10-fixes/udp-weighted-9cb-analysis.md`) retains the intermediate errors and verifies matching source, fixtures and commands. These passes do not erase 9dc's failure or guarantee immediate distribution.

The final full run independently records **PASS** for UDP weighted routing at log lines 4085–4086. After readiness at 14:20:57.552 UTC, its initial echo times out after two seconds, then succeeds on retry at 14:20:59.629 (lines 3546–3552). No failed distribution batch is logged before cleanup at 14:20:59.694, so success on the first batch is inferred from the upstream control flow; no exact successful ratio is retained. The 2.07-second subtest duration includes the echo timeout and is not a measurement of distribution convergence alone. All these official UDP weighted invocations target only **82.47.250.4:5300**, unlike the manual HTTP/TLS probes that cover both public VIPs.

## Manual traffic and availability measurements

### EndpointSlice changes

The complete 9dc (`artifacts/2026-09-10-fixes/runs/endpoint-latency-9dc-complete/summary.json`) and 9cb (`artifacts/2026-09-10-fixes/runs/endpoint-latency-9cb/summary.json`) measurements each apply twenty EndpointSlice changes across two VIPs: 40/40 change/address outcomes converge, none is censored. Helper source/binary, parameters, existing backend Pod IPs/UIDs and configured debounce **500 ms** match. Runtime snapshots verify the expected digest on all four controller Pods before and after each measurement.

| VIP | 9dc third new-Pod response after patch completion: p50 / p95 / max | 9cb: p50 / p95 / max |
| --- | --- | --- |
| 82.47.250.4 | 732.42 / 881.63 / 883.34 ms | 221.26 / 363.59 / 375.33 ms |
| 82.47.250.5 | 733.92 / 881.44 / 897.72 ms | 220.85 / 366.55 / 372.32 ms |

Median observed convergence decreases by **511–513 ms, about 70%**. Across the complete twenty-change sequence, each run records 1,600 fresh HTTP/1.1 requests, all HTTP 200, with no transport/parse error or connection reuse. Old-Pod replies during convergence decrease from 336/335 to 126/126 on the two VIPs; these remain stale-routing observations despite HTTP 200. Neither run observes an old-Pod reply after its confirming three-response series within the rest of that change window. The full comparison (`artifacts/2026-09-10-fixes/endpoint-latency-comparison.md`) and raw audit (`artifacts/2026-09-10-fixes/endpoint-latency-comparison.json`) include first-response metrics and every change/address outcome.

Timing uses one host monotonic clock around `kubectl patch` and receipt of sampler output, including command/stream overhead. This is manual HTTP churn between already-ready Pods, not an internal-controller timing measurement, a general latency bound or proof of UDP weighted convergence. There is one complete run per image, and the candidate also contains the publisher-cache and certificate-name fixes; this comparison does not isolate one patch’s causal effect. The invalid-label preparation attempt and interrupted-stream attempt remain retained separately and excluded from these totals.

### Certificate-name inference

On 9cb, adding a hostname-less listener sharing an existing certificate is tested with an invalid SAN and with no inferable SAN/CN names. Across both public VIPs and both default gateway Pod IPs, **2,379/2,379 fresh TLS connections retain the expected explicit-host certificate fingerprint**: 1,189 in the invalid-SAN case and 1,190 in the empty-name case. The named listener stays programmed, the invalid inferred listener is rejected and successful reconcile counters advance without reconcile failures or shadow resets. The result (`artifacts/2026-09-10-fixes/runs/tls-inference-final/result.json`) retains the per-case counts and metrics; the invalid-SAN status (`artifacts/2026-09-10-fixes/runs/tls-inference-final/invalid-san/added-status.json`) and empty-name status (`artifacts/2026-09-10-fixes/runs/tls-inference-final/no-names/added-status.json`) retain the listener conditions.

This probe deliberately disables certificate-chain/hostname trust verification and checks the exact leaf fingerprint; it tests certificate selection and availability, not trusted HTTP traffic. The official HTTPS-listener test separately checks its TLS request path. Earlier certificate-name mutation probes retained six transient mismatches in ninety observations, and a failed assumption that unknown SNI would have no fallback certificate. Those notes and observations (`artifacts/2026-09-10-fixes/tls-probe/probe-notes.md`) remain valid; successful eventual selection does not establish a zero-length remove/add certificate gap.

### Weights and HTTP 500/503 shares

With valid Services containing one and three ready Pods, the earlier manual weights probe passes all six checks: 400 fresh connections per VIP for target shares 0.5, 0.25 and 1.0, **2,400 connections total**, including no traffic to a zero-weight target. The retained samples (`artifacts/2026-09-10-fixes/weights-probe/README.md`) also preserve earlier persistent-connection observations: Sōzu reuses an already connected backend before another balancing decision. Configured weights therefore apply to new backend selections and do not guarantee per-request proportions on a persistent HTTP connection; existing UDP flows also retain their backend.

On combined 9dc, eight mixed error-share stages pass on both VIPs, 400 fresh HTTP/1.1 connections each: **6,400 requests**, all first samples passing, exact error bodies and backend identities checked. Absolute share tolerance is 0.08. Counts (`artifacts/2026-09-10-fixes/runs/weighted-errors-9dc-retry/compact-summary.json`) and the complete summary (`artifacts/2026-09-10-fixes/runs/weighted-errors-9dc-retry/summary.json`) retain all results:

| Stage | VIP .4 | VIP .5 |
| --- | --- | --- |
| Half missing Service | 208 HTTP 500; 192 healthy | 183 HTTP 500; 217 healthy |
| Half unavailable Service | 204 HTTP 503; 196 healthy | 191 HTTP 503; 209 healthy |
| All weights zero | 400 HTTP 500 | 400 HTTP 500 |
| Invalid reference with weight zero | 400 healthy | 400 healthy |
| Grant initially denied | 208 HTTP 500; 192 healthy | 217 HTTP 500; 183 healthy |
| Grant allowed | 205 healthy; 195 remote | 208 healthy; 192 remote |
| Grant revoked | 216 HTTP 500; 184 healthy | 194 HTTP 500; 206 healthy |
| Grant restored | 199 healthy; 201 remote | 209 healthy; 191 remote |

Status is checked before each grant-state sample, so a first-sample PASS is not a zero-delay claim after the API change. A prior preparation failure (`artifacts/2026-09-10-fixes/runs/weighted-errors-9dc/summary.json`) encountered a null EndpointSlice list and ran no traffic stage; it is retained. Revision/digest fields in this helper are caller-supplied. The pre-probe Deployment check was not archived separately; the retained pre-full Pod snapshot (`artifacts/2026-09-10-fixes/combined-pods-before-full.json`) and post-probe runtime snapshot (`artifacts/2026-09-10-fixes/runs/weighted-errors-9dc-retry/gateway-runtime-after-probes.json`) provide the surrounding runtime evidence.

The loopback error responders close backend connections, so a persistent client may eventually select and retain a healthy backend. The independent-connection results do not remove that limitation for mixed error proportions.

### Controller-only restart

The 9dc restart measurement (`artifacts/2026-09-10-fixes/controller-restart-probe/summary.json`) sends fresh HTTP/1.1 requests directly to **one Pod IP, 172.16.0.132:8080**, bypassing the public Service. Only controller PID 1 receives SIGTERM; its restart count changes 0→1, then it becomes Ready. Sōzu keeps the same container ID and restart count zero.

Over fifty seconds at 20 Hz per path, `/all-zero` returns **959 HTTP 500 and 41 consecutive HTTP 503**, with no transport error. The first 503 is at 13:40:49.336878955 UTC, the last at 13:40:51.336968826 and the next 500 at 13:40:51.386977305: approximately **2.05 seconds from the first failed sample to observed recovery**, at 50 ms resolution. Correct 500 bodies have 22 bytes; transient 503 bodies come from Sōzu. The healthy `/missing-zero` route returns **1,000/1,000 HTTP 200** throughout.

This demonstrates a temporary loss of the local 500 response during one controller restart. It does not bound all restarts, Service withdrawal/PDB timing, the shutdown tail or mixed error proportions during responder downtime.

### Other retained checks and topology limits

Header-edit probes pass across HTTP/1.1, HTTPS/1.1 and HTTP/2 on both public VIPs, including duplicate values (VIP .4 (`artifacts/2026-09-10-fixes/headers-probe/82.47.250.4.jsonl`), VIP .5 (`artifacts/2026-09-10-fixes/headers-probe/82.47.250.5.jsonl`)). Path mutation probes converge after add/repoint/remove operations, but retain **eight transient failures among 196 observations** (log (`artifacts/2026-09-10-fixes/path-probe/results.jsonl`)); they do not establish zero interruption. Replacing pre-fix state containing native exact frontends and applying the UDP flow-setting changes required complete Pod rollouts during this campaign. Separate UDP ownership checks improve from four correct replies among twelve clients to **48/48** after rollout, with before (`artifacts/2026-09-10-fixes/before-vip-udp.jsonl`), direct-backend control (`artifacts/2026-09-10-fixes/direct-backend-udp.jsonl`) and post-change logs retained.

The actual combined chart exposes the additional protocol ports required by the fixtures: HTTP 5300/TCP maps to internal 15300 alongside UDP 5300, and additional HTTPS uses 9443. Earlier bind-conflict/connection-refused attempts were resolved by deployment configuration before official assertions; fixture protocols were not changed.

Gateway address isolation uses a real second data-plane instance with a private ClusterIP Service. The provider rejected a second public load balancer with **HTTP 507**, `insufficient-storage` for `loadbalancer_ip` (events (`artifacts/2026-09-10-fixes/isolation-loadbalancer-capacity.json`)). The focused isolation run used 172.31.81.78; the first full combined run used **172.31.218.218** alongside public VIPs 82.47.250.4/.5 (Services (`artifacts/2026-09-10-fixes/combined-services-before-full.json`)). `HTTPRouteMultipleGateways` passes its four backend checks in both combined runs (final log lines 3933–3939). This proves separate routing in the in-cluster public/private topology, not the availability of two public load balancers.

## Reproduction, artifact integrity and cleanup

Local validation of the final controller source tree passes **315 workspace tests**, formatting and workspace Clippy with warnings denied. The selected validation record (`artifacts/2026-09-10-fixes/derived-local-validation.json`) identifies the tested tree and original record hash; the subsequent signoff-only rewrite changed no files.

### Evidence archive

The [archive](https://github.com/CleverCloud/sozu-gateway/releases/download/conformance-evidence-2026-09-10/sozu-gateway-conformance-2026-09-10.tar.gz)
and its [SHA-256 file](https://github.com/CleverCloud/sozu-gateway/releases/download/conformance-evidence-2026-09-10/sozu-gateway-conformance-2026-09-10.tar.gz.sha256)
preserve all 325 campaign artifacts and the three original report files from
source commit `6ac08fd665e2a2a7f67de6be74a9521ac422c95d`. Each archived file
was checked against its Git blob. The compressed archive is 1,312,119 bytes;
its SHA-256 is `0203830cc248d74e12eb070aa279c5ff9b71e5f6770beb3e106e64247c120273`.
The archive release tag identifies the publication base, not a tested controller.

The artifact index (`artifacts/2026-09-10-fixes/README.md`) and inventory retain
complete logs, metadata and unsuccessful attempts. Metadata keeps original
argv/source-path strings as historical provenance. Download and extract the
archive before running the portable audit; Python 3 and PyYAML are required:

```sh
mkdir -p .scratch/conformance-evidence
cd .scratch/conformance-evidence
curl --fail --location --remote-name \
  https://github.com/CleverCloud/sozu-gateway/releases/download/conformance-evidence-2026-09-10/sozu-gateway-conformance-2026-09-10.tar.gz
curl --fail --location --remote-name \
  https://github.com/CleverCloud/sozu-gateway/releases/download/conformance-evidence-2026-09-10/sozu-gateway-conformance-2026-09-10.tar.gz.sha256
sha256sum --check sozu-gateway-conformance-2026-09-10.tar.gz.sha256
tar -xzf sozu-gateway-conformance-2026-09-10.tar.gz
python3 sozu-gateway-conformance-2026-09-10/docs/conformance/artifacts/2026-09-10-fixes/audit-conformance.py \
  --artifacts sozu-gateway-conformance-2026-09-10/docs/conformance/artifacts/2026-09-10-fixes \
  --output conformance-comparison.json
```

It checks terminal Go verdicts against all YAML profile counts/failure names, selected-test completeness and combined-run exit codes, then rebuilds the three-column comparison. It requires final completion by default; focused PASS lines or missing final metadata cannot become a final successful result. The baseline report hash is `d9d3a91f865425194ea5a087c4abc9c1371d087327f7c654fcd934447643f07f`; first-combined report hash is `319a8052057f28524ff9d496caa21cd6edddb40808275f80956e3187d0d27b17`. Final report hash: `78dd322b2ba82404f8e63deb3efa400d908a1a58d9b838cf9c2dcccecea45369`.

Runner cleanup uses UUID ownership and UID deletion preconditions. Both combined full runs record no cleanup errors and no retained fixture GatewayClass. A stale-UID deletion check (`artifacts/2026-09-10-fixes/uid-delete-probe/result.json`) returned HTTP 409 and preserved the resource; deletion with its actual UID succeeded. Per-run cleanup does not prove restoration of the entire test cluster.

Cluster restoration (`artifacts/2026-09-10-fixes/restoration/result.json`) completed at 14:39:48 UTC. The original Helm values, three demonstration resource specifications and public Service UID/specification/addresses are restored. The two gateway Pods use the original controller digest `sha256:060e0832b9e184689a200d8c2ab8baaf9ce39fa6592a87ee787904054327c98e`, with all containers Ready and zero restarts. The separate Gateway instance, build Pod, runner, temporary namespaces and runner cluster RBAC are removed. Only the original six namespaces and `sozu` GatewayClass remain; all three existing nodes are Ready without DiskPressure. No node was added. Gateway API standard v1.6.2 CRDs remain installed.

The restored-image smoke (`artifacts/2026-09-10-fixes/restore-smoke/README.md`) is **FAIL overall**, preserved with exit 1: HTTP, HTTPS with chain/hostname verification and TCP each pass 20/20 checks, while UDP passes only **3/20** (3/10 on VIP .4, 0/10 on .5, three-second timeouts). Earlier readiness attempts are retained separately. Restoring the original image also removes the candidate fixes; baseline UDP defects were already measured, but this restoration smoke is not an isolated causal experiment. The original demonstration routes (`artifacts/2026-09-10-fixes/restore-smoke/demo-results.json`) pass all four checks across both VIPs (header response and HTTPS redirect). Both controllers report zero reconcile failures and shadow resets in the metrics (`artifacts/2026-09-10-fixes/restore-smoke/metrics-results.json`).

The smoke precedes a final NodePort restoration (`artifacts/2026-09-10-fixes/restoration/nodeport-restoration.json`): temporary exposure changes had released the original L4 NodePorts, so Helm allocated replacements. The original free TCP/UDP NodePorts were restored, and the complete Service specification was then compared with its saved original. No L4 traffic result is attributed to that final allocation change. The restored demonstration originally contained only one HTTP Gateway and two HTTPRoutes; the temporary TLS/L4 smoke resources were removed.
