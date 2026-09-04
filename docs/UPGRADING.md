# Upgrading

Breaking changes, what they cost, and what to do about them. Newest first.

---

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
