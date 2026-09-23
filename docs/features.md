# Feature support

What the controller does and does not do today (Phase 1 Ingress + TLS, Phase 2 Gateway
API, Phase 3 HTTPRoute filters). It distinguishes what Sōzu
**fundamentally cannot do** from what is simply **not wired up yet**, so a hard constraint is never
mistaken for a roadmap item.

Legend: ✅ supported · 🟡 planned · ❌ not supported.

| Area | Feature | Status | Notes |
| ---- | ------- | :----: | ----- |
| Ingress | IngressClass selection (`spec.ingressClassName`) | ✅ | |
| Ingress | Legacy `kubernetes.io/ingress.class` annotation | ✅ | |
| Ingress | Default IngressClass (`is-default-class`) | ✅ | reconciles class-less Ingresses |
| Ingress | Host match — exact | ✅ | a host Sōzu cannot parse (it fails IDNA/UTS #46 processing, e.g. `xn--a.example.com`, although the apiserver accepts it) is reported (`InvalidHostname`) and its rule skipped |
| Ingress | Host match — wildcard (`*.example.com`) | ✅ | one extra label |
| Ingress | `pathType: Prefix` | ✅ | |
| Ingress | `pathType: Exact` | ✅ | compiled to an anchored whole-path regex, query string allowed (Sōzu's `Equals` misses `/get?x=1` and cannot be removed once added) |
| Ingress | `pathType: ImplementationSpecific` | ✅ | mapped to a Sōzu regex, verbatim — Sōzu 2.2.1 does **not** anchor regexes and its next release anchors them at both ends, so write a full-span pattern; the apiserver requires the path to start with `/`, which `/{0}^/api(?:[/?](?-u:.*))?$` satisfies while still anchoring the start; a pattern Sōzu cannot compile is reported (`InvalidPathRegex`) and that path is skipped |
| Ingress | Multiple Ingresses / hosts / paths | ✅ | de-duplicated by the key Sōzu stores a route under (`address;host;rule[;method]`, so a regex `/x;GET` and `/x` + `GET` contest one key); a contested key is won by the oldest claimant (`creationTimestamp`, then `namespace/name`), Ingress and HTTPRoute alike, and the loser is reported with `RouteCollision` |
| Ingress | Rule without a host (catch-all) | ✅ | one plain-HTTP `*` frontend (Sōzu `DomainRule::Any`), emitted in `POST` position so it never shadows a specific-host route. No HTTPS frontend: a `*` is not covered by any certificate, so the host stays plain HTTP |
| Ingress | `spec.defaultBackend` | ❌ | not routed; reported as a `DefaultBackendUnsupported` problem |
| Ingress | `backend.resource` (non-Service backend) | ❌ | only Service backends |
| TLS | Termination from a `Secret` (`tls.crt`/`tls.key`) | ✅ | `type: kubernetes.io/tls` Secrets only (the controller watches nothing else); works with cert-manager-issued Secrets. Each TLS entry must list `hosts` — a hostless entry is reported (`TlsEntryWithoutHosts`) and skipped. A `tls.key` that does not load (PKCS#8/PKCS#1/SEC1; RSA, ECDSA P-256/P-384, Ed25519) or does not match `tls.crt` is reported (`InvalidCertificate`) and the Secret is skipped |
| TLS | SNI host selection | ✅ | handled by Sōzu |
| TLS | Wildcard certificate | ✅ | |
| TLS | Zero-gap certificate rotation | ✅ | `ReplaceCertificate` |
| TLS | HTTP → HTTPS redirect | ✅ | automatic for TLS-enabled Ingress hosts (301); opt out with `sozu.io/ssl-redirect: "false"` |
| Routing | Backends = pod IPs from EndpointSlice | ✅ | never the Service ClusterIP; `addressType: IPv4`/`IPv6` only — an FQDN slice is reported (`FqdnEndpointsUnsupported`) and ignored |
| Routing | Multi-port Service (match by port name) | ✅ | |
| Routing | Ready-endpoint filtering | ✅ | excludes not-ready endpoints |
| Routing | Hot reload — no proxy restart | ✅ | see [E2E-RESULTS.md](E2E-RESULTS.md) |
| Routing | Idempotent reconcile + periodic resync | ✅ | |
| Routing | Load-balancing algorithm selection | ✅ | Service annotation `sozu.io/load-balancing` (round-robin/random/least-loaded/power-of-two) |
| Routing | Sticky sessions | ✅ | Service annotation `sozu.io/sticky-sessions: "true"`; the `SOZUBALANCEID` cookie holds an opaque per-backend id |
| Routing | Per-endpoint weights | 🟡 | IR + translator support it; no standard K8s per-endpoint weight to map from |
| API gateway | Request/response header edits | ✅ | via HTTPRoute `RequestHeaderModifier`/`ResponseHeaderModifier`: `set` replaces, `add` appends, `remove` deletes; empty `set`/`add` values are rejected |
| API gateway | URL rewrite — `ReplaceFullPath` / `hostname` | 🟡 | **measured expressible** on Sōzu 2.2.0 ([E2E-RESULTS §5c](E2E-RESULTS.md)), not wired: reported as `FilterUnsupported`. Wiring it must first refuse a literal `$` (Sōzu rejects the frontend outright) and answer for the query string, which a path rewrite drops |
| API gateway | URL rewrite — `ReplacePrefixMatch` | ❌ | the compiled prefix regex's only capture group is the element boundary, so `$PATH[1]` yields `/`, not the remainder — measured |
| API gateway | Redirects — scheme + status | ✅ | `RequestRedirect`, 301/302/308 (303 and 307 have no Sōzu policy and are refused) |
| API gateway | Redirects — hostname / path / port target | ✅ | `hostname`, `path.replaceFullPath` and `port`, under 301/302/308. An unset target keeps the request's own value, and the query string is preserved. Refused, with the reason: `path.replacePrefixMatch`, a literal `$` (Sōzu reads it as a rewrite template and rejects the frontend), a redirect that changes nothing, and combining with `URLRewrite` |
| API gateway | HTTP Basic auth | 🟡 | Sōzu Cluster field; not wired (no core Gateway filter) |
| API gateway | Connection limit per source IP | ✅ | Service annotation `sozu.io/max-connections-per-ip` (a connection cap, not an RPS quota); chart-wide default `sozu.maxConnectionsPerIp`, `0` (unlimited). Counted per Service port and per worker, on the client IP the Service preserves; its `429` rejections appear in `/metrics` only with `metrics.perCluster: true` |
| API gateway | Match on header value / query param | ❌ | not supported by Sōzu |
| API gateway | Weighted split across multiple Services | ❌ | not supported by Sōzu |
| API gateway | Request mirroring / shadowing | ❌ | not supported by Sōzu |
| Gateway API | `GatewayClass` (by `controllerName`) | ✅ | status `Accepted` reported |
| Gateway API | `Gateway.spec.infrastructure.parametersRef` | ❌ | no parameter kinds are supported; an explicit reference rejects the Gateway with `Accepted: False` / `InvalidParameters`, without programming its routes or certificates |
| Gateway API | `Gateway` HTTP/HTTPS listeners | ✅ | must declare a port the chart's `exposure` table advertises for that protocol (default `80`/`443`); a mismatch is rejected with `PortUnavailable`. A Gateway with no accepted listeners reports `Accepted: False` / `ListenersNotValid`; a mix of accepted and rejected listeners reports `Accepted: True` / `ListenersNotValid`. An unresolved certificate affects `Programmed` and `ResolvedRefs`, without rejecting the Gateway. A listener `hostname` Sōzu cannot parse (IDNA) is rejected with `UnsupportedValue` and reported (`InvalidHostname`) |
| Gateway API | `HTTPRoute` (host, path, method) | ✅ | status `Accepted`/`ResolvedRefs` per parent. A `RegularExpression` match Sōzu cannot compile is dropped and reported (`InvalidPathRegex`, a Warning Event), like an unsupported header/query match; the rule's other matches still program and the route stays `Accepted`. A hostname Sōzu cannot parse (IDNA) is dropped the same way (`InvalidHostname`). A route whose hostnames intersect none of the listener's is `Accepted: False` / `NoMatchingListenerHostname` and does not count toward `attachedRoutes` — it is attached to nothing |
| Gateway API | HTTPRoute collision precedence | ✅ | among emitted frontends sharing one Sōzu route key (the `;`-joined listener, host, rule and method string): oldest creation timestamp, then alphabetical `namespace/name`, then first rule; two different matches of one route that share the key report the second as `RouteCollision`. Skipped rules do not reserve a match. See [mixed Ingress collisions](UPGRADING.md#httproute-collision-precedence) |
| Gateway API | Separate Gateway addresses and routing tables | ✅ | `gatewayProvisioning.enabled` automatically provisions a Deployment and Service per owned Gateway; the default instance serves Ingress. Local routing controllers remain per Pod. See [UPGRADING](UPGRADING.md#automatic-gateway-instances) |
| Gateway API | `ReferenceGrant` (cross-namespace refs) | ✅ | gates cross-ns backend/cert refs |
| Gateway API | `allowedRoutes.namespaces` — `from: All`/`Same` | ✅ | |
| Gateway API | `allowedRoutes.namespaces` — `from: Selector` | ✅ | evaluated against Namespace labels (`matchLabels` + `matchExpressions`, ANDed; an empty selector matches every namespace). `Selector` **replaces** `Same`: the Gateway's own namespace is admitted only if its labels match. A selector this build cannot evaluate — an unknown `operator`, a malformed expression, `from: Selector` with no selector — still fails closed and is reported (`NamespaceSelectorInvalid`) |
| Gateway API | One Service `backendRef` per rule | ✅ | a single ref with `weight: 0` (drain) is rejected (`ZeroWeightBackendUnsupported`): Sōzu cannot express the spec's all-zero-weight 500 |
| Gateway API | Weighted multi-`backendRef` split | ❌ | not supported by Sōzu |
| Gateway API | Header/query matches | ❌ | not supported by Sōzu |
| Gateway API | Rule-level filters (header edit, redirect) | ✅ | see the API-gateway rows above (URLRewrite reported unsupported) |
| Gateway API | Per-`backendRef` filters | ❌ | filters wire onto the frontend, not one backend; reported (`FilterUnsupported`), the rule still routes without them |
| Gateway API | `rule.timeouts` | ❌ | no Sōzu equivalent; reported (`TimeoutsUnsupported`), the rule still routes without the timeout |
| Gateway API | TLS `Passthrough` | ❌ | terminate only |
| Gateway API | `Gateway` TCP/UDP listeners | ✅ | the declared port must be a `TCP`/`UDP` entry of the chart's `exposure` table (only Helm can open a Service port); `owner` may reserve it for one namespace |
| Gateway API | `TCPRoute` / `UDPRoute` | ✅ | one Service `backendRef`; a socket forwards to the oldest route with a resolved backend reference, by `creationTimestamp` then `namespace/name` (`L4RouteConflict` on other claimants); all admitted routes remain `Accepted: True` and count toward the listener's `attachedRoutes` |
| Gateway API | `GRPCRoute` / `TLSRoute` | ❌ | |
| Protocols | HTTP / HTTPS (L7) | ✅ | |
| Protocols | TCP / UDP ingress (L4) | ✅ | `TCPRoute`/`UDPRoute` only (the `tcp/udp-services` ConfigMaps are gone); one port → one Service, no host routing; ports > 1024 (unprivileged), and never 443 — see below |
| Operations | Exposure via `Service type=LoadBalancer` | ✅ | |
| Operations | Connection capacity | ✅ | Sōzu's buffer pool (`sozu.maxBuffers`, default 20000 per worker) covers its 10,000-connection limit for HTTP/1 and TCP; an HTTP/2 connection needs one buffer plus two per allocated stream slot (finished slots are kept for reuse). Buffer-pool memory up to `maxBuffers × 16 KiB` per worker, committed only as connections arrive; TLS and other connection state need headroom on top |
| Operations | Structured logs (`tracing`) | ✅ | |
| Operations | Prometheus `/metrics` (controller + proxy-wide Sōzu series) | ✅ | pulled over the command socket on each scrape; `metrics.enabled` |
| Operations | Per-Service / per-backend Sōzu metrics | ✅ | opt-in only, `metrics.perCluster`: on Sōzu 2.2.1 a scrape can wedge the workers of a large gateway, see [Upgrading](UPGRADING.md#metrics-exports-proxy-wide-sōzu-series-only) |
| Operations | Gateway API status write-back (loop-safe) | ✅ | Accepted/Programmed/ResolvedRefs |
| Operations | Ingress `status` write-back (loadBalancer) | ✅ | publishes the gateway LB address; enable with `rbac.allowStatusWrites` |
| Operations | Dedicated `/healthz` readiness gate | ✅ | `/readyz` goes green only after the first reconcile, and drops again if Sōzu restarts with no routes until the re-apply succeeds, so a Pod is in the Service only while it can actually serve |

## Notes

- **Gateway certificate name inference.** An HTTPS listener without `hostname` uses its
  certificate's DNS SANs, falling back to the CN only when no DNS SAN exists. Names are merged
  with explicit listener hostnames only after validation. Empty inference, malformed SNI
  patterns, regex syntax, non-ASCII names, and names longer than Sōzu's 4096-byte limit leave
  that listener `ResolvedRefs: False` with reason `InvalidCertificateRef`. Exact names,
  leading `*.` wildcards and the certificate wildcard `*` remain supported. A rejected
  listener does not change a shared certificate already serving explicit hostnames.
  This fixes initial certificate installation. Sōzu 2.2.1 does not apply SNI name
  changes to an already loaded certificate with the same fingerprint; roll the gateway
  Pods to apply such changes until [#81](https://github.com/CleverCloud/sozu-gateway/issues/81)
  provides native support. See [Upgrading](UPGRADING.md#gateway-certificate-name-inference).
- **Regex paths (`ImplementationSpecific`, HTTPRoute `RegularExpression`).** The pattern is
  handed to Sōzu verbatim, and what Sōzu does with it is about to change: 2.2.1 does **not**
  anchor regexes (measured, see PROTOCOL.md — `/api` also matches `/x/api`), while the next
  release wraps every rule in `\A(?:…)\z`, so `/api` will match **only** `/api` and, since the
  query string is part of the target, not `/api?x=1`. Write patterns that mean the same under
  both: anchor both ends yourself and spell out the tail — `^/api(?:[/?](?-u:.*))?$` for an
  element-boundary prefix, `^/api(?:\?(?-u:.*))?$` for an exact path with a query. A
  suffix-shaped pattern such as `\.js$` needs a leading `(?-u:.*)` to survive the change. The
  tail is `(?-u:.*)` rather than `.*` because Sōzu compiles with `regex::bytes`, whose Unicode
  `.` skips a byte that is not valid UTF-8 — a remainder its tolerant HTTP/1 parser can admit —
  so a plain `.*` would read differently before and after. **An Ingress path must start with
  `/`** (apiserver validation, every `pathType`); a regex still anchors its start behind a slash
  matched zero times, `/{0}^/api(?:[/?](?-u:.*))?$`, which the apiserver accepts and Sōzu
  compiles. An HTTPRoute `RegularExpression` value has no such constraint. A pattern that
  spells the same rule as a `Prefix`/`Exact` path on the same host is one route to Sōzu and is
  arbitrated as a collision; one that merely overlaps it is a second route, and which of the two
  Sōzu 2.2.1 applies is the order they were added — undefined here, do not lean on it. Sōzu
  appends the method to the complete pattern with a bare `;`, so a pattern ending in `;GET`
  shares the key of that pattern *without* the `;GET` suffix restricted to `GET` (`/x;GET`
  against `/x` + `GET`) and the two are arbitrated as a collision; write a literal `;` as
  `[;]` to avoid that.
- **API-gateway filters.** Header edits and redirects (scheme + status) are exposed through the IR
  and Gateway API HTTPRoute filters (Phase 3). A Gateway `set` deletes existing occurrences before
  appending its value; `add` appends and `remove` deletes, on requests and responses. Empty `set`
  or `add` values cannot be distinguished from deletion in Sōzu's protocol: the affected rule is
  skipped with `Accepted=False`, reason `UnsupportedValue`, and a `FilterUnsupported` problem.
  Other rules keep routing. Redirect host/path/port targets are **wired** — measured working on
  Sōzu 2.2.0 under every policy (see [E2E-RESULTS §5c](E2E-RESULTS.md) and [PROTOCOL.md §13](../PROTOCOL.md)),
  with a literal `$` refused in the builder because Sōzu reads it as a rewrite template and
  rejects the frontend outright, which an all-or-nothing translation turns into a failed
  reconcile for everyone. `URLRewrite` stays unwired for one measured reason: on the *forwarding*
  path a rewrite **drops the query string** that `ReplaceFullPath` keeps. (A redirect does not —
  the two share Sōzu's fields but not that behaviour.)
  The per-source-IP connection limit is wired through Service annotations (see below). HTTP Basic
  auth exists in Sōzu's data plane but has no core Gateway API filter, so it remains unwired.
- **Hard limits.** Matching on header values or query parameters, weighted traffic split across
  several Services, and request mirroring are not expressible in Sōzu today, so they are out of
  scope rather than merely deferred.

## Annotations

Cluster-level routing is tuned with annotations on the backing **Service** (a cluster is 1:1 with a
Service, so both an Ingress and a Gateway route to that Service share one configuration):

| Annotation | Values | Default | Effect |
| ---------- | ------ | ------- | ------ |
| `sozu.io/load-balancing` | `round-robin`, `random`, `least-loaded`, `power-of-two` | `round-robin` | Sōzu load-balancing algorithm for the cluster. Unknown values fall back to the default. |
| `sozu.io/sticky-sessions` | `"true"` / `"false"` | `"false"` | Pin a client to one backend via Sōzu's `SOZUBALANCEID` cookie. Its value is an opaque id, the same on every gateway replica; a client whose backend is gone is load-balanced and re-pinned. Two sticky Services on different paths of one host share the cookie (`Path=/`) and can overwrite each other's pin. |
| `sozu.io/max-connections-per-ip` | integer | `sozu.maxConnectionsPerIp` (`0`, unlimited) | Cap simultaneous connections from one source IP to each port of this Service. Over the cap → `429`. A non-numeric value is ignored. Rejections are counted per Service port: they show in `/metrics` only with `metrics.perCluster: true`, or in the access logs. |
| `sozu.io/retry-after` | integer (seconds) | `60` (Sōzu's default) | `Retry-After` header sent on that `429`; `0` omits it. |

One annotation is read from the **Ingress** instead (it depends on that Ingress's TLS, not the Service):

| Annotation | Values | Default | Effect |
| ---------- | ------ | ------- | ------ |
| `sozu.io/ssl-redirect` | `"true"` / `"false"` | `"true"` | Redirect HTTP→HTTPS (`301`) for hosts that have a loaded cert. Auto-on; set `"false"` to keep serving plain HTTP. (Gateway API uses an explicit `RequestRedirect` filter instead.) |

## HTTP and HTTPS exposure

The chart accepts multiple HTTP and HTTPS entries in `exposure`. Each Gateway
listener selects its advertised `(protocol, port)` and gets the corresponding
bind; HTTPS certificate stores remain separate per bind. The first entry of each
protocol serves Ingress routes. Keep these entries first when adding ports.

Sōzu creates these listeners at boot. Updating the table rolls the gateway Pods
and interrupts connections that do not finish draining before termination.

## L4 (TCP/UDP)

UDP flow identity includes the client IP and source port; clients sharing an IP
keep separate reply destinations.

Raw TCP/UDP forwarding is a `TCPRoute` or a `UDPRoute` on a layer-4 `Gateway`
listener. There is no host multiplexing at layer 4: one port forwards to exactly
one Service.

Declare the port in the chart's `exposure` table — only Helm can open a port on
the Service — then point a `Gateway` listener and a route at it:

```yaml
exposure:                        # values.yaml
  - { name: http,     port: 80,   bind: 8080, protocol: HTTP,  transport: TCP }
  - { name: https,    port: 443,  bind: 8443, protocol: HTTPS, transport: TCP }
  - { name: postgres, port: 5432, bind: 5432, protocol: TCP,   transport: TCP, owner: demo }
```

```yaml
listeners:                       # Gateway
  - { name: postgres, protocol: TCP, port: 5432 }
---
rules:                           # TCPRoute
  - backendRefs: [{ name: postgres, port: 5432 }]
```

A full example is in [`examples/api-gateway/l4-routes.yaml`](../examples/api-gateway/l4-routes.yaml).

- A socket carries exactly one route. Two routes claiming it are settled by
  oldest `creationTimestamp`, then `namespace/name`; the loser reports
  the winning route and port in its `Accepted` condition message and an
  `L4RouteConflict` Warning Event. All admitted routes remain `Accepted: True`
  and count toward the listener's `attachedRoutes`, including those whose
  backends are unresolved. A route counts once per listener even when several
  parentRefs select it. The conflict does not fail the reconcile.
- Only routes whose backend reference resolves to a Service and port contend
  for the socket. If the oldest route's Service disappears, a younger route
  with a resolved backend can receive traffic. A Service with no ready endpoints
  still holds its route's place. Reserve shared L4 ports with `owner` when
  separate namespaces must not compete for them.
- The exposure entry's optional `owner` names the only namespace whose Gateways
  may declare that port. Down here there is no hostname to arbitrate with, so
  the alternative would be a race.
- Weighted splits and `weight: 0` drains are refused exactly as they are for
  HTTPRoute — Sōzu cannot express either.
- **Ports must be > 1024, and 443 is impossible.** Both containers run as uid
  1000 with every capability dropped, and the Service already publishes 443/TCP
  for `https` — a Service cannot expose one `(port, protocol)` twice. The chart
  fails the render with that explanation rather than letting the apiserver
  reject it obscurely. Layer-4 traffic therefore lives on a port TLS clients do
  not dial by default.

The cluster + backends resolve to pod IPs exactly like HTTP, so hot reload and
pruning work the same way.

> **Removed in this release:** the `tcp/udp-services` ConfigMaps. Layer-4
> routing now requires the Gateway API CRDs — see [UPGRADING.md](UPGRADING.md)
> for why, and how to migrate.
