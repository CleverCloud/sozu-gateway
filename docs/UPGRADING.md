# Upgrading

Breaking changes, what they cost, and what to do about them. Newest first.

---

## Separate Gateway instances

`gatewayInstances` can assign a Gateway to its own controller, Sōzu Pods and
Service. The default instance excludes those Gateways automatically and remains
the only instance that handles Ingress and GatewayClass status. The empty list
preserves the existing Deployment identity and selectors.

```yaml
gatewayInstances:
  - name: public
    gateway:
      namespace: apps
      name: public
    replicaCount: 2
    service:
      type: LoadBalancer
```

Each entry provisions a separate Deployment, Service, ConfigMap, disruption
budget and enabled metrics resources. Images, exposure, resource settings and
the ServiceAccount are shared settings; each Pod has its own command socket
and persisted shadow. `service` overrides and `replicaCount` are optional. Each
provided Service field replaces that field from the release, including maps:
`service.annotations: {}` explicitly removes inherited annotations.
Use a LoadBalancer Service for a public Gateway address. ClusterIP Services can
publish their internal addresses for in-cluster clients; a pending LoadBalancer
never publishes its ClusterIP as an external address. A configured publish
LoadBalancer or ClusterIP Service without an assigned address keeps the Gateway
`Programmed=False` with `AddressNotAssigned`, even if the local listeners are
ready. NodePort and ExternalName Services, missing publish Services and
deployments without a publish Service retain listener-based programming status;
node address publication is not implemented.

Instance Pods use `app.kubernetes.io/instance: <release>_gateway` together with
`sozu.io/gateway-instance: <instance>`. User-managed NetworkPolicies and monitors
that select only the default release label need selectors for these Pods too.
Route status parents owned by this controller are sorted consistently across
instances, which can cause a one-time reorder when upgrading.

Creating, removing or retargeting an entry moves the Gateway to a different
address. Allow for LoadBalancer provisioning and update DNS or clients; this
handoff is not atomic. Until the new address is assigned, existing status
addresses remain visible: ownership scope alone cannot identify which prior
Service published them. Consumers must check `Programmed` during migration.
The assigned address replaces the previous list. Gateway and instance names must be unique within the
release. Overlong names fail rendering instead of being truncated into another
instance's resource or selector. Service names also reserve the `-metrics`
suffix, so an instance cannot claim another instance's metrics Service.

Outside Helm, `--gateway-scope namespace/name` owns one Gateway and ignores
Ingress; the default controller must receive matching `--exclude-gateway`
arguments. Keep that ownership partition disjoint. All instances may use the
same `controllerName`: route status retains other instances' parent entries,
including their complete references, preserves the relative order of entries
from other controllers, and retries conflicting updates against
the latest resourceVersion. A route losing its last local parent also has its
stale status removed. The shadow format is unchanged.

When using the Helm topology with Ingress status RBAC disabled, the controller
still publishes its Service for Gateway addresses and disables Ingress status
writes explicitly. The standalone equivalent is `--ingress-status-writes=false`.

---

## Sōzu 2.2.1

The chart now defaults to `clevercloud/sozu:2.2.1` (was `2.2.0`). The controller's
`sozu-command-lib` was already pinned to `2.2.1` and stays there. The protobuf
schema, channel framing and routing diff are unchanged between these versions;
no controller configuration or persisted-shadow migration is needed.

[Upstream 2.2.1](https://github.com/sozu-proxy/sozu/releases/tag/2.2.1) fixes
frontend validation and command failure handling, and redacts sensitive debug
and error output. Existing header and rewrite limitations are unchanged.

Changing the image rolls the gateway Pods. Keep at least two replicas during
the rollout. If your values pin `image.sozu.tag`, or you use `--reuse-values`,
set `--set image.sozu.tag=2.2.1` to adopt the new image.

See the [upgrade validation](E2E-RESULTS.md#sozu-221-upgrade-validation-2026-09-10)
for the measured results and coverage limits.

---

## The data plane defaults to two replicas

`replicaCount` was `3`. It is now `2`. Nothing else changes: the same hard
hostname spread, the same `maxUnavailable: 1` budget.

Three was chosen so that losing a Pod never empties the Service. Two does that
already, and the difference was measured rather than assumed. Forcing the loss of
one gateway Pod — which is what a rolling node replacement does, one worker at a
time — cost a single replica **4.8%** and **8.0%** of fresh connections in two
trials, and cost two and three replicas **nothing**, on the same cluster with the
same probes. The third replica earned its keep only when a *second* loss
overlapped the first: two Pods destroyed at once cost the two-replica deployment
3.2% while the three-replica one stayed clean.

So the third replica buys tolerance of a concurrent second failure, not
protection against ordinary maintenance. It is worth keeping if you want that
margin, and worth its capacity to say so:

```yaml
replicaCount: 3
```

Two replicas are also the safer default on small clusters, where the hard
hostname spread has fewer domains to work with.

**Nothing needs doing on upgrade** unless you set `replicaCount` yourself, in
which case your value still wins. A release that adopts the new default rolls one
Pod out; the budget and the drain keep that gap-free.

---

## Sōzu is drained on shutdown, and the grace period grows to 40s

The data-plane container gets a `preStop` hook and
`terminationGracePeriodSeconds: 40` (was Kubernetes' default 30).

Sōzu registers no signal handler, and a signal with no handler is discarded when
it reaches PID 1 of a container — so SIGTERM did nothing and every Pod deletion
sat out the full grace period before being SIGKILLed, losing the response to
whatever was in flight. Measured on a three-node cluster: **31–32s to delete a
gateway Pod before, 6–7s after**, the difference being the 5s delay plus about a
second of draining.

**The first upgrade is not drained.** The outgoing Pod is the old spec, so it
still waits out its own grace period; only Pods created from this chart onwards
drain.

**`--reuse-values` needs care.** Helm replays the previous release's values over
the new chart and does not pick up keys the chart has since added. The chart
defaults `sozu.drain` internally so this no longer fails, but
`--reset-then-reuse-values` is the flag that actually gives you the new defaults.

**Sizing under `externalTrafficPolicy: Local`** — the chart's default. The 5s
delay covers kube-proxy and the CNI, not an external load balancer, which stops
sending only once its own health check fails (often 10–30s). Before this change
Sōzu kept accepting for the whole 30s grace and covered that window by accident;
now the listener closes at 5s and connections arriving after that are refused.
If you depend on an external LB, raise `sozu.drain.delaySeconds` to at least its
depool time.

**What is drained** is HTTP/1 exchanges in progress and HTTP/2 streams. TCPRoute
and UDPRoute sessions, WebSockets, TLS handshakes in progress and idle
keep-alives are cut once the delay elapses.

Set `sozu.drain.enabled: false` to opt out entirely; the grace period then
returns to Kubernetes' default too.
## Prometheus metrics are served by default

`metrics.enabled` now defaults to `true`. A `helm upgrade` with unchanged values
therefore adds a `ClusterIP` Service, opens a container port, and rolls the Pod.

**What it exposes.** The endpoint has no authentication and no NetworkPolicy, and
the series carry `cluster_id` as `namespace.service.port` — so any Pod that can
reach the Service can enumerate the routed Services of every tenant. Backend Pod
IPs are not exposed at Sōzu's default detail level. On a shared cluster, put a
NetworkPolicy in front of it or turn it off:

```yaml
metrics:
  enabled: false
```

**What it costs.** A scrape is a `QueryMetrics` on the same command socket that
carries routing applies, and that socket is served by a single worker in order.
A slow scrape can therefore delay — and in the worst case fail — a reconcile.
Two known limits, neither of which the chart can fix on its own:

- the scraper stops waiting after 10s and returns `503`, but the query it started
  keeps the socket busy for longer than that;
- an `AggregatedMetrics` payload larger than `max_command_buffer_size`
  (1 638 400 bytes) is rejected by Sōzu, which closes the connection instead of
  answering. Large clusters will see scrapes fail deterministically.

**What it is good for.** The controller's own signals — the
last-successful-reconcile timestamp above all — plus Sōzu's request counters and
status codes. Note what it is *not* good for: `cluster.available_backends`
against `cluster.total_backends` only moves when Sōzu's passive circuit breaker
trips, and that is armed by a *refused* connection. A backend that has gone
silent is still counted available, so those two series do not detect it.
## The backend connect timeout drops to 2 seconds

The chart used to set no timeouts, so Sōzu's own applied — including **3 seconds
to connect to a backend**. It now renders `connect_timeout = 2`.

A `helm upgrade` picks this up like any other config change, including with
`--reuse-values`, because the value ships in the chart rather than in your
overrides. **A backend that legitimately takes longer than 2 seconds to accept a
connection — a cold runtime, a saturated accept queue — starts being answered
`504` where it previously waited.**

The change is there so that a backend which has gone *silent* fails inside the
proxy, where it is answered, logged as `backend_timeout` and counted in
`http.status.504`, instead of consuming the caller's entire budget and being
recorded on the client as an unexplained hang. It buys attribution, not
recovery: a connect that times out gets no retry and no failover.

If it cuts off a backend that was merely slow, raise it:

```yaml
sozu:
  timeouts:
    connect: 5   # or null, to return to Sōzu's own default of 3
```

This reaches the HTTP and HTTPS listeners only. Layer-4 listeners are created by
the controller over the command socket and keep their own fixed budgets, so a
TCPRoute or UDPRoute is unaffected either way.

---

## The data plane moved from one replica to three, one per node

`replicaCount` was `1`. It is now `3`, spread hard across nodes
(`kubernetes.io/hostname`, `maxSkew: 1`, `whenUnsatisfiable: DoNotSchedule`), and
a PodDisruptionBudget with `maxUnavailable: 1` is rendered alongside — it appears
only above one replica, since over a single Pod a budget can only block every
disruption or permit every one of them.

A single replica has no one to fail over to: while its Pod is rescheduled the
Service holds no endpoint at all, so callers get a connection error rather than a
slower answer. A cluster upgrade triggers exactly that, by design, because the
platform drains and replaces every worker in turn.

Spreading hard rather than preferring is what makes that hold: a preference
measurably does not — a soft constraint put two replicas on one node and left
another empty on every rollout of a three-node cluster, and nothing moves a
running Pod afterwards.

**An earlier version of this note claimed that with fewer nodes than replicas the
surplus Pods stay `Pending` and `helm upgrade --wait` never completes. That is
wrong.** `maxSkew: 1` permits an uneven-by-one distribution; it does not demand a
node per Pod. Tested directly: five replicas on a four-node cluster all reached
`Running`, spread 2,1,1,1. A Pod that does stay `Pending` under this constraint is
short of a node for another reason — taints, resources — not short of skew.

Lowering the count on a smaller cluster is still reasonable, for capacity rather
than for schedulability:

```yaml
replicaCount: 2
```

or keep the count and relax the placement, accepting that replicas may share a
node — which is the wrong shape behind `externalTrafficPolicy: Local`, where a
Pod on every node is what matters rather than a count of Pods:

```yaml
topologySpreadConstraints:
  - maxSkew: 1
    topologyKey: kubernetes.io/hostname
    whenUnsatisfiable: ScheduleAnyway
```

Both defaults rely on `matchLabelKeys`, honoured from Kubernetes 1.27 and a hard
rejection under strict field validation before it, so the chart now declares
`kubeVersion: ">=1.27.0-0"` rather than leaving that to be discovered.

This buys survival of a Pod or a node going away. It does **not** make a cluster
upgrade lossless on its own — see the reconcile-loop limitation in
[CLAUDE.md](../CLAUDE.md): a controller whose watches stop delivering keeps
serving the routes it last saw, and every replica is then equally stale.

---

## Watch blindness is bounded by default

`controller.watchTimeoutSecs` now defaults to `60`, where the controller
previously inherited kube-rs's own bound. A `helm upgrade` with unchanged values
therefore rolls the Pod and changes how the controller talks to the apiserver.

**What it fixes.** A control plane replaced underneath the controller does not
close the connections its watches ride on: nothing errors, no event arrives, and
the reflectors stop advancing while every reconcile keeps *succeeding* against a
frozen cache. Sōzu then serves routes for backends that moved minutes ago, with
`reconcile_failures_total` at `0` and `/readyz` green. Measured across two
upgrades of a managed cluster, **17.1% and 14.7% of fresh HTTP requests through
the gateway failed** for the duration of node replacement — every one an HTTP
`504` from Sōzu against a withdrawn backend, while bare TCP, DNS and the
application's own Service stayed clean. With this setting the same two upgrades
measured `0.000%`.

**What it costs.** One watch reconnect per watch per minute. The watcher resumes
from the stored `resourceVersion` rather than re-listing, so a quiet cluster pays
one request per watched kind per minute and nothing else. Set it back to `0` to
restore the previous behaviour:

```yaml
controller:
  watchTimeoutSecs: 0
```

**What the number means.** `60` is `timeoutSeconds` on every watch, which is also
what kube-rs derives its client-side idle timeout from. Both are nominal: cutting
controllers off from the apiserver and timing each to its first logged watch
error gave **336–364s at the default and 117–132s at 60**. Idle expiry logs only
at `DEBUG`, so those figures include the failed reconnect that follows it, and
why they exceed their nominal bound is not established — only that they do. Size
any staleness alert on the measured figures.

**What it does not fix.** `/readyz` still latches green on a controller that has
gone blind, and no `sozu_gw_controller_*` series moves while it is blind. The
setting bounds how long that lasts; it does not make it visible.

**A restart is not a substitute.** v0.3.0 suggested rolling the gateway once the
control plane had been replaced. That was measured on the next upgrade and does
not work: an arm whose three controllers were restarted on the provider's
control-plane step — confirmed restarted, one Pod at a time — still lost
**16.53%**, indistinguishable from the untreated control, because the fresh
watches attach to an apiserver that is itself about to be withdrawn. Restarting
is only safe once the *new* apiserver is serving, which the provider's step event
does not tell you.

## Downgrading the controller

Upgrades need nothing special. **Downgrades do**, and this is the procedure.

The controller persists its last-applied state to `/run/sozu/shadow.json` as a
bare `Ir` with no version field. Forward compatibility is covered — every field
added since is `#[serde(default)]`, and a frozen fixture test
(`crates/controller/tests/fixtures/shadow-v0.2.json`) keeps it that way, so a
new controller reads an old file.

Backwards is the hard direction, and it cannot be fixed by defaulting: an older
build has no variant for an enum value a newer one wrote. `RedirectStatus`
gained `PermanentRedirect`, so a shadow holding it is unreadable to anything
older. serde fails the **whole** parse, not the field, and
[`shadow.rs`](../crates/controller/src/shadow.rs) then starts from an empty
`Ir`.

That is not a crash, which is the problem: diffing from empty produces only
*adds*. Clusters and backends upsert, duplicate frontends are repaired — but
anything in Sōzu that the desired state no longer wants is never removed,
because nothing in the diff asks for it. You get **orphaned routes serving
traffic nobody declared**, and the only trace is one line:

```
WARN persisted shadow is unreadable; will re-apply
```

### The procedure

**Restart the Sōzu container as part of the downgrade.** That is the operative
step: an empty Sōzu makes the controller ignore the persisted shadow outright
(it probes `save_state` first) and re-apply the full desired state against a
clean data plane, so there is nothing left to orphan.

```sh
# roll the whole Pod — both containers share the socket and the volume anyway
kubectl -n sozu-system rollout restart deploy/sozu-gateway
```

Delete `/run/sozu/shadow.json` too if the volume outlives the Pod. It is not
what fixes the orphans, but it stops the next start from parsing a file it
cannot read and logging a warning that no longer means anything.

**Do not** downgrade the controller alone and leave Sōzu running. That is
exactly the case where the shadow is unreadable *and* the data plane is
non-empty, which is the orphan path above.

---

## The `tcp/udp-services` ConfigMaps are removed

**What went away**

| Removed | Replacement |
| ------- | ----------- |
| Helm `l4.tcpServices` / `l4.udpServices` | a `TCPRoute` / `UDPRoute` |
| `--tcp-services-configmap` / `--udp-services-configmap` (`SOZU_GW_TCP_SERVICES`, `SOZU_GW_UDP_SERVICES`) | — |
| the rendered `*-tcp-services` / `*-udp-services` ConfigMaps and their namespaced `Role`/`RoleBinding` | — |

A `helm upgrade` that still sets `l4.tcpServices` or `l4.udpServices` **fails**
rather than silently dropping the routes: unknown values are ignored by Helm, so
the chart checks for them explicitly.

**Read this before upgrading**

Layer-4 routing now requires the **Gateway API CRDs** (v1.6.1 standard channel,
which is where `tcproutes`/`udproutes` live from v1.6 on). On a cluster without
them, the controller runs in Ingress-only mode and there is no way to forward a
TCP or UDP port at all. That is a functional regression, deliberately taken:
the ConfigMap path was cluster-global with no admission control whatsoever —
anyone able to edit the map routed any port to any Service in any namespace,
with no ReferenceGrant and no Gateway to consent — and keeping a second,
weaker way to do the same thing is exactly the kind of quiet approximation this
project refuses.

If you need layer 4 and cannot install the Gateway API CRDs, stay on the
previous release.

**Migrating**

A mapping like

```yaml
l4:
  tcpServices:
    "5432": "demo/postgres:5432"
```

becomes an exposure entry (only Helm can open a port on the Service) plus a
route. The exposure entry was already required in the previous release, so this
half is likely done:

```yaml
# values.yaml
exposure:
  - { name: http,     port: 80,   bind: 8080, protocol: HTTP,  transport: TCP }
  - { name: https,    port: 443,  bind: 8443, protocol: HTTPS, transport: TCP }
  - { name: postgres, port: 5432, bind: 5432, protocol: TCP,   transport: TCP, owner: demo }
```

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: { name: l4, namespace: demo }
spec:
  gatewayClassName: sozu
  listeners:
    - { name: postgres, protocol: TCP, port: 5432 }
---
apiVersion: gateway.networking.k8s.io/v1
kind: TCPRoute
metadata: { name: postgres, namespace: demo }
spec:
  parentRefs: [{ name: l4, sectionName: postgres }]
  rules:
    - backendRefs: [{ name: postgres, port: 5432 }]
```

The optional `owner` on the exposure entry restricts which namespace's Gateways
may claim that port — the map had no equivalent, so adding it is the point.

Apply the route **before** removing the values: where both named one socket, the
route already won, so there is no gap.

A full worked example is in
[examples/api-gateway/l4-routes.yaml](../examples/api-gateway/l4-routes.yaml).

**Problems that no longer exist**

`InvalidL4Mapping`, `L4PortDuplicate`, `L4PortNotExposed`, `L4PortNotOwned` and
`L4PortClaimedByRoute` are gone with the path that raised them. Their
equivalents on the route path are `PortNotExposed` and `ListenerPortNotOwned`
(on the Gateway listener) and `L4RouteConflict` (on the losing route) — and
unlike a ConfigMap entry, every one of them lands on an object you can
`kubectl describe`.

**Also**

`scripts/e2e-l4.sh` and `examples/ingress/l4-tcp.yaml` are removed;
`scripts/e2e-l4-routes.sh` (`just e2e-l4-routes`) covers layer 4.
