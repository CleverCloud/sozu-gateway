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
| Ingress | Host match — exact | ✅ | the `Host` header is matched case-insensitively (Sōzu ≥ 2.3.0; a trailing-dot `Host` matches no host). A request whose path or method no rule of the exact host matches falls through to a covering `*.` wildcard host, then to the hostname-less catch-all. A host Sōzu cannot parse (it fails IDNA/UTS #46 processing, e.g. `xn--a.example.com`, although the apiserver accepts it) is reported (`InvalidHostname`) and its rule skipped |
| Ingress | Host match — wildcard (`*.example.com`) | ✅ | exactly one extra label ([#82](https://github.com/CleverCloud/sozu-gateway/issues/82)); also serves an exact host's path/method misses (Sōzu ≥ 2.3.0), then falls through to the hostname-less catch-all |
| Ingress | `pathType: Prefix` | ✅ | |
| Ingress | `pathType: Exact` | ✅ | compiled to an anchored whole-path regex, query string allowed (Sōzu's `Equals` misses `/get?x=1` and cannot be removed once added) |
| Ingress | `pathType: ImplementationSpecific` | ✅ | mapped to a Sōzu regex, verbatim — Sōzu ≥ 2.3.0 anchors it at both ends (`\A(?:…)\z`) against the whole request target, query string included, so a substring or suffix pattern no longer matches: write a full-span pattern; the apiserver requires the path to start with `/`, which `/{0}^/api(?:[/?](?-u:.*))?$` satisfies while still anchoring the start; a pattern Sōzu cannot compile, bare or anchored, is reported (`InvalidPathRegex`) and that path is skipped |
| Ingress | Multiple Ingresses / hosts / paths | ✅ | de-duplicated by the key Sōzu stores a route under (`address;host;rule[;method]`, so a regex `/x;GET` and `/x` + `GET` contest one key); a contested key is won by the oldest claimant (`creationTimestamp`, then `namespace/name`), Ingress and HTTPRoute alike, and the loser is reported with `RouteCollision` |
| Ingress | Rule without a host (catch-all) | ✅ | one plain-HTTP `*` frontend (Sōzu `DomainRule::Any`), emitted in `POST` position so it never shadows a specific-host route. No HTTPS frontend: a `*` is not covered by any certificate, so the host stays plain HTTP. Among hostless paths Sōzu takes the **first rule added** that matches, not the most specific, and a re-forked worker re-adds them in key order, so overlapping hostless paths (`/` and `/api`) can route to the wrong one (pre-existing, [#80](https://github.com/CleverCloud/sozu-gateway/issues/80)) |
| Ingress | `spec.defaultBackend` | ❌ | not routed; reported as a `DefaultBackendUnsupported` problem |
| Ingress | `backend.resource` (non-Service backend) | ❌ | only Service backends |
| TLS | Termination from a `Secret` (`tls.crt`/`tls.key`) | ✅ | `type: kubernetes.io/tls` Secrets only (the controller watches nothing else); works with cert-manager-issued Secrets. Each TLS entry must list `hosts` — a hostless entry is reported (`TlsEntryWithoutHosts`) and skipped. A `tls.key` that does not load (PKCS#8/PKCS#1/SEC1; RSA, ECDSA P-256/P-384, Ed25519) is reported (`InvalidCertificate`) and the Secret is skipped. So is a pair Sōzu 2.3.2 refuses: a leaf rustls-webpki cannot parse as an end-entity certificate (e.g. X509v1, an unknown critical extension, over 64 KiB), or whose public key is not byte-for-byte the one derived from `tls.key` (a mismatched key, a compressed EC point). Its hosts then get no HTTPS frontend and no HTTP→HTTPS redirect; plain HTTP keeps routing |
| TLS | SNI host selection | ✅ | handled by Sōzu |
| TLS | One certificate owner per SNI name | ✅ | when objects at one HTTPS listener name the same host with different certificates (Ingress `tls[].hosts`, Gateway listener `hostname` or inferred names), the oldest claimant's certificate keeps it, whichever expires last. Age is the claiming object's (`creationTimestamp`, then `namespace/name`, as for route keys), not the claim's: adding a host to an older object wins it. The name is dropped from the others' certificates (one left with no name is not loaded); each certificate that lost names is reported once with `CertificateNameClaimed` (a Warning Event; on a Gateway also in the listener's `Programmed` message). Copies of one certificate are no conflict. Applied live from Sōzu 2.3.2 — see [Upgrading](UPGRADING.md#a-contested-sni-name-is-served-by-its-oldest-claimants-certificate) |
| TLS | Wildcard vs exact certificate names across objects | ❌ | not arbitrated: `*.example.com` and `app.example.com` are different names, and Sōzu serves the exact one, so a newer object's `app.example.com` certificate wins that host over an older `*.example.com` owner |
| TLS | Wildcard certificate | ✅ | |
| TLS | Zero-gap certificate rotation | ✅ | `ReplaceCertificate` |
| TLS | Intermediate chain changes on an unchanged leaf | ✅ | a same-fingerprint `ReplaceCertificate` carrying the new chain, applied in place by Sōzu 2.3.2; Secrets sharing a leaf with different chains serve the one with the most intermediates, whatever order they are read in and whichever of them keeps SNI names |
| TLS | SNI name changes on an unchanged certificate | ✅ | a same-fingerprint `ReplaceCertificate`, applied in place by Sōzu 2.3.2 — no Pod roll ([#81](https://github.com/CleverCloud/sozu-gateway/issues/81)) |
| TLS | Client-certificate validation (mTLS; Gateway `spec.tls.frontend`) | ❌ | not wired. Sōzu 2.3 has a client-auth policy per HTTPS listener; the first HTTPS `exposure` entry is shared by every Ingress and Gateway on the instance, while a further, Gateway-only entry is its own listener, so per-port enforcement there is representable later. Today such a Gateway stays `Accepted`/`Programmed` and its HTTPS listener serves **every client, with no client authentication**; only an informational `GatewaySpecUnsupported` (`spec.tls`) is reported |
| TLS | TLS to backends (`BackendTLSPolicy`, Gateway `spec.tls.backend`) | ❌ | Sōzu has no TLS towards backends ([sozu#1218](https://github.com/sozu-proxy/sozu/issues/1218)); backends are plain pod IP:port |
| TLS | HTTP → HTTPS redirect | ✅ | automatic for TLS-enabled Ingress hosts (301); opt out with `sozu.io/ssl-redirect: "false"` |
| Routing | Backends = pod IPs from EndpointSlice | ✅ | never the Service ClusterIP; `addressType: IPv4`/`IPv6` only — an FQDN slice is reported (`FqdnEndpointsUnsupported`) and ignored |
| Routing | Multi-port Service (match by port name) | ✅ | an EndpointSlice port is matched to the Service port by name, `""` meaning none; a slice whose only port has no name is also used for a Service port of the same protocol, and one named for another port never is |
| Routing | Backend protocol from the Service port's `appProtocol` (GEP-1911) | ✅ | HTTP routes only (Ingress and HTTPRoute). `kubernetes.io/h2c` makes Sōzu speak HTTP/2 with prior knowledge to the pods (`Cluster.http2`), for HTTP/2 and HTTP/1.1 clients alike — clients of a pod that does not speak h2c then get `503` ([measured](probes/h2c-backend_sozu-2.3.2_2026-10-19.txt)); `kubernetes.io/wss` is reported and still sent HTTP/1.1 (next row); every other value, `grpc`, `http2`, `https` and `kubernetes.io/ws` included, keeps HTTP/1.1. A port number the Service exposes over both UDP and TCP resolves to the TCP entry, and each entry is its own cluster. TCPRoute/UDPRoute ignore it. Needs Sōzu 2.3, see [Upgrading](UPGRADING.md#a-service-port-declaring-kubernetesioh2c-gets-http2) |
| Routing | TLS to backends (`appProtocol: kubernetes.io/wss`, `https`) | ❌ | not supported by Sōzu. An HTTPRoute to a `kubernetes.io/wss` port reads `ResolvedRefs: False` / `UnsupportedProtocol`, and it and an Ingress get an `UnsupportedAppProtocol` Warning Event; both are still programmed as before, sending cleartext HTTP/1.1, because dropping the rule's match would let another rule answer it. An empty cluster's `503` is not used either: other invalid `backendRef`s drop their match today ([#78](https://github.com/CleverCloud/sozu-gateway/issues/78) tracks an error answer). `https` is neither honoured nor refused |
| Routing | WebSocket to a port declaring `kubernetes.io/h2c` | ❌ | the HTTP/1.1 `Upgrade` does not cross the HTTP/2 backend connection: a pod serving both protocols answers the handshake as a plain `GET`, and the client gets `200` instead of `101`, with no error ([measured](probes/h2c-backend_sozu-2.3.2_2026-10-19.txt)). Serve WebSockets from a port without it |
| Routing | Ready-endpoint filtering | ✅ | excludes not-ready endpoints |
| Routing | Hot reload — no proxy restart | ✅ | see [E2E-RESULTS.md](E2E-RESULTS.md) |
| Routing | Idempotent reconcile + periodic resync | ✅ | |
| Routing | Load-balancing algorithm selection | ✅ | Service annotation `sozu.io/load-balancing` (round-robin/random/least-loaded/power-of-two); `power-of-two` spreads across every backend since Sōzu 2.3.0 (2.2.1 used two of them) |
| Routing | Sticky sessions | ✅ | Service annotation `sozu.io/sticky-sessions: "true"`; the `SOZUBALANCEID` cookie holds an opaque per-backend id. A client is re-pinned when its backend is gone **or** after a single connect timeout to it (Sōzu 2.3: the backend enters back-off) |
| Routing | Per-endpoint weights | 🟡 | IR + translator support it; no standard K8s per-endpoint weight to map from. Sōzu 2.3.2 honours a backend weight under every policy, a weight of `0` counting as `1` (`random` reads the raw value) |
| Routing | Hostless catch-all (`*`) answering for another object's host | ❌ | not arbitrated: two catch-alls on one path contest one route key and are settled like any other (`RouteCollision`), but a catch-all also serves another object's hostname on every path that object does not route |
| Routing | Active health checks (TCP connect) | ✅ | implementation-specific and opt-in: chart `sozu.healthCheck.mode: "tcp"` for every Service, or the `sozu.io/health-check` annotations per Service (below). A TCP check proves the port accepts a connection, not that the application is alive. Each Sōzu worker probes every backend, so the probe rate is about replicas × workers × backends / interval (Sōzu adds up to a fifth of the interval as jitter); a backend speaking another protocol may log every probe (MariaDB: `Aborted connection`). A backend that stops answering is left out of rotation — 6.3 to 9.4 s later at the default timing in the [recorded runs](probes/health-check_sozu-2.3.2_2026-10-19.txt) — and its sticky clients are re-pinned — except when no backend passes normal selection (health or connect back-off): Sōzu then fails open, ignoring health for every backend it is not backing off from, so a down backend can take traffic while a healthy one is backing off. Only on a Service port a programmed Ingress, HTTPRoute or TCPRoute actually routes to: a port whose only route lost its route key or its socket, or routes nothing, gets no probes. Never on a UDP cluster: a Service port routed by a UDPRoute and by a TCPRoute, Ingress or HTTPRoute is one cluster, and loses the check with `HealthCheckNotApplied`, a Warning Event on each of those objects and on the UDPRoute. Needs Sōzu ≥ 2.3.0 (`sozu.io/health-check-port` ≥ 2.3.2) |
| Routing | HTTP-mode health checks | 🟡 | Sōzu 2.3 has them (path, accepted statuses); not wired: they overlap kubelet readiness, and a wrong path marks every backend down, which Sōzu answers by failing open to them anyway |
| API gateway | Request/response header edits | ✅ | via HTTPRoute `RequestHeaderModifier`/`ResponseHeaderModifier`: `set` replaces, `add` appends, `remove` deletes; empty `set`/`add` values are rejected |
| API gateway | URL rewrite — `ReplaceFullPath` / `hostname` | 🟡 | not wired: reported as `FilterUnsupported`, and the rule is served **without** the rewrite. The path is **measured expressible** (Sōzu 2.2.0, re-measured on 2.2.1 and 2.3.2, [E2E-RESULTS §5c](E2E-RESULTS.md)); wiring it must refuse a literal `$` (Sōzu rejects the frontend outright) and capture the query string, which a literal path rewrite drops. `hostname` is blocked upstream: Sōzu's `rewrite_host` sends an HTTP/1 backend two `Host` lines, which a Go backend answers `400` (measured on 2.2.1 and 2.3.2) |
| API gateway | URL rewrite — `ReplacePrefixMatch` | 🟡 | not wired, **measured expressible** on 2.2.1 and 2.3.2 with a capturing spelling of the non-root prefix regex ([PROTOCOL.md §13](../PROTOCOL.md)); on today's capture-less regex `$PATH[1]` gets the frontend rejected. The root `/` must keep Sōzu's plain prefix rule |
| API gateway | Redirects — scheme + status | ✅ | `RequestRedirect`, 301/302/308 (303 and 307 have no Sōzu policy and are refused) |
| API gateway | Redirects — hostname / path / port target | ✅ | `hostname`, `path.replaceFullPath` and `port`, under 301/302/308. An unset target keeps the request's own value. The query string is kept only without a `path` target: `path.replaceFullPath` drops it (measured on 2.2.1 and 2.3.2; a known defect, not fixed yet). Refused, with the reason: `path.replacePrefixMatch`, a literal `$` (Sōzu reads it as a rewrite template and rejects the frontend), a redirect that changes nothing, and combining with `URLRewrite` |
| API gateway | HTTP Basic auth | 🟡 | Sōzu Cluster field; not wired (no core Gateway filter) |
| API gateway | Connection limit per source IP | ✅ | Service annotation `sozu.io/max-connections-per-ip` (a connection cap, not an RPS quota); chart-wide default `sozu.maxConnectionsPerIp`, `0` (unlimited). Counted per Service port and per worker, on the client IP the Service preserves; its `429` rejections appear in `/metrics` only with `metrics.perCluster: true`. Never applied to a UDPRoute: Sōzu 2.3 would turn it into a silent per-source flow cap counting every client port, so the controller sends none on a UDP cluster |
| API gateway | Match on header value | 🟡 | Sōzu 2.3.2 has header predicates on HTTP(S) frontends; not wired yet (reported, that match skipped) |
| API gateway | Match on query param | ❌ | not supported by Sōzu |
| API gateway | Weighted split across multiple Services | ❌ | not a weight problem (weights are honoured; since Sōzu 2.3.2 by every policy): Sōzu selects a backend per backend connection, not per request, so a keep-alive client stays on one Service, and it has no native `500` for the spec's invalid-backend share ([#79](https://github.com/CleverCloud/sozu-gateway/issues/79), [#78](https://github.com/CleverCloud/sozu-gateway/issues/78)) |
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
| Gateway API | Weighted multi-`backendRef` split | ❌ | per-connection backend selection under connection reuse, and no native `500` ([#79](https://github.com/CleverCloud/sozu-gateway/issues/79), [#78](https://github.com/CleverCloud/sozu-gateway/issues/78)) — see the API-gateway row |
| Gateway API | Header matches | 🟡 | Sōzu 2.3.2 has header predicates; not wired yet: reported (`HeaderOrQueryMatchUnsupported`) and that match skipped — the rule's other matches still program |
| Gateway API | Query-parameter matches | ❌ | not supported by Sōzu; reported and that match skipped |
| Gateway API | Listener isolation (`GatewayHTTPListenerIsolation`) | ❌ | not supported, and not claimed. Listeners on one Sōzu listener share its route table: a request whose path or method misses on its exact host falls through to a covering `*.` host, then to the hostname-less routes, whichever listeners they came from |
| Gateway API | Wildcard hostnames (`*.example.com`) | 🟡 | exactly one label, as Sōzu matches them ([#82](https://github.com/CleverCloud/sozu-gateway/issues/82)) |
| Gateway API | Rule-level filters (header edit, redirect) | ✅ | see the API-gateway rows above (URLRewrite reported unsupported) |
| Gateway API | Per-`backendRef` filters | ❌ | filters wire onto the frontend, not one backend; reported (`FilterUnsupported`), the rule still routes without them |
| Gateway API | `rule.timeouts` | ❌ | no Sōzu equivalent; reported (`TimeoutsUnsupported`), the rule still routes without the timeout |
| Gateway API | TLS `Passthrough` | ❌ | terminate only |
| Gateway API | `Gateway` TCP/UDP listeners | ✅ | the declared port must be a `TCP`/`UDP` entry of the chart's `exposure` table (only Helm can open a Service port); `owner` may reserve it for one namespace |
| Gateway API | `TCPRoute` / `UDPRoute` | ✅ | one Service `backendRef`; a socket forwards to the oldest route with a resolved backend reference, by `creationTimestamp` then `namespace/name` (`L4RouteConflict` on other claimants); all admitted routes remain `Accepted: True` and count toward the listener's `attachedRoutes` |
| Gateway API | `GRPCRoute` / `TLSRoute` | ❌ | |
| Protocols | HTTP / HTTPS (L7) | ✅ | |
| Protocols | gRPC (HTTPRoute or Ingress to a `kubernetes.io/h2c` port) | ✅ | HTTPS listener only ([measured](probes/grpc-quick-interop_sozu-2.3.2_2026-10-19.txt)). Measured on Sōzu 2.3.2 with grpc-go: a stream with no DATA or HEADERS for [30 s](probes/grpc-streams_sozu-2.3.2_2026-10-19.txt) is reset and HTTP/2 PINGs do not count (send heartbeats, or [raise](probes/grpc-idle-timeouts_sozu-2.3.2_2026-10-19.txt) `sozu.timeouts.back` and `front`, listener-wide); 100 concurrent streams per client connection, Sōzu's default (`lib/src/protocol/mux/h2.rs:173`), more queue on the client ([300 one-second calls on one channel took 3.04 s](probes/grpc-streams_sozu-2.3.2_2026-10-19.txt)); RPCs are [balanced per client connection](probes/grpc-backend-changes_sozu-2.3.2_2026-10-19.txt), not per RPC; a backend's GOAWAY or removal from the endpoints [loses nothing](probes/grpc-backend-changes_sozu-2.3.2_2026-10-19.txt) with ≥ 2 endpoints; Sōzu's `502`/`503`/`504` [read as](probes/grpc-errors_sozu-2.3.2_2026-10-19.txt) `UNAVAILABLE`, `404` as `UNIMPLEMENTED`. Read from Sōzu 2.3.2's source, not measured: a soft stop closes HTTP/2 client connections, open gRPC streams included, 5 s after it starts unless the listener sets `h2_graceful_shutdown_deadline_seconds` (`lib/src/https.rs:1478-1484`). See [Upgrading](UPGRADING.md#a-service-port-declaring-kubernetesioh2c-gets-http2) |
| Protocols | Cleartext HTTP/2 from clients (h2c prior knowledge, `Upgrade: h2c`) | ❌ | not supported by Sōzu, deliberately (2.3.2 `lib/src/http.rs:764-782`): its HTTP listener answers the prior-knowledge preface with `400` ([measured](probes/grpc-cleartext_sozu-2.3.2_2026-10-19.txt)), and never upgrades an `Upgrade: h2c` offer itself — it forwards the header to an HTTP/1.1 backend (`lib/src/http.rs:770-772`). That was measured only with an HTTP/1.1 backend that ignores the offer, which answered over HTTP/1.1; a backend that accepts it was not tried. HTTP/2 to clients is TLS-only (ALPN h2), so gRPC over `http://` fails before routing |
| Protocols | TCP / UDP ingress (L4) | ✅ | `TCPRoute`/`UDPRoute` only (the `tcp/udp-services` ConfigMaps are gone); one port → one Service, no host routing; ports > 1024 (unprivileged), and never 443 — see below |
| Operations | Exposure via `Service type=LoadBalancer` | ✅ | |
| Operations | Connection capacity | ✅ | Sōzu's buffer pool (`sozu.maxBuffers`, default 20000 per worker) covers its 10,000-connection limit for HTTP/1 and TCP; an HTTP/2 connection needs one buffer plus two per allocated stream slot (finished slots are kept for reuse). Buffer-pool memory up to `maxBuffers × 16 KiB` per worker, committed only as connections arrive; TLS and other connection state need headroom on top, as do two Sōzu 2.3 costs outside the pool (read from the source): replay captures, up to about 16.8 MB allocated per worker, and a 16 MiB HTTP/2 connection window that lets a stalled upload hold about 6.25 MiB of kernel receive buffer at 100 streams (about 1 MiB on 2.2.1) |
| Operations | Structured logs (`tracing`) | ✅ | |
| Operations | Prometheus `/metrics` (controller + proxy-wide Sōzu series) | ✅ | pulled over the command socket on each scrape; `metrics.enabled` |
| Operations | Per-Service / per-backend Sōzu metrics | ✅ | opt-in only, `metrics.perCluster`: on Sōzu 2.2.1 and still on 2.3.2 ([measured](probes/metrics-no-clusters_sozu-2.3.2_2026-10-19.txt)) a scrape can wedge the workers of a large gateway, see [Upgrading](UPGRADING.md#metrics-exports-proxy-wide-sōzu-series-only) |
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
  A later name change on an already loaded certificate with the same fingerprint is a
  same-fingerprint `ReplaceCertificate`, which Sōzu 2.3.2 applies in place
  ([#81](https://github.com/CleverCloud/sozu-gateway/issues/81); 2.2.1 ignored it and needed a
  Pod roll). See [Upgrading](UPGRADING.md#gateway-certificate-name-inference).
- **Regex paths (`ImplementationSpecific`, HTTPRoute `RegularExpression`).** The pattern is
  handed to Sōzu verbatim. Sōzu 2.3.0 and later wrap every rule in `\A(?:…)\z` (2.2.1 did
  **not** anchor regexes — measured, see PROTOCOL.md — so `/api` also matched `/x/api`): `/api`
  matches **only** `/api` and, since the query string is part of the target, not `/api?x=1`;
  measured on 2.3.2, `/api/v[0-9]+` no longer matches `/api/v1/x` and `\.js$` matches nothing.
  Anchor both ends yourself and spell out the tail — `^/api(?:[/?](?-u:.*))?$` for an
  element-boundary prefix, `^/api(?:\?(?-u:.*))?$` for an exact path with a query; a
  suffix-shaped pattern needs a leading `(?-u:.*)`, `^(?-u:.*)\.js(?:\?(?-u:.*))?$`. Such a
  pattern means the same on 2.2.1 and 2.3. The tail is `(?-u:.*)` rather than `.*` because Sōzu
  compiles with `regex::bytes`, whose Unicode `.` skips a byte that is not valid UTF-8 — a
  remainder its tolerant HTTP/1 parser can admit. A pattern that compiles bare but not once
  wrapped (an `(?x)` comment swallowing the closing group, nesting within two levels of the
  parser's limit, e.g. 248–249 nested groups) is refused by Sōzu 2.3.2 and reported here
  (`InvalidPathRegex`). **An Ingress path must start with
  `/`** (apiserver validation, every `pathType`); a regex still anchors its start behind a slash
  matched zero times, `/{0}^/api(?:[/?](?-u:.*))?$`, which the apiserver accepts and Sōzu
  compiles. An HTTPRoute `RegularExpression` value has no such constraint. A pattern that
  spells the same rule as a `Prefix`/`Exact` path on the same host is one route to Sōzu and is
  arbitrated as a collision; one that merely overlaps it is a second route, and which of the two
  Sōzu applies is the order they were added — undefined here, do not lean on it. Sōzu
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
  Sōzu 2.2.0 under every policy and re-measured unchanged on 2.2.1 and 2.3.2 (see
  [E2E-RESULTS §5c](E2E-RESULTS.md) and [PROTOCOL.md §13](../PROTOCOL.md)), with a literal `$`
  refused in the builder because Sōzu reads it as a rewrite template and rejects the frontend
  outright, which an all-or-nothing translation turns into a failed reconcile for everyone. A
  literal path target **drops the query string**, in a redirect `Location` as on the forwarding
  path, so a `path.replaceFullPath` redirect loses it today; only a redirect without a path
  target keeps it. That is also why `URLRewrite` path rewriting stays unwired, since
  `ReplaceFullPath` must keep the query; capturing it in the path rule would fix both (measured,
  not wired). `URLRewrite.hostname` is blocked by Sōzu sending two `Host` lines.
  The per-source-IP connection limit is wired through Service annotations (see below). HTTP Basic
  auth exists in Sōzu's data plane but has no core Gateway API filter, so it remains unwired.
- **Hard limits.** Matching on query parameters, a weighted traffic split across several
  Services (weights are honoured — since Sōzu 2.3.2 by every policy — but a backend is selected
  per backend connection rather than per request, and there is no native `500` for an invalid
  backend's share), and request mirroring are not expressible in Sōzu today, so they are out of
  scope rather than merely deferred. Header matching is not one of them: Sōzu 2.3.2 has header
  predicates, but a frontend whose predicates hold outranks every unpredicated frontend of the
  host, whatever its path or method (measured), so a direct mapping misroutes and wiring them
  needs its own design.
- **Listener isolation.** Gateway API asks an implementation without it to say so: this one has
  none. Every Gateway listener on one Sōzu listener (one `exposure` entry) shares its route table,
  and Sōzu 2.3 lets a request whose path or method misses on an exact host fall through to a
  covering `*.` host and then to the hostname-less routes, regardless of which listener each
  came from. `GatewayHTTPListenerIsolation` is not claimed.

## Annotations

Cluster-level routing is tuned with annotations on the backing **Service** (a cluster is 1:1 with a
Service, so both an Ingress and a Gateway route to that Service share one configuration):

| Annotation | Values | Default | Effect |
| ---------- | ------ | ------- | ------ |
| `sozu.io/load-balancing` | `round-robin`, `random`, `least-loaded`, `power-of-two` | `round-robin` | Sōzu load-balancing algorithm for the cluster. Unknown values fall back to the default. |
| `sozu.io/sticky-sessions` | `"true"` / `"false"` | `"false"` | Pin a client to one backend via Sōzu's `SOZUBALANCEID` cookie. Its value is an opaque id, the same on every gateway replica; a client whose backend is gone — or, on Sōzu 2.3, in back-off after a single connect timeout — is load-balanced and re-pinned. Two sticky Services on different paths of one host share the cookie (`Path=/`) and can overwrite each other's pin. |
| `sozu.io/max-connections-per-ip` | integer | `sozu.maxConnectionsPerIp` (`0`, unlimited) | Cap simultaneous connections from one source IP to each port of this Service. Over the cap an HTTP request gets `429` and a TCPRoute session is closed; a UDPRoute gets no cap at all, and neither does any other route to a Service port a UDPRoute also uses. A non-numeric value is ignored. Rejections are counted per Service port: they show in `/metrics` only with `metrics.perCluster: true`, or in the access logs. |
| `sozu.io/retry-after` | integer (seconds) | `60` (Sōzu's default) | `Retry-After` header sent on that `429`; `0` omits it. |
| `sozu.io/health-check` | `tcp`, `off` | `sozu.healthCheck.mode` (`off`) | Active TCP-connect health check of every backend of this Service. |
| `sozu.io/health-check-interval`, `-timeout` | integer (seconds) | `sozu.healthCheck` (`3`, `2`) | Seconds between two probes of a backend, and that a probe may take to connect. Keep the timeout below the interval. |
| `sozu.io/health-check-healthy-threshold`, `-unhealthy-threshold` | integer | `sozu.healthCheck` (`2`, `2`) | Probes in a row that bring a backend back, and that take one out. |
| `sozu.io/health-check-port` | port number | the backend's own port | Port the probe dials on the backend's IP. Needs Sōzu 2.3.2: 2.3.0 and 2.3.1 have no such field (proto field 9), ignore it and probe the data port. |

Unlike the annotations above, which fall back to their default on a value they cannot use, the
`sozu.io/health-check*` annotations **report** one (`InvalidHealthCheck`, a Warning Event on every
object routing to the Service) and leave that Service with no check at all — neither the value
nor the default — because a silently replaced check reads as one that took. The tuning annotations
are read only when the check is on.

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
  HTTPRoute.
- TCP half-close is not supported: a client that shuts down its write side
  after its request gets no reply, the session being closed once the request is
  flushed (measured on [2.2.0 and 2.2.1](probes/tcp-half-close_2026-09-10.md) and
  [2.3.2](probes/tcp-half-close_sozu-2.3.2_2026-10-19.txt); upstream
  [sozu#1840](https://github.com/sozu-proxy/sozu/issues/1840)).
- A connect that times out closes the session, with no failover to another
  backend and no back-off — unlike HTTP on Sōzu 2.3 — so a black-holed backend
  keeps taking its share of new sessions until its endpoint is withdrawn.
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
