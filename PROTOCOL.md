# PROTOCOL.md — Sōzu command socket, as actually observed

**Source of truth for the Translator and `sozu-agent`.** Everything here is taken
verbatim from the real crate source and **confirmed against a live Sōzu 2.1.0**
with the probe in [`crates/sozu-agent/examples/probe.rs`](crates/sozu-agent/examples/probe.rs).
If this document and the scoping prompt disagree, this document wins.

- **Crate**: `sozu-command-lib` **2.3.2** (pinned). License **LGPL-3.0**.
  Edition 2024, `rust-version = 1.93.1`. protobuf via **prost 0.14**. It redacts sensitive
  `Debug` and error output (since 2.2.1). Its `command.proto` only *adds* to 2.2.1's — new
  fields and verbs we leave unset, §4 — so the messages we send are encoded as before.
- **Data plane**: Sōzu **2.3.2** (`clevercloud/sozu:2.3.2`; binary is **musl** → run via Docker).
  The dated 2.1.0/2.2.0/2.2.1 observations below retain their original versions; what 2.3
  changed in the state semantics we rely on is in §3, §6, §7 and §9, and
  [the 2.2.1 validation](docs/E2E-RESULTS.md#sozu-221-upgrade-validation-2026-09-10)
  records the previous image's checks.
- **Generated proto**: `src/proto/command.rs` (prost-generated; `package command;`, **`syntax = "proto2"`**).
- **Type paths**: there are *no* root re-exports. Use the literal paths:
  - `sozu_command_lib::proto::command::{Request, Response, Cluster, AddBackend, RemoveBackend, RequestHttpFrontend, AddCertificate, ReplaceCertificate, RemoveCertificate, CertificateAndKey, PathRule, SocketAddress, IpAddress, Status, ...}`
  - the request oneof: `sozu_command_lib::proto::command::request::RequestType`
  - enums: `...::command::{ResponseStatus, PathRuleKind, RulePosition, LoadBalancingAlgorithms, TlsVersion, ...}`
  - channel: `sozu_command_lib::channel::Channel`
  - cert helpers: `sozu_command_lib::certificate::{split_certificate_chain, get_cn_and_san_attributes, calculate_fingerprint, load_full_certificate}`
  - state: `sozu_command_lib::state::ConfigState`

---

## 1. Transport & wire format

The command socket is a **UNIX stream socket**. The crate's `Channel<Tx, Rx>` owns
the framing — **we never reimplement it**.

- Frame = **8-byte little-endian length prefix** (value = `payload_len + 8`) followed
  by the **prost-encoded** message. (Source: `channel.rs` write/read paths + raw-frame test.)
- The JSON + `\0` form some code paths use is **only for state files**
  (`SaveState`/`LoadState` on disk), **not** the socket.
- API (`channel.rs`):
  - `Channel::<Tx, Rx>::from_path(path: &str, buffer_size: u64, max_buffer_size: u64) -> Result<_, ChannelError>` — returns a **non-blocking** channel.
  - `channel.blocking()?` — switch to blocking for synchronous send/recv.
  - `channel.write_message(&Tx)?` / `channel.read_message() -> Result<Rx, ChannelError>`.
  - `Tx`/`Rx` must be `prost::Message + Default + Debug`.
- Buffer sizing: `command_buffer_size` / `max_command_buffer_size` in `config.toml`
  bound the *server* side; the client picks its own `from_path` sizes. We use 1 MiB /
  16 MiB (cert PEMs are small, but headroom is cheap).

### ✅ Transport type — RESOLVED by probe

The external client sends a **bare `Request`** — i.e. `Channel<Request, Response>`.
`WorkerRequest { id, content }` is the **Sōzu↔Sōzu internal** envelope (proto comment:
*"This is sent only from Sōzu to Sōzu"*) and is **not** used on the command socket.
Confirmed: `Status`, `AddCluster`, `AddBackend`, `AddHttpFrontend`, `AddCertificate`,
`AddHttpsFrontend` all succeeded sent as bare `Request`.

---

## 2. Request envelope & how to build one

```proto
message Request { oneof request_type { /* tagged variants */ } }
```

Build a request by wrapping a `RequestType` variant — `From<RequestType> for Request`
exists (`proto/mod.rs`), so just `.into()`:

```rust
use sozu_command_lib::proto::command::{request::RequestType, Request, Status};
let req: Request = RequestType::Status(Status {}).into();
```

proto2 enum fields are stored as **`i32`** in Rust; set them with `Variant as i32`.
There are **no** generated setter/getter accessors for these — assign the i32 directly.
prost `map<…>` fields become **`BTreeMap`** (deterministic). Every message derives
`serde::{Serialize, Deserialize}` (JSON enum case = `SCREAMING_SNAKE_CASE`).

---

## 3. `RequestType` variants used in Phase 1

Rust variant names (PascalCase; proto field names are snake_case):

| Variant | Payload | Use |
|---|---|---|
| `Status(Status)` | `Status {}` (empty) | health/ack probe |
| `AddCluster(Cluster)` | upsert | one cluster per target Service |
| `RemoveCluster(String)` | cluster_id | GC |
| `AddBackend(AddBackend)` | upsert | **pod IP:port** (never ClusterIP) |
| `RemoveBackend(RemoveBackend)` | | scale-down / pod churn |
| `AddHttpFrontend(RequestHttpFrontend)` | verb=15 | HTTP route |
| `RemoveHttpFrontend(RequestHttpFrontend)` | verb=16 | |
| `AddHttpsFrontend(RequestHttpFrontend)` | verb=17 | HTTPS route (**same payload**, different verb) |
| `RemoveHttpsFrontend(RequestHttpFrontend)` | verb=18 | |
| `AddCertificate(AddCertificate)` | | load Secret cert onto :443 listener |
| `ReplaceCertificate(ReplaceCertificate)` | | atomic cert rotation |
| `RemoveCertificate(RemoveCertificate)` | by fingerprint | |
| `LoadState(String)` / `SaveState(String)` | path | bootstrap/persist (optional) |
| `ListFrontends(FrontendFilters)` / `QueryClusterById(String)` | | verification/queries |
| `AddHttpListener`/`AddHttpsListener`/`ActivateListener` | | **not needed** when listeners are static in `config.toml` (see §8) |

> HTTP vs HTTPS is the **verb** (`AddHttpFrontend` vs `AddHttpsFrontend`), the payload
> type is identical (`RequestHttpFrontend`); the `address` field selects which listener.

### State semantics that changed in 2.3 (read from source, measured where marked)

- **`RemoveCluster` cascades** (2.3.0, upstream c2cad338): it also removes every frontend and
  backend naming the cluster, and `ConfigState::diff` relies on that — it emits no removal for
  what the cascade takes (§9). A frontend removal the cascade made redundant answers `Ok`
  "nothing to remove: cluster … does not exist …", since 2.3.0 (same commit, §6).
- **A frontend removal is cluster-scoped.** It removes the stored entry only if it names the
  cluster that entry belongs to — always so for TCP/UDP, and for HTTP/HTTPS since 2.3.0 (2.2.1
  removed an HTTP route by its key alone). Naming another cluster answers "Did not find …" and
  leaves the route serving. Measured on 2.3.2: add K→a, add K→b (`Exists`), remove K naming b
  ("Did not find"), re-add K→b (`Exists` again). `sozu-agent` therefore looks the stored owner up
  with `ListFrontends` (it returns `cluster_id` for http, https, tcp and udp frontends) and
  removes that entry.
- **`AddBackend` upserts only inside an existing cluster** (2.3.2, upstream 8953343f): naming a
  cluster the state does not hold — never added, or already removed — fails with
  `ClusterNotFound` ("Backend names a cluster that does not exist …; add the cluster first",
  measured; 2.2.1 answered `Ok` and stored an orphan backend). Our tiers add clusters first.
- **A UDP frontend address is exclusive across clusters** (2.3.0, upstream 044efef): a second
  cluster's claim answers `UdpFrontendAddressTaken` ("UDP frontend address … is already claimed
  by another frontend", measured; 2.2.1 let the last add win). An exact re-add of the same
  identity still answers `Exists`.

---

## 4. Message field reference (exact, from `proto/command.rs`)

`required` proto2 fields are plain Rust types; `optional` → `Option<T>`; `repeated` → `Vec<T>`.
Use `..Default::default()` for everything not listed below.

### `RequestHttpFrontend`
```rust
cluster_id: Option<String>   // None = a deny/redirect-only frontend; Some(id) routes to a cluster
address:    SocketAddress    // REQUIRED — the listener address this attaches to (e.g. 0.0.0.0:443)
hostname:   String           // REQUIRED — SNI/Host match
path:       PathRule         // REQUIRED — see PathRule
method:     Option<String>
position:   i32 (RulePosition, default Tree)
tags:       BTreeMap<String,String>
// Phase-3 fields (leave default in Phase 1): redirect, required_auth, redirect_scheme,
// redirect_template, rewrite_host, rewrite_path, rewrite_port, headers, hsts, ...
```

### `Cluster`
```rust
cluster_id:     String        // REQUIRED
sticky_session: bool          // REQUIRED (Phase 1: false)
https_redirect: bool          // REQUIRED (Phase 1: false)
load_balancing: i32           // REQUIRED — LoadBalancingAlgorithms (RoundRobin = 0)
proxy_protocol: Option<i32>, answer_503: Option<String>, load_metric: Option<i32>, http2: Option<bool>
// Phase-3 fields: answers, https_redirect_port, authorized_hashes, www_authenticate,
// max_connections_per_ip, retry_after, health_check, udp ...
// 2.3, fields 17–23, all left unset: max_connections_per_subnet, affinity_header,
// affinity_cookie, shard_percent, shard_min_backends, shard_mode, max_connection_attempts
```
On a UDP cluster, 2.3 applies `max_connections_per_ip` as a cap on concurrent flows per source
address, dropping new flows silently, with every client source port its own flow (2.3.0,
upstream 99e6aee2); the translator never sets it on a UDP cluster.

### Added in 2.3, not used here

Fields: `interface` on every listener config, `ActivateListener` / `DeactivateListener` /
`RemoveListener` (bind to a network interface); `AddBackend.unix_path` (a UNIX-socket backend);
`RequestHttpFrontend.header_predicates` (header-based routing). Verbs 60–75:
`set/query_max_connections_per_subnet`, `set_max_connection_attempts`,
`add/update_https_listener_with_client_auth`, `close_sessions`,
`add/remove_http(s)_frontend_with_header_predicates` (66–69),
`add_http(s)_frontend_with_silent_close` (70–71), `add_unix_backend` (72) and the protected
health-check verbs (73–75). `RequestType::add_http_frontend` and siblings pick the predicate or
`silent_close` verb when the payload needs it — `ConfigState::diff` included — so the
translator tiers those verbs with the plain ones even though our IR never produces them. A
2.2.x data plane does not answer verbs it does not know.

### `AddBackend` / `RemoveBackend`
```rust
// AddBackend
cluster_id: String           // REQUIRED
backend_id: String           // REQUIRED — stable id per endpoint, e.g. "<cluster>-<ip>-<port>"
address:    SocketAddress    // REQUIRED — the POD IP:port
sticky_id:  Option<String>, load_balancing_parameters: Option<LoadBalancingParams>, backup: Option<bool>
// RemoveBackend = { cluster_id, backend_id, address } (all required)
```
On a `sticky_session` cluster the `SOZUBALANCEID` cookie is set to
`sticky_id.unwrap_or(backend_id)`, but a returning cookie is matched against
`sticky_id` only, so without one it never selects a backend. ✅ Measured on
2.2.1 (two backends, two workers): with `sticky_id: None` ten replays of the
cookie alternated between both backends; after an `AddBackend` upsert setting
`sticky_id` on the *existing* backends, every replay reached the pinned one and
the cookie was no longer re-issued. The upsert updates `sticky_id` in place in
the main process's state and in each worker. ✅ Re-measured on 2.3.2 (one
worker): the in-place upsert behaves the same, as does the cookie over HTTP/2.
2.3 also re-pins a sticky client whose backend is in connect back-off.

An `AddBackend` naming a cluster the main process does not hold is refused since
2.3: `could not apply request on the main process state: Backend names a cluster
that does not exist (cluster_id_bytes=N); add the cluster first`. 2.2.1 accepts it
and serves nothing for it. ✅ Measured on both
([partial-apply probes](docs/probes/)): after an acknowledged `RemoveCluster`, a
shadow that still lists the cluster re-sends only the new backend, which 2.3.2
refuses on every reconcile. On 2.3 `RemoveCluster` also removes the cluster's
backends and its HTTP, HTTPS, TCP and UDP frontends (`ConfigState::remove_cluster`,
read in the 2.3.2 source); 2.2.1 removes the cluster entry alone. The translator's
batches remove the frontends and backends explicitly before the cluster either way.

### `PathRule`
```rust
kind:  i32      // PathRuleKind: Prefix=0, Regex=1, Equals=2
value: String   // the prefix / regex / exact value
```

### `SocketAddress` / `IpAddress`
```rust
SocketAddress { ip: IpAddress, port: u32 }     // both REQUIRED
IpAddress { inner: Option<ip_address::Inner> } // Inner::V4(u32 fixed32) | V6(Uint128{low,high})
```
**Never hand-pack.** Use `From<std::net::SocketAddr>`:
`let a: SocketAddress = "10.0.0.5:8080".parse::<SocketAddr>()?.into();`
(also `SocketAddress::new_v4(a,b,c,d,port)`). ✅ Round-trip verified — traffic reached
`127.0.0.1:9000` correctly, so v4 `fixed32` packing is correct end-to-end.

### `AddCertificate` / `ReplaceCertificate` / `RemoveCertificate` / `CertificateAndKey`
```rust
AddCertificate     { address: SocketAddress, certificate: CertificateAndKey, expired_at: Option<i64> }
ReplaceCertificate { address, new_certificate: CertificateAndKey, old_fingerprint: String, new_expired_at: Option<i64> }
RemoveCertificate  { address: SocketAddress, fingerprint: String }   // hex-encoded fingerprint
CertificateAndKey {
    certificate:       String,        // REQUIRED — leaf PEM
    certificate_chain: Vec<String>,   // intermediate PEMs (empty for self-signed)
    key:               String,        // REQUIRED — private key PEM
    versions:          Vec<i32>,      // TlsVersion; empty => server default (TLS 1.2 + 1.3)
    names:             Vec<String>,   // explicit SNI names; if empty, derived from cert
}
```
`address` binds the cert to the matching **listener address** (`0.0.0.0:443`); SNI
selection is then done by Sōzu. ✅ Verified: explicit `names = [host]` served the right
cert under SNI (`openssl s_client -servername app.example.com` returned our CN/SAN).

---

## 5. Enums (exact i32 values)

```text
ResponseStatus:           Ok=0, Processing=1, Failure=2
PathRuleKind:             Prefix=0, Regex=1, Equals=2
RulePosition:             Pre=0, Post=1, Tree=2
LoadBalancingAlgorithms:  RoundRobin=0, Random=1, LeastLoaded=2, PowerOfTwo=3, Hrw=4, Maglev=5
TlsVersion:               SslV2=0, SslV3=1, TlsV10=2, TlsV11=3, TlsV12=4, TlsV13=5
                          (JSON / config.toml spelling: "TLS_V12", "TLS_V13")
```

---

## 6. Response semantics — ✅ verified

`Response { status: i32 (ResponseStatus), message: String, content: Option<ResponseContent> }`.

- A request fanned out to the workers returns an **interim `Processing`** message **then** a
  terminal `Ok`/`Failure`. The client **must loop reading until `status != Processing`** — and
  must accept a terminal answer as the *first* message: a request the main process refuses
  itself (state validation) gets its `Failure` with no `Processing` before it (measured on
  2.2.1, so the old "every request gets `Processing`" was already wrong there), and since 2.3.0
  a removal whose cluster is gone gets a terminal-only `Ok` ("nothing to remove: cluster
  … does not exist …", followed by "this key belongs to cluster …, left in place" when another
  cluster holds the key).
  Observed terminal messages: apply ops → `"Successfully applied request to all workers"`;
  `Status` → `"Successfully collected the status of workers"`.
- **A fan-out times out after `worker_timeout`** (default 10 s, unset in our config; measured
  answers at 10.01 s on 2.2.1 and 2.3.2). With no worker answer the terminal `Failure` has an
  **empty message**; with some, it lists only their `N: OK` segments (measured). A worker's late
  answer is dropped. Upstream's 2.3.2 notes call such a timeout indeterminate: a late worker
  success may still have applied the order, so workers must be restarted or reconciled before
  retrying.
- When **no** worker acknowledges (every worker refused, or the fan-out timed out without one
  `Ok`), the main process reverts its own state with an inverse request, by release (read):
  2.2.1 — listener and HTTP/HTTPS frontend adds; 2.3.0 — TCP/UDP frontend adds too; 2.3.1
  (6f1f1cc0) — frontend *removals*, restoring the stored frontend snapshotted before dispatch;
  2.3.2 — the predicate/`silent_close` verbs and an `ActivateListener` that turned an inactive
  listener active. Upserts (`AddCluster`, `AddBackend`) and certificates have no inverse.
  Measured: re-sending a refused `AddTcpFrontend` answered "already exists" (a main-process
  leftover) on 2.2.1, and the real worker refusal again on 2.3.2. When the workers applied the
  order late, the rollback splits main and workers (measured, every worker paused past the
  timeout): an add the main reverted but the workers serve makes every re-add fail "could not add
  frontend: … Could not add route" (2.2.1 and 2.3.2); a removal the main restored but no worker
  serves (since 2.3.1; measured on 2.3.2) makes the next removal fail "Did not find" in every
  worker — which `sozu-agent` tolerates as gone — and leaves a ghost that a re-forked worker
  resurrects. Removing the ghost's cluster drops it (main-process cascade); `sozu upgrade
  --worker N` for each worker re-forks them from the main state, socket identity unchanged, after
  which the retry converges in both directions (measured on 2.3.2).
- **During a soft stop** (`sozu shutdown`, or SIGTERM since 2.3.0) the main keeps answering
  `Status` while the workers drain, and frontend adds and removals fail
  "N: found no listener at …" (2.3; "… with address …" on 2.2), measured.
- **`Status` and `RunState` are unchanged** (`RUNNING=0, STOPPING=1, STOPPED=2,
  NOT_ANSWERING=3`). `NOT_ANSWERING` marks a worker that did not answer within `worker_timeout`,
  so that `Status` itself takes ~10 s (measured). A soft-stopping worker reads `RUNNING` until it
  exits, then `STOPPED`; a `STOPPED` entry stays listed until a main-process upgrade (read).
- For broadcast apply ops, the terminal `status == Ok` means *all workers applied*.
  `content` was `None` for `Add*`; `Status` carried `ContentType::Workers(WorkerInfos)`.
- `content_type` oneof of interest: `Workers`, `WorkerResponses(map<id, …>)`,
  `FrontendList(ListedFrontends)`, `Clusters(ClusterInformations)`, `ListenersList`.
  (When `content` is `WorkerResponses`, individual sub-responses *could* differ from the
  aggregate — for >1 worker we should walk the map; not yet observed with 1 worker.)

---

## 7. Certificates

Build `CertificateAndKey` straight from the Kubernetes Secret bytes (`tls.crt` / `tls.key`):

- `split_certificate_chain(pem: String) -> Vec<String>` splits a concatenated PEM into
  individual certs (leaf is element 0; remainder is the chain).
- `get_cn_and_san_attributes(&X509Certificate) -> Vec<String>` derives SNI names if we
  ever want to (we set `names` explicitly from the Ingress host instead).
- `calculate_fingerprint(pem_bytes) -> Vec<u8>` for the hex fingerprint used by
  `RemoveCertificate`/`ReplaceCertificate`.
- **Rotation decision**: `ConfigState::diff` emits cert change as `RemoveCertificate(old)` +
  `AddCertificate(new)` (brief gap; still so in 2.3, where the diff also turns an order-only
  change into a Remove + Add of the same cert, which our tiering would reorder into a lost
  certificate). For zero-gap rotation we emit
  `ReplaceCertificate { old_fingerprint, new_certificate }` ourselves.
- **2.3.2 validates the pair before any state change**, in the main process and in every worker
  (`tls::build_validated_certificate`, upstream 0bc160b7): rustls `CertifiedKey::keys_match`,
  accepting only `Ok` or `InconsistentKeys::Unknown`. A leaf rustls-webpki cannot parse as an
  end-entity cert, or whose SubjectPublicKeyInfo differs byte-wise from the one ring derives from
  the key, is refused ("could not apply request on the main process state: invalid private key:
  …"). Measured refused on 2.3.2 and accepted on 2.2.1: X509v1 (`UnsupportedCertVersion`), a
  compressed EC point and a mismatched pair (`KeyMismatch`). It also refuses a name over 4096
  bytes, one containing `/`, and one the SNI trie cannot insert; names are lowercased at load
  (2.3.0). The builder runs the same `keys_match` (`extract_cert`).
- **A same-fingerprint `ReplaceCertificate` whose names changed is applied** (2.3.2, 0bc160b7):
  each worker re-inserts the entry under its resolver lock. Measured: names `[a]` → `[b]` on one
  fingerprint serves `b` and drops `a` on 2.3.2; 2.2.1 acknowledged it and kept serving `a`
  (our #81). An identical replay is still a no-op, and a plain `AddCertificate` of an existing
  fingerprint is still ignored, so `Replace` stays the verb for a name change.
- **A `ReplaceCertificate` fails `NotFound` when the listener holds no certificate at all**
  (2.3.0 deletes an emptied address bucket, upstream 6dd569ee); with another certificate present
  but the old fingerprint missing, it inserts the new one and answers `Ok`. `sozu-agent` answers
  that refusal with an `AddCertificate` of the replacement
  ([record](docs/probes/replace-certificate-fallback_sozu-2.3.2_2026-10-19.txt)).

---

## 8. Listeners — static in `config.toml` (decided & verified)

✅ With `activate_listeners = true` and `[[listeners]]` for `:80` (http) and `:443`
(https) in `config.toml`, **`AddHttpFrontend`/`AddHttpsFrontend`/`AddCertificate`
succeed without any `AddHttpListener`/`ActivateListener`** over the socket.

**Design**: the Helm chart ships listeners in `config.toml`; the controller manages
only clusters / frontends / backends / certificates. (Dynamic listener creation over
the socket remains available if we ever need per-listener tuning at runtime.)
Note: HTTPS listeners advertising `h2` require `buffer_size >= 16393`.

---

## 9. `ConfigState` & the diff strategy — REUSE sozu's diff

`sozu_command_lib::state::ConfigState`:
```rust
ConfigState::new() -> Self
fn dispatch(&mut self, request: &Request) -> Result<(), StateError>   // fold an Add*/Remove* into state
fn diff(&self, other: &ConfigState) -> Vec<Request>                   // minimal requests: self -> other
fn generate_activate_requests(&self) -> Vec<Request>
fn produce_initial_state(&self) -> InitialState
fn cluster_state(&self, id: &str) -> Option<ClusterInformation>
```

**Translator plan**: build the desired `ConfigState` by folding our `Add*` requests via
`dispatch` into a fresh `ConfigState`; keep a **shadow** `ConfigState` of last-applied;
emit `shadow.diff(&desired)`; on success set `shadow = desired`. This is idempotent
(`s.diff(&s)` is empty) and the crate self-verifies `diff` in its own debug-assert tests.

**Since 2.3.0 the diff relies on the `RemoveCluster` cascade** (§3): it emits `RemoveCluster(X)`
and no removal for X's frontends and backends. Our tiers put `RemoveCluster` after the frontend
adds, so a route moved from X to another cluster would be added while X's frontend still held
the key. The translator therefore appends `shadow.cluster_removal_requests(X)` for every
`RemoveCluster(X)` the diff emits. That public helper returns the explicit
`Remove{Http,Https,Tcp,Udp}Frontend` (predicate-aware verbs) and `RemoveBackend` requests for
what X owns — `[]` when X is not in the state, never the `RemoveCluster` itself — so batches keep
their 2.2 shape and apply cleanly on 2.2.1 and 2.3.2 data planes (measured on both).

In debug builds `diff` replays its own output and compares `tcp_fronts` with an order-sensitive
`Vec` equality, so it panics when a cluster gains a TCP frontend that sorts before one it already
holds (e.g. port 3000 added to a cluster holding 3001) — on 2.2.1 and 2.3.2 alike. Release
builds are unaffected; tests must avoid that shape.

Caveats (respect these):
- `generate_requests` is **private**; `diff` / `dispatch` / `produce_initial_state` /
  `generate_activate_requests` are the public entry points.
- Frontend/certificate ordering in `diff` output is **non-deterministic** (HashSet diff);
  clusters/backends/listeners are deterministic (BTreeMap). **Sort the `Vec<Request>`** if
  we want stable logs/golden tests.
- We still keep an in-memory shadow as the "last applied"; on (re)start we push the full
  desired state (and may `LoadState`/`SaveState` to a file for crash recovery — TBD).

---

## 10. Verified end-to-end sequence (probe output)

```
[1] Status                 -> PROCESSING -> OK  (content: Workers[id=0, Running])
[2] AddCluster             -> PROCESSING -> OK  "Successfully applied request to all workers"
    AddBackend             -> PROCESSING -> OK
    AddHttpFrontend        -> PROCESSING -> OK
[3] AddCertificate         -> PROCESSING -> OK
    AddHttpsFrontend       -> PROCESSING -> OK
[4] Status (idempotent)    -> PROCESSING -> OK

curl http  app.example.com:8080  -> 200   (served by backend through Sōzu)
curl https app.example.com:8443  -> 200
openssl s_client -servername app.example.com -> subject CN=app.example.com, SAN app.example.com
```

Reproduce: `bash .scratch/run-probe.sh` (will be promoted into the `justfile`).

---

## 11. Kubernetes Ingress → Sōzu mapping (Phase 1)

| K8s | Sōzu |
|---|---|
| Ingress rule `host` (exact) | `RequestHttpFrontend.hostname` |
| Ingress rule `host` wildcard `*.x` | `hostname` wildcard (Sōzu supports `*.` prefix) — to confirm in Étape 2 |
| `pathType: Prefix` | `PathRule { Regex, "^<value>(?:[/?](?-u:.*))?$" }` — element-boundary matching (raw `Prefix` also matches `/foobar`), full-span so it reads the same once Sōzu anchors regexes (§13 item 3); the root `/` stays `PathRule { Prefix, "/" }` |
| `pathType: Exact` | `PathRule { Regex, "^<value>(?:\\?(?-u:.*))?$" }` — never `Equals`: 2.2.x compares `Equals` against the query-bearing target (`/get?x=1` misses), and cannot remove an `Equals` rule it holds (`PathRule::eq` has no `Equals` arm), so a removed route kept serving — both measured live 2026-09-21 |
| `pathType: ImplementationSpecific` | `PathRule { Regex, value }` (2.2.1 does **not** anchor regexes — measured; 2.3.0 and later anchor them at both ends and compile the value bare, then wrapped — write full-span patterns) |
| backend `Service` → `EndpointSlice` pod IPs | one `Cluster` + N `AddBackend` (pod `IP:port`) |
| `spec.tls[].secretName` (`tls.crt`/`tls.key`) | `AddCertificate` on the `:443` listener address |
| HTTP rule | `AddHttpFrontend` on `:80` listener address |
| TLS host | `AddHttpsFrontend` on `:443` listener address |

---

## 12. Open questions / decisions (see chat — pending your input)

1. **Listener ports / model**: static listeners on `:80`+`:443` in `config.toml`
   (recommended, verified) vs dynamic via socket. → defaulting to static.
2. **State persistence on restart**: rely solely on the in-memory shadow + full re-push at
   startup, or also `SaveState`/`LoadState` a file for fast crash recovery? → defaulting to
   re-push; file persistence optional later.
3. **Regex path semantics** are now measured against a live Sōzu 2.1.0: rules are matched
   against the request target with the **query string still attached** (so `Equals("/foo")`
   does not match `/foo?page=2`), and regexes are **not anchored** (`/foo(/|$)` also matches
   `/xx/foo`). Both drive the `pathType: Prefix` and `Exact` mappings above. **Sōzu 2.3.0
   (upstream 47eb07c) reverses the second fact**: every `Regex` path rule is compiled bare, then
   wrapped in `\A(?:value)\z`, and must span the whole target, query string included (marked
   BREAKING upstream); the main process refuses a frontend whose value fails either compile
   (measured on 2.3.2: `/(?x)api # v1 only` and 249 nested groups, both accepted by 2.2.1; the
   wrapper adds two nesting levels against `regex-syntax`'s limit of 250, so 248 fails wrapped
   too, measured on the crate Sōzu builds against). The
   generated rules are written full-span for that reason — both anchors
   ours, plus a `(?-u:.*)` tail consuming the remainder as raw bytes, since Sōzu compiles with
   `regex::bytes` and kawa's `tolerant-http1-parser` admits `0xA0`–`0xFF` in a target — and the
   `ir` crate tests each rule compiled both ways on Sōzu's `regex` version. Re-measured on
   2.2.1 (2026-09-21): a `RemoveHttpFrontend` for an `Equals` rule is acknowledged but the
   worker keeps matching it — the isolated-host control answered 503 on the removed path
   against 404 on a sibling — which is why `Exact` is a regex too. 2.3.0 can remove an `Equals`
   rule (upstream 795b95d) but still compares it against the query-bearing target, so `Exact`
   stays a regex. Host matching (2.3.0, measured): case-insensitive (704b60d; a trailing dot
   still matches no host); `*.` covers exactly one label; and an exact host whose rules all miss
   the path or method hands the request to a covering `*.` host before the `POST` catch-all
   (2ea35516; 2.2.1 went straight to `POST`, or 404).
4. **Multi-worker fan-in**: with `worker_count > 1`, confirm whether to inspect per-worker
   `WorkerResponses` for partial failure (we'll add defensive handling in `sozu-agent`).

---

## 13. Rewrite & redirect templates — measured on 2.2.0; re-measured unchanged on 2.2.1 and 2.3.2 (2026-10-19)

`RequestHttpFrontend` carries `rewrite_host`, `rewrite_path` and `rewrite_port`. The proto
documents them under `PERMANENT` only, and describes `rewrite_host` as rewriting "both the
backend authority/path and the wire request line". Both statements needed measuring before
anything could be mapped onto them; `crates/sozu-agent/examples/rewrite_redirect_probe.rs`
reproduces every row below against a live Sōzu. The 2026-10-19 runs gave the same answers on
`clevercloud/sozu:2.2.1` and `:2.3.2` except where a row says otherwise, and their echo backend
records every `Host` line, where 2.2.0's recorded only the last.

Setup: one cluster with one backend (an in-process echo that reports the request line and the
`Host` it received), one frontend per row, request `GET /original` unless stated.

| Frontend fields | Observed |
| --------------- | -------- |
| *(none)* — control | `200`, backend sees `GET /original`, `Host: <frontend hostname>` |
| `rewrite_path="/rewritten"` | `200`, backend sees `GET /rewritten`, **Host unchanged** |
| `rewrite_host="elsewhere"`, `rewrite_path="/rewritten"` | `200`, backend sees `GET /rewritten` and **two** `Host: elsewhere` lines (2026-10-19; a Go `net/http` backend answers `400` to that, measured with a separate probe on both versions) |
| `rewrite_path="/rewritten"`, request `GET /original?q=1&x=2` | `200`, backend sees `GET /rewritten` — **the query string is dropped** |
| `rewrite_path="/lit$PATH[0]eral"` | `200`, backend sees `GET /lit/originaleral` — `$PATH[0]` is the whole request path |
| `rewrite_path="/price$100"` | **`AddHttpFrontend` REJECTED**: `Could not parse path rewrite "/price$100"` |
| `rewrite_path="/price$$100"` | **REJECTED** too — `$$` is no escape (2026-10-19) |
| `redirect=PERMANENT`, `scheme=USE_HTTPS`, `rewrite_host="new"`, `rewrite_path="/new"` | `301`, `Location: https://new/new` |
| `redirect=FOUND`, same rewrites | `302`, `Location: https://new/new` |
| `redirect=PERMANENT_REDIRECT`, same rewrites | `308`, `Location: https://new/new` |
| `redirect=FOUND`, same rewrites + `rewrite_port=8443` | `302`, `Location: https://new:8443/new` |
| `redirect=FOUND`, `scheme=USE_HTTPS`, no rewrites | `302`, `Location: https://<request host><request path>` |
| `redirect=FOUND`, `rewrite_path="/new"` on the `Exact` regex, request `GET /original?q=1&x=2` | `302`, `Location: http://<host>/new` — **the query is dropped** (2026-10-19) |
| path rule `Regex "^/foo(/\|\?\|$)"` (the pre-full-span prefix shape), `rewrite_path="/pfx[$PATH[1]]"`, request `/foo/bar/baz` | 2.2.0 and 2.2.1: `200`, backend sees `GET /pfx[/]`. 2.3.x: `404` — anchored, the pattern no longer spans the target |
| today's prefix rule `^/foo(?:[/?](?-u:.*))?$`, `rewrite_path="/xyz$PATH[1]"` | **REJECTED** — the rule has no capture group, so `$PATH[1]` is out of range (2026-10-19) |
| prefix rule `^/foo(?:(/)((?-u:[^?\n]*)))?((?:\?(?-u:.*))?)$`, `rewrite_path="/xyz$PATH[1]$PATH[2]$PATH[3]"` | `/foo` → `/xyz`; `/foo/bar?q=1` → `/xyz/bar?q=1`; with `"/$PATH[2]$PATH[3]"`, `/foo` → `/`; with `"/new$PATH[3]"`, `/foo/bar?q=1` → `/new?q=1` (2026-10-19) |
| `Exact` rule `^/original((?:\?(?-u:.*))?)$`, `rewrite_path="/rewritten$PATH[1]"` (or a `FOUND` redirect to `"/new$PATH[1]"`) | `GET /rewritten?q=1&x=2` (`Location: …/new?q=1&x=2`) — a captured query survives (2026-10-19) |

Raw runs: [2.2.0](docs/probes/rewrite-redirect_sozu-2.2.0_2026-08-04.txt),
[2.2.1](docs/probes/rewrite-redirect_sozu-2.2.1_2026-10-19.txt),
[2.3.2](docs/probes/rewrite-redirect_sozu-2.3.2_2026-10-19.txt).

What that settles:

1. **`rewrite_host` does not change which backend is dialled.** The proxy dialled the cluster's
   configured backend address and rewrote only the forwarded `Host`. Backends are explicit
   addresses (`AddBackend`), so there is no authority to resolve. An earlier recorded result —
   a route timing out with `408` under a literal rewrite — was taken against **2.1.0** and does
   not reproduce here. But it is not faithful either: on 2.2.1 and 2.3.2 (and presumably 2.2.0,
   whose probe could not see it) an HTTP/1 backend receives the rewritten `Host` **twice**, which
   a strict backend refuses — so `URLRewrite.hostname` stays unwired until upstream fixes it.
2. **All three redirect policies honour the rewrite fields**, `FOUND` and `PERMANENT_REDIRECT`
   included, despite the proto documenting only `PERMANENT`. `rewrite_port` lands as an explicit
   port in `Location`.
3. **`$` is a template sigil, and an unparseable template is a hard reject.** Not a warning, not
   a passthrough: `AddHttpFrontend` returns `Failure`. Since translation is all-or-nothing, one
   route whose rewrite value contains a stray `$` would fail *every* reconcile. Any mapping onto
   these fields has to escape or refuse `$` in the builder.
4. **`$PATH[n]` indexes the path-rule regex's capture groups**, `[0]` being the whole match. The
   regex a Kubernetes `pathType: Prefix` compiles to (§ the path mapping above) has **no**
   capture group — its boundary and tail are non-capturing — so a template naming `$PATH[1]` gets
   the frontend rejected. `ReplacePrefixMatch` needs boundary, remainder and query captured
   separately: `^<esc>(?:(/)((?-u:[^?\n]*)))?((?:\?(?-u:.*))?)$` matches the same targets as
   today's rule and gives the Gateway API results in the table on 2.2.1 and 2.3.2 (2.3's
   `\A(?:…)\z` wrapper adds no group). It is a change to that shared, load-bearing regex in
   `ir::PathMatch::sozu_rule`, not grafted on per route, and it does not extend to the root `/`,
   which must stay Sōzu's `Prefix "/"`: spelled as a regex it competes with every other regex of
   the host and the last one added wins (measured).
5. **A literal path rewrite discards the query string**, on the forwarding path and in a redirect
   `Location` alike. Gateway API's `ReplaceFullPath` replaces the path and leaves the query alone,
   so this is a real deviation, not a detail; capturing the query in the rule and appending its
   group (`/new$PATH[1]` on an `Exact` rule) keeps it (measured).
6. **A redirect keeps the query only when no path target is set.** `rewrite_host` with
   `rewrite_path` unset answers `Location: <request scheme>://<new host><request path><request
   query>`, so only the authority moves; with `rewrite_path` set the query is gone, as on the
   forwarding path (2026-10-19 — 2.2.0 measured only the first case).

### Header `set` appends instead of replacing — measured

`HeaderPosition`'s proto doc says the mutation "Mirrors HAProxy `http-request set-header` /
`http-response set-header` parity", and HAProxy's `set-header` **replaces**. The shipped
`clevercloud/sozu:2.2.0` binary appends.

| Frontend `headers` | Request sent | Backend / client sees |
| ------------------ | ------------ | --------------------- |
| `Request X-Env=prod` | `X-Env: staging` | `X-Env: staging`, `X-Env: prod` — **both** |
| `Request X-Env=prod` | *(none)* | `X-Env: prod` — one, so it is a pure append |
| `Request X-Env=` (empty val) | `X-Env: staging` | *(absent)* — delete works as documented |
| `Response X-Served-By=sozu` | backend answers `X-Served-By: backend` | `X-Served-By: backend`, `X-Served-By: sozu` |

These observations describe a **single protobuf Header entry**. They do not prevent composing
Gateway `set` from two entries: an empty value deletes existing occurrences, then a non-empty
value appends the replacement. Gateway `add` uses only the append entry; `remove` uses only the
delete entry. The controller now preserves this distinction without a protocol extension.

Sōzu 2.2.1's request path (`lib/src/protocol/mux/router.rs`, `apply_request_rewrites_and_headers`)
separates deletions from insertions, and its response path (`lib/src/protocol/mux/shared.rs`,
`apply_response_header_edits`) does the same for both HTTP/1 and HTTP/2. Each removes all existing
occurrences with a case-insensitive name comparison before appending the non-empty values.
The two entries belong to one frontend configuration, not separate control-plane updates.

A literal empty Gateway `set` or `add` value remains unrepresentable: the legacy wire encoding
would delete it. The builder rejects the affected rule with `FilterUnsupported` and
`Accepted=False` / `UnsupportedValue` instead of silently changing its meaning. The IR schema
is unchanged; an older shadow containing the former append-only `set` is updated by replacing
the frontend on the next reconcile.

Raw run: [docs/probes/rewrite-redirect-headers_sozu-2.2.0_2026-08-05.txt](docs/probes/rewrite-redirect-headers_sozu-2.2.0_2026-08-05.txt).
Re-measured unchanged on 2.2.1 and 2.3.2 (2026-10-19, the raw runs above), which also show two
edits the builder admits but Sōzu does not apply faithfully: a delete + append of `Host`
reaches the backend as two different `Host` lines and a delete of `Host` removes nothing, and a
CR/LF in a header value or a redirect path is written raw into the backend request or the
client response.
