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

## 5c. Rewrite and redirect targets — a measurement, not a feature

Two things this project reported as unsupported rested on the proto's doc comments plus one
result taken against an older Sōzu. Both were re-measured against **Sōzu 2.2.0** with
`crates/sozu-agent/examples/rewrite_redirect_probe.rs`, which programs its own frontends over
the command socket and drives raw HTTP against its own in-process echo backend. The full table
is in [PROTOCOL.md §13](../PROTOCOL.md); the verdicts:

| Question | Answer |
| -------- | ------ |
| `URLRewrite` with `ReplaceFullPath` (`rewrite_path` alone) | **Works.** Backend sees the rewritten path, `Host` untouched, `200` |
| `URLRewrite` with `hostname` (`rewrite_host`) | **Works.** Only the forwarded `Host` changes; the proxy still dials the cluster's configured backend |
| The recorded `408` under a literal rewrite | **Does not reproduce on 2.2.0.** It was taken against 2.1.0 |
| `RequestRedirect` host/path target under `PERMANENT` | `Location: https://new/new` |
| … under `FOUND` (Gateway API's **default** 302) | `Location: https://new/new` — undocumented in the proto, works |
| … under `PERMANENT_REDIRECT` (308) | `Location: https://new/new` |
| `RequestRedirect` `port` target (`rewrite_port`) | `Location: https://new:8443/new` |
| A literal `$` in a rewrite value | **`AddHttpFrontend` is rejected.** Translation is all-or-nothing, so one such route fails *every* reconcile |
| Query string across a path rewrite | **Dropped** — Gateway API's `ReplaceFullPath` keeps it |
| `ReplacePrefixMatch` via `$PATH[1]` | Yields `/` — the compiled prefix regex's only capture group is the boundary `(/\|?\|$)`, not the remainder |

**No behaviour changed in this pass, by design.** The deliverable is the measurement. What it
buys is that three "not supported" rows in [features.md](features.md) were wrong about *why*:
`URLRewrite` and redirect host/path/port targets are not Sōzu limits, they are unwired — with
two conditions any wiring must meet first (refuse or escape `$`; decide what to do about the
dropped query string, which is a spec deviation rather than a detail). `ReplacePrefixMatch`
stays a genuine limit, and now with a measured reason.

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
| 2026-08-04 | v1.6.1 | v1.6.1 | **did not run** | did not run | same three | [gateway-http_crd-v1.6.1_2026-08-04_setup-blocked.md](conformance/gateway-http_crd-v1.6.1_2026-08-04_setup-blocked.md) | the suite aborts in setup — see below. Nothing to compare |
| 2026-08-04 | v1.6.1 | v1.6.1 | 17 / 37 | 0 / 3 | same three | [gateway-http_crd-v1.6.1_2026-08-04_selector-excluded.yaml](conformance/gateway-http_crd-v1.6.1_2026-08-04_selector-excluded.yaml) | **conditioned run** — the `Selector` base Gateway carries `gateway-api/skip-this-for-readiness`, without which nothing runs at all. **Not comparable to row 1**: different suite, denominator 33 → 37, and a base object excluded |
| 2026-08-04 | v1.6.1 | v1.6.1 | 17 / 37 | 0 / 3 | same three | [gateway-http_crd-v1.6.1_2026-08-04_master-control.yaml](conformance/gateway-http_crd-v1.6.1_2026-08-04_master-control.yaml) | **control run** on `master` (c68a72d, i.e. E0–E6), same conditioning as row 3. Identical result *and identical failed-test set* → E7–E11 moved no test either way |

> **A conditioned row is not a score.** Rows 3 and 4 exist to produce a per-test picture, not a
> number to quote. Quoting `17/37` without "with a base Gateway excluded from readiness" would be exactly the
> kind of decontextualised figure this log was created to stop.

### v1.6.1: `Selector` stopped costing 5 tests and started costing all of them

The single most important result of this pass, and it changes a roadmap priority rather than a
number.

The suite's shared base manifests include a `backend-namespaces` Gateway whose listener uses
`allowedRoutes.namespaces.from: Selector`. This controller has no Namespace label index, so it
fails that listener **closed** — `Programmed: False` — which is the honest stance and the reason
row 1's score dropped from 16 to 12.

On the **v1.6.1** suite, `NamespacesMustBeReady`
(`conformance/utils/kubernetes/helpers.go`) requires **every** Gateway in the conformance
namespaces to be `Programmed: True`, and the suite calls it during *setup*. One Gateway we fail
closed therefore aborts the entire profile before a single test runs: 2 530 polling lines, all
naming that one object, then `context deadline exceeded`.

So the arithmetic in the roadmap — "recovering the 5 `Selector` tests takes 12/33 to ~17/33, still
a failure, so do it for correctness rather than for the scoreboard" — was right about the *reason*
and is now wrong about the *stakes*. Evaluating `Selector` is no longer the highest-leverage item
on the board; it is the **precondition for measuring anything at all** on a current suite.

Row 3 confirms the documented blast radius to the letter. Excluding that one Gateway from the
readiness gate flips exactly the three tests the analysis said were gated on its *setup* —
`GatewayModifyListeners`, `GatewayObservedGenerationBump`, `HTTPRouteObservedGenerationBump` — and
leaves failing the two that genuinely need it to work, `HTTPRouteCrossNamespace` and
`GatewayWithAttachedRoutes`.

The rest of the 12 → 17 delta is the suite growing: v1.6.1 adds four core tests to GATEWAY-HTTP
(33 → 37), of which two pass and two fail (`HTTPRouteMultipleGateways`, `HTTPRouteNoBackendRefs`).
Nothing in the E7–E11 work moved a test either way — row 4 is `master` (E0–E6) under the same
conditioning, and it returns not just the same counts but the same twenty failed test names. That
is a measurement, not an inference: the whole point of a second pass is that "our changes did
nothing here" has to be shown rather than assumed.

### Why the two passes are not the two the roadmap asked for

The plan called for one run just after the API-version bump and one after the port-model change, so
that a delta could be attributed to one or the other. Both had already merged by the time this ran,
which makes that split unrecoverable — the honest thing is to say so rather than present a single
number as if it had been decomposed. The two boundaries that still existed were taken instead:
the last recorded v1.2.1 run against the v1.6.1 suite (rows 1 → 3, the API/suite delta), and
`master` against this stack on the *same* suite (the control run, isolating E7–E11).

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
git clone --depth 1 --branch v1.6.1 https://github.com/kubernetes-sigs/gateway-api
cd gateway-api   # raise the suite's client QPS (default 5 flakes on status polling):
# in conformance/conformance.go, after config.GetConfig(): cfg.QPS = 100; cfg.Burst = 200
cd conformance   # from v1.6 the suite is its own Go module inside a workspace,
                 # so `go test ./conformance` from the root no longer resolves it
go test . -run TestConformance -timeout 150m -args \
  --gateway-class=sozu --conformance-profiles=GATEWAY-HTTP \
  --supported-features=HTTPRouteResponseHeaderModification,HTTPRouteSchemeRedirect,HTTPRouteMethodMatching \
  --organization=clevercloud --project=sozu-gateway \
  --url=https://github.com/CleverCloud/sozu-gateway \
  --contact=https://github.com/CleverCloud/sozu-gateway/issues \
  --version=<version under test> --report-output=report.yaml
```

The gateway must be deployed with `rbac.allowStatusWrites=true` and a `sozu` GatewayClass present.
Name the resulting file `gateway-http_crd-<bundle>_<YYYY-MM-DD>.yaml` and add a row above.

**It will abort in setup** until `allowedRoutes.namespaces.from: Selector` is evaluated for real
(see above). To get a per-test picture anyway, keep the base Gateway out of the readiness gate
while the run starts — and record the row as *conditioned*, never as a score:

```bash
# alongside the run, until the suite is past setup
until kubectl -n gateway-conformance-infra get gateway backend-namespaces >/dev/null 2>&1; do sleep 3; done
while kubectl -n gateway-conformance-infra annotate gateway backend-namespaces \
  gateway-api/skip-this-for-readiness=true --overwrite >/dev/null 2>&1; do sleep 3; done
```

Two practical notes: the first run on a cold node times out in setup on **image pulls** — v1.6.1's
base manifests add grpc/tls/tcp/coredns backends — so re-run once the images are cached; and
`go test` without `-v` prints nothing until the package finishes, which looks like a hang for the
~24 minutes a full run takes.

### `GatewayClass.status.supportedFeatures`

Published, and **empty**. That is a result, not a stub.

`FeatureName` is an upstream, boolean-per-feature vocabulary, and the conformance tooling
cross-checks what an implementation declares. At the last recorded run no feature's tests pass
cleanly — extended is 0/3 on the three the runs have always declared via `--supported-features` —
so naming any of them would publish a claim in the one machine-readable channel that exists for
honesty, and one the next run contradicts immediately.

Publishing the field empty rather than leaving it absent is deliberate: "we claim nothing" is a
statement, and it makes the first genuine entry visible as a change. The list lives in one constant
in [`controller/src/status.rs`](../crates/controller/src/status.rs) whose rule is that an entry is
added only when a **row in the log above** shows its tests passing — never because the code looks
like it implements the feature. `write_gatewayclass` compares the published list as well as the
conditions, because a list changed by a version bump travels with conditions that did not move, and
a conditions-only guard would compute the new list and never write it.

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
   Namespace reflector + label matching would flip Selector to supported. On the v1.2.1 suite that
   was worth 5 tests; on **v1.6.1 it is worth the entire run**, which aborts in setup without it
   (see above). No longer "highest-leverage item" — it is the precondition for measuring at all.
2. **Per-Gateway HTTPS listener** (`HTTPRouteHTTPSListener`) — multi-listener HTTPS with SNI on the
   shared `:443`; intertwined with the catch-all-collision limit above.

(`GatewaySecret{Invalid,Missing}ReferenceGrant` now pass — cert `ReferenceGrant` denial reports
`RefNotPermitted`, with the grant `group` checked too.)

## Reproduce

```sh
just e2e            # section 1: Ingress + TLS — install + demo app + HTTP/HTTPS + hot removal
just e2e-gateway    # sections 4–5: Gateway API routing + header/redirect filters
just e2e-l4-routes  # section 5b: TCPRoute + UDPRoute through the Gateway API
just e2e-all        # all three, sharing one freshly-built image
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
