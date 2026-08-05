# End-to-end results

This document records the behaviour of the Sōzu gateway as validated end-to-end on a live
Kubernetes cluster, not just in unit tests. The pure logic (builder, translator) is covered by
golden tests in `crates/*/tests/`; this page is about the assembled system serving real traffic.

## Environment

- Managed Kubernetes cluster, **Cilium** CNI (LoadBalancer via Cilium LB-IPAM), single node,
  Kubernetes v1.36.
- Data plane: **Sōzu 2.2.0** (`clevercloud/sozu:2.2.0`), control plane built from this repo.
- The controller image was distributed via the anonymous `ttl.sh` registry (no credentials), the
  add-on installed with the Helm chart, traffic generated with [`hey`](https://github.com/rakyll/hey).

## 1. Functional path (Ingress + TLS)

Installing the chart, then a demo app (`whoami`) with a TLS `Ingress` of class `sozu`:

| Check | Result |
| ----- | ------ |
| `helm install` (controller + Sōzu in one Pod) | Pod `2/2` Running |
| `Service type=LoadBalancer` | external IP assigned (Cilium LB-IPAM) |
| HTTP through Sōzu (`Host: app.example.com`) | **200**, served by a real backend pod |
| HTTPS through Sōzu (SNI `app.example.com`) | **200**, served cert CN = `app.example.com` |
| Controller convergence | reacts to `Secret`/`EndpointSlice` appearing: `0 → 1 → 2` backends, cert added |
| Hot route removal (`kubectl delete ingress`) | subsequent requests `404` — no proxy restart |

## 2. Zero-downtime hot reload (config changes)

A web app (nginx, 3 replicas, `maxUnavailable=0` rolling update, `preStop` drain) behind an
`Ingress`, with **continuous load** (`hey -c 50`) flowing through the LoadBalancer while the app
was churned.

Operations performed during the load window: `rollout restart`, scale `3 → 8`, scale `8 → 1`, an
env-change rollout, and an Ingress **hot-add** of a `/v2` path.

| Metric | Result |
| ------ | ------ |
| Requests | **266 433** over 95 s (~**2 800 req/s**) |
| Status codes | **`[200]` 100 %** — 0 non-200 |
| Transport errors (refused/timeout) | **0** |
| Latency | p50 **17 ms**, p95 23 ms, p99 **29 ms**, max 252 ms |

The controller applied every backend/frontend delta to the **running** Sōzu (no restart): backends
tracked `0→…→8→…→1`, frontends `1→2` on the Ingress edit. **No outage, no 5xx.**

> Zero-downtime during pod churn also relies on the application draining gracefully
> (`maxUnavailable=0` + a `preStop` so a terminating pod keeps serving until the controller has
> reconciled it out of Sōzu). The controller never restarts the proxy and applies only minimal,
> idempotent deltas.

## 3. Data-plane (Sōzu) replacement under load

Replacing the Sōzu Pod itself — mechanically what a Sōzu version bump does — while load was
flowing through the LoadBalancer:

| Scenario | Requests | Result |
| -------- | -------- | ------ |
| `replicaCount=1`, Pod replaced | 128 179 | **`[200]` 100 %**, 0 errors |
| `replicaCount=2`, rolling replace | 170 066 | **`[200]` 100 %**, 0 errors |

Why it held: the rolling update keeps the old, already-programmed Pod serving until the new Pod is
`Ready`, and the new Pod's co-located controller programs Sōzu within the readiness delay — so by
the time the LoadBalancer routes to it, the routes exist.

> **Program gap — now gated.** The controller container exposes `/readyz`, which turns green only
> after its first successful reconcile (Sōzu programmed). A fresh Pod is therefore `Ready` — and
> joins the Service — only once its routes exist, closing the cold-start "program gap" the plain
> Sōzu TCP probe left open. For a robust data-plane upgrade still run `replicaCount >= 2` and set
> `maxUnavailable=0` explicitly. A real version bump must also bump the controller (built against a
> matching `sozu-command-lib`) and the Sōzu image together.

## 4. Gateway API (Phase 2)

Installing the Gateway API CRDs (v1.6.1 standard channel), then a `GatewayClass`, a `Gateway`
(HTTP + HTTPS listeners) and an `HTTPRoute` to the demo app:

| Check | Result |
| ----- | ------ |
| Controller detects the CRDs | logs `Gateway API detected; watching …` (Ingress-only otherwise) |
| Routing through Sōzu (same IR/translator) | HTTP **200** + HTTPS **200** |
| `GatewayClass` status | `Accepted=True` |
| `Gateway` status | `Accepted=True`, `Programmed=True` |
| `HTTPRoute` status (per parent) | `Accepted=True`, `ResolvedRefs=True` |
| Status loop-safety | `HTTPRoute` `resourceVersion` stable over 12 s — no self-triggered loop |

A Gateway route and an Ingress to the same Service share one Sōzu cluster, confirming both APIs
compile to the same IR.

## 5. HTTPRoute filters (Phase 3)

Two HTTPRoutes on the HTTP listener — one carrying header modifiers, one a redirect — exercised
against the `whoami` demo (which echoes the request it received):

| Check | Result |
| ----- | ------ |
| `RequestHeaderModifier` (`set X-Env: prod`) | whoami echoes `X-Env: prod` in the request it sees |
| `ResponseHeaderModifier` (`set X-Served-By: sozu`) | response carries `X-Served-By: sozu` |
| `RequestRedirect` (`scheme: https`, `statusCode: 301`) | **301 Moved Permanently**, `Location: https://old.example.com/` |
| Redirect-only route (no `backendRef`) | accepted and programmed as a cluster-less Sōzu frontend |

The redirect route has no `backendRef` (the Gateway API forbids combining `RequestRedirect` with
backends), so it maps to a frontend with no cluster — Sōzu answers the 301 itself.

## 5b. Layer-4 routing through the Gateway API (TCPRoute / UDPRoute)

`just e2e-l4-routes`, on Gateway API **v1.6.1** standard-channel CRDs (which is where
`tcproutes`/`udproutes` live from v1.6 on — they are no longer experimental-only). A `Gateway`
with a `protocol: TCP` listener on 9000 and a `protocol: UDP` listener on 9001, both declared in
the chart's `exposure` table, with a TCPRoute and a UDPRoute attached by `sectionName`.

Probed by an **in-cluster** socat client rather than `kubectl port-forward`: port-forward is
TCP-only, so a UDPRoute cannot be exercised through it at all, and talking to the Service also
covers the Service-port → in-pod-bind mapping the exposure table exists for.

| Check | Result |
| ----- | ------ |
| TCPRoute `Accepted` / `ResolvedRefs` | **True** / **True** |
| UDPRoute `Accepted` / `ResolvedRefs` | **True** / **True** |
| `status.parents[].parentRef.sectionName` | preserved (`echo-tcp`) — see the parentRef-fidelity fix |
| Listener `Programmed`, `attachedRoutes` | **True**, `1` on each of the two listeners |
| Raw TCP round-trip through Sōzu | echo returned (`hello-tcproute`) |
| Raw **UDP** round-trip through Sōzu | echo returned (`hello-udproute`) |
| Second TCPRoute claiming port 9000 | `Accepted=False`, reason `RouteConflict`, incumbent untouched |
| Traffic during that conflict | **still served** — the losing route does not fail the reconcile |

The last two rows are the point of settling layer-4 port conflicts in the builder rather than in
the translator: a `TranslatorError` is propagated by `?` out of `reconcile`, so one tenant's second
route would have stopped routing for every other tenant, HTTP included.

The UDP round-trip is worth calling out separately: nothing in this repo had ever exercised Sōzu's
UDP proxy before, so "UDPRoute programs a UDP frontend" and "a datagram comes back" were two
different claims. Both are now measured.

## 6. Gateway API conformance (GATEWAY-HTTP)

The **official** `kubernetes-sigs/gateway-api` conformance suite, `GATEWAY-HTTP` profile, run
against a live cluster (`GatewayClass=sozu`, `rbac.allowStatusWrites=true`).

The profile is **not passing** — and with Sōzu it **cannot** be (see the hard ceiling below), so
the goal is a well-documented **partial**, not the "Conformant" badge.

### Run log

One row per run. Reports under [docs/conformance/](conformance/) are **immutable**: a re-run adds
a file, never edits one, so a row always points at the bytes it describes.

A score is meaningless without its denominator and the suite that produced it — the CRD bundle and
the conformance suite are versioned separately, and `--supported-features` changes what is even
attempted. Record all of it or the next row is as unreadable as a bare "3 → 16 → 12".

| Date | CRD bundle | Suite | Core | Extended | Declared `--supported-features` | Report | Delta vs previous |
| ---- | ---------- | ----- | ---- | -------- | ------------------------------- | ------ | ----------------- |
| 2026-07-02 | v1.2.1 | v1.2.1 | 12 / 33 | 0 / 3 | `HTTPRouteResponseHeaderModification`, `HTTPRouteSchemeRedirect`, `HTTPRouteMethodMatching` | [gateway-http_crd-v1.2.1_2026-07-02.yaml](conformance/gateway-http_crd-v1.2.1_2026-07-02.yaml) | first recorded run at this shape |

### How the score got here

An earlier campaign took core from **3 → 16**: `observedGeneration`, the `Remove*` reconcile wedge,
catch-all `*` routing, invalid-route status reasons, `allowedRoutes.namespaces`, per-listener
status, and cert `ReferenceGrant` denial reporting.

The **16 → 12** that followed is an *honesty correction, not a regression*: the `from: Selector`
fail-closed change removed passes that were artifacts of the old fail-open bug (below), while
`HTTPRouteInvalidCrossNamespaceParentRef` newly passed. Those two campaigns predate this run log
and have no immutable report of their own, which is exactly the gap the log exists to close.

Each re-run keeps paying for itself. This one caught a controller bug in the field: one hung status
write could park the reconcile loop for ~5 minutes (kube's default ~295 s read timeout), starving
every status-polling test. Fixed by giving one-shot API calls a seconds-bounded client.

> **The `Selector` fail-closed blast radius (5 tests).** The suite's shared base infrastructure
> includes a `backend-namespaces` Gateway whose listeners use
> `allowedRoutes.namespaces.from: Selector`. An unevaluable selector fails CLOSED (the listener
> admits no routes, `Programmed: False`), so that base Gateway never reads `Programmed` — which
> fails `HTTPRouteCrossNamespace` and `GatewayWithAttachedRoutes` directly, and gates the *setup*
> of `GatewayModifyListeners`, `GatewayObservedGenerationBump` and
> `HTTPRouteObservedGenerationBump` (the framework waits for every base Gateway before running
> them; `GatewayClassObservedGenerationBump` needs no Gateway and passes). **Note:** the
> hostname/path routing tests (`ListenerHostnameMatching`, `HostnameIntersection`,
> `PathMatchOrder`) still route correctly by hand but fail on a base-setup cert-timing gate.

### Reproduce

```bash
git clone --depth 1 --branch v1.2.1 https://github.com/kubernetes-sigs/gateway-api
cd gateway-api   # raise the suite's client QPS (default 5 flakes on status polling):
# in conformance/conformance.go, after config.GetConfig(): cfg.QPS = 100; cfg.Burst = 200
go test ./conformance -run TestConformance -timeout 120m -args \
  --gateway-class=sozu --conformance-profiles=GATEWAY-HTTP \
  --supported-features=HTTPRouteResponseHeaderModification,HTTPRouteSchemeRedirect,HTTPRouteMethodMatching \
  --organization=clevercloud --project=sozu-gateway \
  --url=https://github.com/CleverCloud/sozu-gateway \
  --contact=https://github.com/CleverCloud/sozu-gateway/issues \
  --version=<version under test> --report-output=report.yaml
```

The gateway must be deployed with `rbac.allowStatusWrites=true` and a `sozu` GatewayClass present.
Name the resulting file `gateway-http_crd-<bundle>_<YYYY-MM-DD>.yaml` and add a row above.

**Hard ceiling — not fixable with Sōzu / one LoadBalancer** (these stay failed):
- **No HTTP 500.** Sōzu's answers are 301/400/401/404/408/413/421/429/502/503/504/507; an invalid
  `backendRef` yields 503, but the spec/tests want exactly 500 → the `HTTPRouteInvalid*BackendRef` /
  `*ReferenceGrant` / `…PartiallyInvalid…` traffic checks.
- **No weighted split** (`HTTPRouteWeight`) and **no header/query-value matching**
  (`HTTPRouteHeaderMatching`, parts of `HTTPRouteMatching`).
- **Header `set` appends instead of replacing.** Gateway `set` must overwrite an existing header,
  but the deployed `clevercloud/sozu:2.2.0` data plane appends — a client sending `X-Env: staging`
  into a route that sets `X-Env: prod` reaches the backend with both — so
  `HTTPRouteRequestHeaderModifier`/`ResponseHeaderModifier` fail. (The *command-lib* documents
  set/replace; the running binary doesn't honour it, so this is a data-plane gap pending a Sōzu
  build that replaces. Re-verified unchanged on the 2.1.0 → 2.2.0 bump.)
- **Catch-all collisions.** Clever Cloud's cluster currently allows **one LoadBalancer**, so all
  Gateways share one Sōzu `:80`/`:443`; two hostname-less routes on the same path collide on key
  `(:8080,*,/path)` (first wins). Per-Gateway addresses would need multiple LBs (unavailable), so
  this is a platform-constrained limit — it drives most of the remaining routing/filter failures
  (`HTTPRouteMatchingAcrossRoutes`, `HTTPRoutePathMatchOrder`, `HostnameIntersection`,
  `ListenerHostnameMatching`, and the extended `RedirectScheme`/`MethodMatching`/header-modifier
  tests). Real users on a shared LB route by hostname (no collision).

**Implementable remaining gaps** (would raise the count):
1. **Evaluate `allowedRoutes.namespaces.from: Selector` for real** — the fail-closed stance exists
   because the controller has no Namespace label index, *not* because of any Sōzu limit. A
   Namespace reflector + label matching would flip Selector to supported and recover the 5
   fail-closed tests above (→ ~17/33). Highest-leverage single item on the board.
2. **Per-Gateway HTTPS listener** (`HTTPRouteHTTPSListener`) — multi-listener HTTPS with SNI on the
   shared `:443`; intertwined with the catch-all-collision limit above.

(`GatewaySecret{Invalid,Missing}ReferenceGrant` now pass — cert `ReferenceGrant` denial reports
`RefNotPermitted`, with the grant `group` checked too.)

Reproduce (the handoff notes previously referenced here lived in the gitignored `.scratch/`):

```bash
git clone --depth 1 --branch v1.2.1 https://github.com/kubernetes-sigs/gateway-api
cd gateway-api   # raise the suite's client QPS (default 5 flakes on status polling):
# in conformance/conformance.go, after config.GetConfig(): cfg.QPS = 100; cfg.Burst = 200
go test ./conformance -run TestConformance -timeout 120m -args \
  --gateway-class=sozu --conformance-profiles=GATEWAY-HTTP \
  --supported-features=HTTPRouteResponseHeaderModification,HTTPRouteSchemeRedirect,HTTPRouteMethodMatching \
  --organization=clevercloud --project=sozu-gateway \
  --url=https://github.com/CleverCloud/sozu-gateway \
  --contact=https://github.com/CleverCloud/sozu-gateway/issues \
  --version=<version under test> --report-output=report.yaml
```

The gateway must be deployed with `rbac.allowStatusWrites=true` and a `sozu` GatewayClass present.

## Reproduce

```sh
just e2e            # section 1: Ingress + TLS — install + demo app + HTTP/HTTPS + hot removal
just e2e-gateway    # sections 4–5: Gateway API routing + header/redirect filters
just e2e-l4         # raw TCP (L4) via the deprecated tcp-services ConfigMap
just e2e-l4-routes  # section 5b: TCPRoute + UDPRoute through the Gateway API
just e2e-all        # all four, sharing one freshly-built image
```

Each suite builds + pushes the controller image to the anonymous `ttl.sh` registry by default (no
credentials needed) and installs the add-on on the current kube-context; the scripts live under
[scripts/](../scripts/).

The load/churn harnesses used for sections 2–3 live under `.scratch/` (developer scaffolding, not
shipped): `hot-reload-test2.sh` (config hot reload) and `dataplane-upgrade-test.sh` (Sōzu Pod
replacement).

## Restart handling

Both halves of the Pod can restart independently, and each case is handled — but
not identically, and only one of the two is gap-free.

| Case | Behaviour |
| ---- | --------- |
| Controller restarts, Sōzu stays up | Gap-free. The persisted shadow is reloaded (`resumed shadow from persisted state`) and **zero** requests are re-applied — the baseline is real, so orphans are still pruned. |
| Sōzu restarts, controller stays up | Detected and repaired, but **not instantly**. Sōzu comes back holding no routes while the in-memory shadow still claims everything is applied, so the diff stays empty and requests 404 until the controller notices. |

Detection of the second case is **polled, not pushed**: the worker-generation
probe runs on the periodic resync (`SOZU_GW_RESYNC_SECS`, 60 s by default) and
when the command socket reconnects. A Sōzu main-process crash under a live
controller therefore leaves the data plane unprogrammed for up to one resync
period. For gap-free serving across a data-plane restart, run
`replicaCount >= 2` so another Pod keeps answering, and/or lower
`SOZU_GW_RESYNC_SECS`. `SOZU_GW_RESYNC_SECS=0` disables the poll entirely and
leaves only the reconnect path.

What the probe compares is Sōzu's live worker-PID set, not whether its state
looks empty; the reasoning is in the doc comment on `check_restart_generation`
in [crates/controller/src/shadow.rs](../crates/controller/src/shadow.rs).

Observed on a **single worker bounce** — which the probe deliberately treats as
a restart, and which is the cheap way to exercise the path (a worker bounce
leaves the main process holding its state, which is why the agent's
duplicate-add repair fires; a real restart would have nothing to collide with):

```
WARN sozu_gw_controller::shadow: Sōzu's worker generation changed (restarted?);
     resetting the shadow to re-apply the full state baseline=Some({7, 8}) current=Some({8, 33})
INFO sozu_gw_controller: applying changes to sozu … requests=16
WARN sozu_gw_agent: sozu already holds this frontend's route key; repairing with a remove + re-add
```

Traffic was uninterrupted for that bounce. It is *not* evidence that a full
Sōzu restart is gap-free — see the resync window above.

## Known limitations

- Installing the Gateway API CRDs under a running controller takes effect on the next resync tick
  (`SOZU_GW_RESYNC_SECS`, 60 s by default), not immediately: the controller re-probes, then exits so
  the restarted process can wire the watches. With `SOZU_GW_RESYNC_SECS=0` there is no re-probe, and
  the Deployment has to be restarted by hand.
