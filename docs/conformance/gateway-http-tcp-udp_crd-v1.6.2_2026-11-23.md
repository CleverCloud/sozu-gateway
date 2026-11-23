# Gateway API conformance, 2026-11-23: Sōzu 2.3.2 against 2.3.6

The official Gateway API **v1.6.2** suite (upstream `ca6c2a65`), HTTP, TCP and UDP profiles, ran
twice on the same cluster with the same runner image and the same Helm values
([tests/conformance/values.yaml](../../tests/conformance/values.yaml), `replicaCount: 1`): once
with the controller at master `2fb8cfce` and `clevercloud/sozu:2.3.2`, once with the controller of
the Sōzu 2.3.6 bump and `clevercloud/sozu:2.3.6`. The question is whether the data-plane bump moves
any verdict. These are measurements of this implementation and deployment, not a certification.

| Profile | Sōzu 2.3.2 (core / extended) | Sōzu 2.3.6 (core / extended) |
| --- | --- | --- |
| GATEWAY-HTTP | **23 / 37**, 2 / 3 | **24 / 37**, 2 / 3 |
| GATEWAY-TCP | **17 / 19** | **17 / 19** |
| GATEWAY-UDP | **19 / 20** | **19 / 20** |

Across the 57 distinct selected tests, 39 pass on 2.3.2 and 40 on 2.3.6; 96 tests needed features
outside the selection and were skipped in both. **Every final verdict is the same in both runs
but one, and that one is decided by the controller before anything reaches Sōzu.**

## The one difference: `HTTPRouteMultipleGateways`

It fails on 2.3.2 and passes on 2.3.6. Its two subtests each attach a hostname-less `/` route
needing a different backend to Gateways sharing one address and port: the shared-listener
collision the [2026-09-10 analysis](gateway-http-tcp-udp_crd-v1.6.2_2026-09-10.md) describes, which
the builder arbitrates by route age and reports as `RouteCollision`. On 2.3.2 the
`Gateway_all-namespaces` route read `Accepted False … RouteCollision host+path */ is already served
by gateway-conformance-infra.infra-backend-v2.8080` and the subtest timed out waiting for
`Accepted True`; on 2.3.6 no `RouteCollision` appears in the log, `/` is served from v2 and then
v3, and all four traffic checks pass. The refusal is the builder's, taken before anything reaches
Sōzu; why it fired in one run and not the other is timing-dependent and not established here. The
pass is therefore not attributed to the bump, nor taken as evidence of a Sōzu change.

## Failing tests (2.3.6)

`HTTPRouteHeaderMatching`, `HTTPRouteHostnameIntersection`, `HTTPRouteInvalidBackendRefUnknownKind`,
`HTTPRouteInvalidCrossNamespaceBackendRef`, `HTTPRouteInvalidNonExistentBackendRef`,
`HTTPRouteInvalidReferenceGrant`, `HTTPRouteListenerHostnameMatching`, `HTTPRouteMatching`,
`HTTPRouteMethodMatching` (extended), `HTTPRouteNoBackendRefs`,
`HTTPRoutePartiallyInvalidViaInvalidReferenceGrant`, `HTTPRoutePathMatchOrder`,
`HTTPRouteReferenceGrant`, `HTTPRouteWeight`, `TCPRouteInvalidNonTCPListener`,
`TCPRouteWeightedRouting`, `UDPRouteWeightedRouting` — the same set as on 2.3.2, plus
`HTTPRouteMultipleGateways` there. The HTTP ones are the documented Sōzu limits: no native HTTP
500 for an invalid or missing backend reference (#78), no per-request weighted split (#79), header
matches and paths whose order Sōzu leaves to insertion (#80), one-label wildcards (#82); see
[features.md](../features.md). `TCPRouteWeightedRouting` and `UDPRouteWeightedRouting` are the
builder's choice instead: it refuses weighted `backendRefs` on every route kind (both passed in the
[2026-09-10 after-fixes campaign](gateway-http-tcp-udp_crd-v1.6.2_2026-09-10_after-fixes.md) with a
controller change, not a Sōzu one). `TCPRouteInvalidNonTCPListener` is a deployment limit: its
Gateway declares an HTTP listener on port 5300, which this exposure table publishes as UDP, so the
controller never accepts that Gateway (`ListenersNotValid`), as in the
[2026-09-10 campaign](gateway-http-tcp-udp_crd-v1.6.2_2026-09-10.md).

## Environment

- Kubernetes **v1.37.2**, two amd64 worker nodes, a provider LoadBalancer with two public
  addresses; standard Gateway API **v1.6.2** CRDs applied server-side before both runs.
- Runner: [tests/conformance](../../tests/conformance/README.md), built once from this repository and
  pushed by digest (`sha256:2484ba3e…`), run as a Pod in the cluster; QPS/Burst 100/200; extended
  features `HTTPRouteResponseHeaderModification`, `HTTPRouteSchemeRedirect`,
  `HTTPRouteMethodMatching` as in the earlier campaigns.
- Sōzu 2.3.2 run: 2026-11-23T10:19:34Z to 10:53:46Z, controller `2fb8cfceb92374f6ff2eea498adb9def3864a20e`,
  image `clevercloud/sozu:2.3.2` (`sha256:273fc588…`).
- Sōzu 2.3.6 run: 2026-11-23T09:44:48Z to 10:18:12Z, controller
  `8923b4ac09e7d63e11ed8de6f62c67ab5f0c3b36`, image `clevercloud/sozu:2.3.6` (`sha256:27a14a59…`).
  That revision is the first commit of the bump's branch (the library, the lock and the chart
  tag); the change as merged differs from it in tests, comments and documentation only.
- Both runs completed and exited 1 on selected failures, not on an orchestration error.

## Reports

Unaltered upstream reports:
[Sōzu 2.3.2](gateway-http-tcp-udp_crd-v1.6.2_2026-11-23_sozu-2.3.2.yaml)
(SHA-256 `9486fed7a5093ceceacf92644ad44d89069dcd6c354b82f26b32ce54089a3b0a`),
[Sōzu 2.3.6](gateway-http-tcp-udp_crd-v1.6.2_2026-11-23_sozu-2.3.6.yaml)
(SHA-256 `c9e83e2bc852d2b7d5e6b22590bd90701256f93fac2bb4eb3062b0bc15f2c178`).
