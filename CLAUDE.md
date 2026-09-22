# CLAUDE.md

This is the primary agent instruction file for the repository. It is symlinked to `AGENTS.md`
so other coding agents (e.g. OpenAI Codex) pick up the same guidance.

## What this is

A Kubernetes Ingress controller / API gateway built on the [Sōzu](https://github.com/sozu-proxy/sozu)
reverse proxy. The controller watches Kubernetes objects, compiles them into a neutral
intermediate representation (IR), diffs that IR against the last-applied state, and pushes the
minimal set of mutations to a co-located Sōzu instance over its protobuf **command socket** —
hot, with no proxy restarts. **Phase 1 (Ingress + TLS), Phase 2 (Gateway API) and Phase 3
(HTTPRoute filters: header edits, redirect) are implemented** and validated
end-to-end; see [docs/E2E-RESULTS.md](docs/E2E-RESULTS.md).

## Commands

Tasks are run with [`just`](https://github.com/casey/just) (`just` with no args lists them):

```bash
just build          # cargo build --workspace
just test           # cargo test --workspace (unit + golden/snapshot tests)
just lint           # cargo fmt --check + clippy -D warnings (the CI gate)
just fmt            # cargo fmt (write)
just image          # docker build the controller image
just chart-lint     # helm lint + template (also rbac.allowStatusWrites + metrics/ServiceMonitor)
just e2e            # in-cluster end-to-end (Ingress + TLS) on the current kube-context
just e2e-gateway    # Gateway API + HTTPRoute filters (header/redirect) end-to-end
just e2e-l4-routes  # layer-4 through the Gateway API (TCPRoute + UDPRoute)
just e2e-all        # every e2e suite, sharing one freshly-built image
```

- **`protoc` is required to build** — `sozu-command-lib`'s `build.rs` runs `prost-build`. The
  devcontainer already installs it; on a bare host `apt-get install protobuf-compiler` first.
- **Run a single test:** `cargo test -p sozu-gw-translator <name>` (crates: `sozu-gw-ir`,
  `sozu-gw-translator`, `sozu-gw-builder`, `sozu-gw-agent`, `sozu-gw-controller`).
- **Snapshot tests use [`insta`](https://insta.rs).** Golden snapshots live in
  `crates/*/tests/snapshots/`. After an intentional change to emitted commands, review/accept
  with `cargo insta review` (or `INSTA_UPDATE=always cargo test`). A diff in a `.snap` is a
  behavior change to scrutinize, not a thing to blindly re-bless.
- The [justfile](justfile) is authoritative for task/command names (`image`, `chart-lint`,
  `chart-package`, `e2e`, …); keep the README in sync with it. Override variables before the
  recipe, e.g. `just IMAGE=my/repo TAG=v0.2.0 image`.
- The e2e scripts default to an ephemeral `ttl.sh` image (build + push), so no registry
  credentials are needed — just a working kube-context.

## Architecture

```
K8s objects ─▶ reflector caches ─▶ builder ─▶ IR ─▶ translator ─▶ protobuf cmds ─▶ Sōzu socket
```

Six workspace crates, layered so the pure ones can be unit-tested without kube or a socket. The
**purity boundary is load-bearing** — keep `ir`, `builder`, and `translator` free of any socket I/O;
all socket/kube-client I/O lives in `sozu-agent` and `controller`.

| Crate | Role | I/O |
|---|---|---|
| [`crates/ir`](crates/ir) | neutral structs (`Cluster`/`Backend`/`Frontend`/`Certificate`/`Ir`) | none |
| [`crates/gateway-api`](crates/gateway-api) | Gateway API CRD types, kopium-generated (types only) | none |
| [`crates/builder`](crates/builder) | typed Ingress **+ Gateway API** objects → IR (+ `Problem`s/results) | none |
| [`crates/translator`](crates/translator) | pure IR → Sōzu commands, diff vs last-applied | none |
| [`crates/prometheus`](crates/prometheus) | pure `AggregatedMetrics` → Prometheus text exposition | none |
| [`crates/sozu-agent`](crates/sozu-agent) | wrapper around `sozu-command-lib` (socket, ack loop) | **socket** |
| [`crates/controller`](crates/controller) | kube-rs watch/reconcile loop, wires it together | **kube + socket** |

### How the reconcile loop works (`controller/src/main.rs`)

One **singleton, global** reconcile — not per-object. Reflector caches for Ingress, IngressClass,
Service, EndpointSlice, Secret (field-selected to `type=kubernetes.io/tls`) — and, when the
Gateway API CRDs are present, GatewayClass, Gateway, HTTPRoute, ReferenceGrant — each ping a
single mpsc channel on any change (EndpointSlice pings are pre-filtered to services some route
actually references); a debounced
(`SOZU_GW_DEBOUNCE_MS`) reconcile rebuilds the *entire* desired IR from the caches, diffs it
against an in-memory **shadow** (the last successfully-applied `Ir`), and applies only the delta.
A periodic resync (`SOZU_GW_RESYNC_SECS`) re-runs that rebuild, so it heals drift between the
caches and Sōzu — but **not** between the caches and the apiserver. It re-reads the same
reflectors, so a cache that has stopped receiving events re-diffs to "no change": the resync is
then a no-op that still counts as a successful reconcile.

- The shadow advances **only on a fully successful apply**. On failure it stays put, and
  re-diffing from the unchanged shadow converges — NOT because the requests are idempotent
  (frontend/listener `Add*` verbs reject duplicates with `StateError::Exists`), but because
  `AddCluster`/`AddBackend` upsert and the agent tolerates teardowns of objects Sōzu says it no
  longer holds (only those: a teardown refused for any other reason fails the batch) and *repairs*
  duplicate frontend adds (remove + re-add on the same route key; see `sozu-agent`). A failed
  reconcile is **retried without waiting for a watch event**: the liveness tick re-runs pending
  work on its next successful probe, the resync rebuilds anyway, and with both disabled the loop
  nudges itself after a short delay — a quiet cluster must never keep a failed apply pending.
- **Fail-fast philosophy:** if a watch stream ends or caches don't sync within the timeout, the
  process exits so Kubernetes restarts it rather than silently going blind. Never `panic!`.
  **It does not cover a stream that goes silent without ending.** `watcher` retries internally, so
  a watch that stops delivering surfaces as neither an item nor an error and the reflector just
  stops advancing, and no `sozu_gw_controller_*` signal moves. What bounds it is the watch's own
  idle timeout: the binary defaults `--watch-timeout-secs` to 60 and the chart's
  `controller.watchTimeoutSecs: 60` mirrors it (an explicit `0` opts out and is rendered as such;
  ≥ 295 is refused at startup because kube-rs refuses it on every watch start — see
  [docs/UPGRADING.md](docs/UPGRADING.md)), where the kube-rs default leaves it near five
  minutes. Measured across three managed-cluster upgrades, the unbounded default cost
  **19.7%, 17.1% and 14.7%** of fresh HTTP requests for the duration of node replacement — every
  failure an HTTP 504 from Sōzu against a withdrawn backend, while bare TCP, DNS and the
  application's own Service stayed clean, and `reconcile_failures_total` stayed 0 with `/readyz`
  green throughout. At 60 the same upgrades measured **0.000%**.

  Two traps around this, both measured rather than reasoned:
  **restarting the controller is not a reliable cure** — an arm restarted on the provider's
  control-plane-replaced event, confirmed Pod by Pod, still lost 16.53%, because the fresh watches
  attach to an apiserver that is itself about to be withdrawn; and **blindness alone is free** —
  four controllers cut off from the apiserver for eight minutes with no backend change lost
  nothing at all. It costs traffic only when the world moves while they cannot see it, which is
  why a quiet cluster never shows this and an upgrade always does.
- The shadow is **persisted** to the shared volume (`--shadow-file`, default `/run/sozu/shadow.json`)
  the moment a socket apply succeeds — before the status/Event apiserver calls that follow, which
  must never hold the baseline back — and reloaded at startup, so restarting *only* the controller
  resumes from the real baseline and still prunes orphans. The file carries the **restart
  generation** of the Sōzu it was applied to (command-socket identity + live worker PIDs) and is
  resumed **only if the Sōzu found at startup answers with the same socket identity**: a Sōzu that
  restarted while the controller was down recreated its socket, so the file is ignored and the full
  state re-applied. (An emptiness probe via `save_state` cannot do this: with the static listeners
  a fresh Sōzu already dumps four records, so it never reads "empty".) Mid-life, a restart under a
  live controller is detected the same way (checked on every resync tick, pending reconnect, and
  post-apply reconnect); a changed **socket** resets the shadow, and the file with it, so the next
  reconcile re-applies everything, and **`/readyz` drops for that window** so the Service stops
  sending traffic to a Pod whose Sōzu holds no routes; it re-latches when the re-apply succeeds. An
  idle socket never reconnects, so a **liveness tick** (`--sozu-probe-secs`, default 2, chart
  `controller.sozuProbeSecs`) runs that same generation check on a timer — one `Status` round-trip,
  nothing rebuilt unless it finds a restart — bounding how long a Sōzu that restarted alone stays in
  the Service before the reset fires (without it, only the next watch event or resync notices, up to
  `SOZU_GW_RESYNC_SECS`, never at 0). A
  changed worker set on the *same* socket does **not** reset:
  the main process re-feeds its state to a respawned worker, and diffing `empty → desired` would
  emit only adds, never removing what left the desired state meanwhile. Worker PIDs alone could
  never detect a restart anyway: a restarted container can reuse the same PID set. Socket
  identity must describe the connection that answered the worker probe, not a later pathname
  lookup that could already refer to a replacement socket.
- **The shadow file is `{generation, ir}` and the `ir` half is a bare `Ir` with no version field**, so every new `Ir` field needs `#[serde(default)]`
  — enforced by a frozen fixture (`controller/tests/fixtures/shadow-v0.2.json`), not by convention.
  The reverse direction cannot be defaulted: an older build has no variant for an enum value a
  newer one wrote, serde fails the *whole* parse, and diffing from the resulting empty `Ir` emits
  only adds — so orphans in Sōzu are never pruned. **Downgrading therefore requires restarting
  Sōzu**, see [docs/UPGRADING.md](docs/UPGRADING.md). Adding an `ir` enum variant is a
  downgrade-breaking change and should say so in its commit.

### Translator diff strategy — the subtle part

The translator deliberately uses **two different diff strategies**, and changes here are easy to
get wrong:

- **Routing graph** (clusters/backends/frontends): reuse Sōzu's own `ConfigState::diff` so the
  semantics match the data plane exactly. Certificates are kept *out* of this path.
- **Certificates**: diffed by hand, keyed by `(listener, fingerprint)` — Sōzu's own cert identity.
  This (a) emits `ReplaceCertificate` for zero-gap rotation, and (b) sidesteps a `debug_assert` in
  `sozu-command-lib` 2.1.0 that fires when `ConfigState::diff` removes the last cert at a listener.

All output is reordered into **dependency-safe tiers** (`canonicalize` / `tier()`): adds go
clusters → backends → certificates → frontends; removes in reverse. Frontend *removes* are
tiered **before** frontend *adds*: Sōzu keys a route by `address;hostname;path[;method]`
(*not* `cluster_id`), so re-pointing a host+path at a different cluster is a `Remove`+`Add` on
the same route key, and adding first would be rejected as a duplicate (`StateError::Exists`)
— there is no atomic frontend replace in 2.1.0. A replacement cert lands before the old is
removed (no TLS gap). This also makes the HashSet-ordered routing diff deterministic for
golden snapshots.

### Gateway API (Phase 2)

`crates/gateway-api` holds **kopium-generated** CRD types (v1.6.1 standard channel,
`--schema=disabled`; regenerate per its README — do not hand-edit). The builder's
[`gateway` module](crates/builder/src/gateway.rs) maps GatewayClass/Gateway/HTTPRoute through the
**same** Service→pod-IP resolver and into the **same** IR as Ingress (a route and an Ingress to one
Service share a cluster). `allowedRoutes.namespaces.from: Selector` is evaluated against a Namespace label
index (a cluster-wide Namespace watch, labels only); a selector this build *cannot* evaluate still
fails closed and is reported. Gateway listeners map to the static listeners by protocol and must
declare the **advertised** ports (default `80`/`443`, `--gateway-http(s)-port` — the Service's
client-facing ports, wired by the chart; a mismatch is rejected with `PortUnavailable`); cross-ns
refs are gated on ReferenceGrant. Anything Sōzu can't represent (weighted multi-backend split,
header/query matches, TLS passthrough) is reported as a `Problem` and skipped, never approximated.

**Phase 3 — HTTPRoute filters.** `RequestHeaderModifier`/`ResponseHeaderModifier` and
`RequestRedirect` (scheme + status) compile into per-frontend `ir::FrontendFilters`, which the
translator maps onto Sōzu's frontend fields. A non-empty header value appends; an empty value
deletes. Gateway `set` therefore emits a deletion followed by an append on the same frontend,
while `add` appends and `remove` deletes. Empty Gateway `set`/`add` values are refused with
`Accepted=False`, `UnsupportedValue`, because they would otherwise become deletions. Unsupported
sub-fields (`RequestMirror`, for example) are reported, never half-applied. A `RequestRedirect`
rule has no `backendRef` (the API forbids it), so it becomes a **cluster-less frontend** — hence `ir::Frontend::cluster_id` is
`Option<String>`. **`URLRewrite` and redirect host/path/port targets are reported, not wired** — and
that is a *choice*, not a Sōzu limit. Both were measured working on Sōzu 2.2.0
([PROTOCOL.md §13](PROTOCOL.md), [docs/E2E-RESULTS.md §5c](docs/E2E-RESULTS.md)), which also
retired an earlier `408` result taken against 2.1.0. Two measured conditions gate any wiring: a
literal `$` in a rewrite value makes Sōzu **reject the frontend** (and translation is
all-or-nothing, so one such route fails every reconcile), and a path rewrite **drops the query
string** that `ReplaceFullPath` keeps. `ReplacePrefixMatch` is a real limit — the compiled prefix
regex has no capture group at all (boundary and tail are non-capturing), so `$PATH[1]` is
undefined; capturing the remainder is a change to that shared regex in `ir::PathMatch::sozu_rule`.
The translator already maps `ir::Rewrite`, so the builder side is the only piece missing.

The CRDs are **optional**, in two tiers: GatewayClass/Gateway/HTTPRoute/ReferenceGrant are
*required* (any one missing ⇒ Ingress-only), TCPRoute/UDPRoute are *optional* (missing ⇒ no
layer-4 routing, everything else unaffected). `crd_served` reads **only a 404** as absent, so the
RBAC grant must cover every probed kind: the v1.6.1 standard channel ships tcproutes/udproutes, and
an ungranted probe answers **403**, which is fatal by design. Status
(`Accepted`/`Programmed`/`ResolvedRefs`) is written by [`controller/src/status.rs`](crates/controller/src/status.rs),
which is **loop-safe** — it reuses `lastTransitionTime` for unchanged conditions and skips no-op
patches, so the controller's own status writes never re-trigger it. Status writes are best-effort.
The route writer is **generic over the route kind** (kopium emits one status triple per kind with no
trait in common, so `RouteParents` is declared controller-side and implemented per kind) and keys a
`status.parents[]` entry on the **whole** parentRef, `sectionName` and `port` included: one route may
name a Gateway once per listener, and matching on `(name, namespace)` alone collapses those entries
into one whose `lastTransitionTime` then moves on every pass.
`Problem`s also surface to users: as the detail in `False` condition messages, and as Warning
**Events** on the owning Ingress/Gateway/route ([`controller/src/events.rs`](crates/controller/src/events.rs),
diffed against the previous pass so resyncs never flood etcd).

**Conformance is a documented partial, by design.** On the v1.6.1 suite, unconditioned, the
GATEWAY-HTTP profile scores **20/37 core, 1/3 extended** — full run log, per-test attribution and
reproduction in [docs/E2E-RESULTS.md](docs/E2E-RESULTS.md) §6, reports in
[docs/conformance/](docs/conformance/) (immutable, one file per run). Rows 2–4 of that log predate
the `Selector` implementation, when the suite **aborted in setup** because
`NamespacesMustBeReady` demands every base Gateway be `Programmed: True` and one of them uses
`from: Selector`; those rows are conditioned and must never be quoted bare. The profile **cannot
fully pass** on Sōzu (no weighted splits, no header/query matching, no HTTP 500), so don't chase
the "Conformant" badge — and don't read the recorded failures as regressions.
`GatewayClass.status.supportedFeatures` is published **empty** on purpose: an entry goes in only
when a recorded run shows its tests passing. [docs/features.md](docs/features.md) is the
user-facing support matrix (supported / planned / not supported); keep it in sync when support
changes.

### Conventions that matter

- **HTTP/HTTPS listeners are NOT modelled in the IR.** They are declared
  statically in Sōzu's `config.toml` ([deploy/sozu/config.toml](deploy/sozu/config.toml)) and
  activated at boot; their addresses come from the chart's `exposure` table. **L4 (TCP/UDP)
  listeners are the exception**: their ports are user-defined, so the IR carries
  `ir::L4Frontend`s and the translator adds + activates the listeners dynamically over the socket —
  `ConfigState::diff` emits `Add{Tcp,Udp}Listener` + `ActivateListener` (and the reverse on removal)
  for free.
- **Layer-4 routes are `TCPRoute`/`UDPRoute` on a `protocol: TCP`/`UDP` Gateway listener, and
  conflicts are settled in the builder.** (The cluster-global `tcp/udp-services` ConfigMaps are
  gone — see [docs/UPGRADING.md](docs/UPGRADING.md).) Two routes contesting a socket are settled by
  oldest `creationTimestamp` **then**
  `namespace/name` — the timestamp has one-second granularity, so without the second key the winner
  would follow cache iteration order and flip between reconciles. The translator's
  `check_l4_conflicts` stays as a net, but it must never be what settles this: it returns an error
  that `reconcile` propagates with `?`, so one tenant's second route would fail the whole reconcile,
  HTTP included.
- **A layer-4 port can never be 443** — the Service already publishes `443/TCP` for `https` and a
  Service cannot expose one `(port, protocol)` twice — and no bind may be privileged (uid 1000,
  all capabilities dropped). Both are `fail`ed by the chart's `validateExposure` helper rather than
  left to a raw apiserver rejection.
- **Backends are pod IP:port resolved from EndpointSlices — never the Service ClusterIP.** When a
  Service has multiple ports, match the EndpointSlice port by name; only fall back to the sole port
  when there is exactly one (don't guess `first()`).
- **`pathType: Prefix` is element-boundary matching, not a string prefix.** `/foo` covers `/foo`,
  `/foo?q=1` and `/foo/bar` but never `/foobar`, so the translator compiles a non-root prefix to an
  regex — Sōzu's own `Prefix` rule is a raw `starts_with`. Two measured facts drive the
  pattern: Sōzu matches path rules against the request target with the **query string attached**,
  and 2.2.1 does **not** anchor regexes — while its unreleased successor wraps every `Regex` rule
  in `\A(?:…)\z` (upstream 47eb07c, BREAKING). **Every generated pattern is therefore full-span:**
  `^<escaped>(?:[/?](?-u:.*))?$` for a prefix, so it reads the same under both routers, and the
  `(?-u:.*)` tail is bytes, not UTF-8 characters, because Sōzu compiles with `regex::bytes` whose
  Unicode `.` skips a byte that is not valid UTF-8 (the `ir` crate's
  `sozu_rules_match_the_same_targets_unanchored_and_anchored` pins both properties on Sōzu's own
  `regex` version). The root `/` stays a plain prefix. The builder canonicalises
  `/foo/` to `/foo` first, so one path has one IR spelling. **`pathType: Exact` is a full-span
  regex too** (`^<escaped>(?:\?(?-u:.*))?$`, trailing slash kept literal), never Sōzu's `Equals`: measured
  on 2.2.1, `Equals` misses a query-bearing target and cannot be removed once added (the worker's
  rule equality has no `Equals` arm), so a removed `Exact` route kept serving. The compilation
  lives in `ir::PathMatch::sozu_rule` and both the builder's collision key and the translator use
  it, so two spellings of one Sōzu rule are one route everywhere.
- A frontend becomes HTTPS-enabled only if a TLS host with a *successfully loaded* cert covers it.
  Wildcard TLS hosts (`*.example.com`) cover exactly one extra label.
- **Tenant input Sōzu would reject is refused in the builder, with the calls Sōzu makes.** A
  `tls.key` is parsed and loaded the way Sōzu loads it (`rustls-pki-types` + the ring provider)
  and then checked against the leaf, which Sōzu skips — by a signature round-trip, never a byte
  comparison of the two key encodings, and **never stricter than Sōzu**: a leaf carrying a
  compressed EC point (`02`/`03`) is admitted unpaired, since ring verifies only uncompressed
  points and Sōzu serves such a leaf regardless; **every path rule that compiles to a
  regex** — a user `ImplementationSpecific`/`RegularExpression` pattern, and the anchored regex a
  non-root `Prefix` or an `Exact` becomes — is compiled with `regex::bytes::Regex::new` on the
  `regex` version Sōzu 2.2.1 builds against (pinned in the lock), because the crate refuses a
  compiled program past 10 MiB and a literal path of a few hundred kilobytes, which Kubernetes
  accepts, reaches it. Measured (2026-09-21): either input unvalidated makes Sōzu reject the request, and since
  translation is all-or-nothing that fails every reconcile of the shared instance until the
  object is removed. The builder is the only place these strings enter the IR; the translator
  does not re-validate.
- **Metrics are pulled, not pushed.** Sōzu has no native `/metrics`; the controller serves one
  (`--metrics-listen`, off when the flag is absent; the chart sets it by default via
  `metrics.enabled`) by issuing a `QueryMetrics` over the command
  socket on each scrape and rendering the returned `AggregatedMetrics` with the pure
  [`prometheus` crate](crates/prometheus), prefixed by the controller's own health signals
  (`sozu_gw_controller_*`, incl. the last-successful-reconcile timestamp — the staleness alert).
  It is best-effort and orthogonal to routing: a socket
  error returns `503`, never a panic. Sōzu's histogram buckets are already cumulative (`le`), so they
  map straight onto Prometheus `_bucket`; `Percentiles` become a `summary` (Sōzu only max-merges them
  across workers — the companion `*_histogram` is the accurate aggregate).
- The Sōzu command socket takes a **bare length-prefixed `Request`**; replies come back as
  `Processing` → `Ok`/`Failure`, so every send loops until a terminal status. This protocol is
  **verified against a live Sōzu**, documented in [PROTOCOL.md](PROTOCOL.md) (the source of truth
  for the translator). Never reimplement the wire
  format or invent protobuf fields — reuse the crate's types and conversions
  (e.g. `addr.into()`, never hand-pack an address).

### Version pins (verified, do not bump casually)

`sozu-command-lib` **2.2.1** (LGPL-3.0, pinned exactly) against Sōzu **2.2.1**, `kube` **4**, `k8s-openapi`
**0.28** with feature `v1_36` (the e2e cluster's version). Gateway API types are generated from the
**v1.6.1** standard-channel CRDs with `kopium` **0.24** (the published `gateway-api` crate targets
`kube` 3 / `k8s-openapi` 0.27, so it can't be used here). Workspace is edition 2021,
rust-version 1.93.1 — a floor the dependency tree sets, not our own code.

The library and data-plane image are aligned at 2.2.1. The library redacts
certificate and key material from `Debug`, and its `command.proto`, `ConfigState::diff`
and channel framing are byte-identical to 2.2.0. If the versions diverge again,
verify that the wire format still agrees before upgrading either side.

Upstream `main` was read against this codebase on 2026-09-22 (a4f0953, 131 commits past 2.2.1,
none released). What it changes for us, so the next bump starts from here: path `Regex` rules
become anchored `\A(?:…)\z` (47eb07c, BREAKING — the generated rules are already full-span, user
patterns are documented); `PathRule::eq` gains its `Equals` arm (795b95d — `Exact` stays a regex
for the query string; an installed `Equals` rule then *could* be removed by a request, but both
diff sides compile to a regex so nothing emits one, and the Pod roll in UPGRADING.md stands);
hostnames match case-insensitively and
certificate names are lowercased at load (704b60d — inferred names are already lowercased here);
a certificate name with `/` is refused (e4aac48 — already refused via `validate_sni_pattern`); a
UDP frontend address becomes exclusive across clusters with a new `StateError` text (044efef —
re-verify `sozu-agent`'s teardown-tolerance list, which keys on failure text); and the worker's
same-fingerprint `ReplaceCertificate` short-circuit is **kept on purpose**, so
[#81](https://github.com/CleverCloud/sozu-gateway/issues/81) stays open.

## Deployment model

Control plane (this repo) and data plane (Sōzu) are **separate processes/containers in one Pod**,
sharing the command socket via an `emptyDir` volume. Both run as the **same unprivileged uid
(1000)** so they can share that socket. The Helm chart ([charts/sozu-gateway](charts/sozu-gateway))
ships both containers, an `IngressClass`, RBAC, and Sōzu's `ConfigMap`. The Sōzu image is used
as-is (`clevercloud/sozu:2.2.1`) because the release binary is musl-linked.

Releases (`v*` tags) publish the controller image (`ghcr.io/clevercloud/sozu-gateway-controller`)
and the Helm chart (`oci://ghcr.io/clevercloud/sozu-gateway`) via
[.github/workflows/release.yml](.github/workflows/release.yml).

## Working notes

- `.scratch/` is local research/probe scaffolding (live-Sōzu protocol probes, recon notes behind
  PROTOCOL.md). It is gitignored, so it may not exist in a fresh clone; never rely on it.
- Errors: typed per-crate with `thiserror`; `anyhow` only in the controller binary.
- Code, comments, and docs are in English.
