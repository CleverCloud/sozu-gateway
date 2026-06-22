# sozu-gateway

A Kubernetes **Ingress controller + API gateway** built on the [Sōzu](https://github.com/sozu-proxy/sozu)
reverse proxy, in Rust. The controller watches Kubernetes objects, compiles them into a
neutral intermediate representation (IR), and pushes the resulting state to a co-located
Sōzu instance over its protobuf **command socket** — entirely hot, no restarts.

> **Status: Phase 1 (MVP — Ingress + TLS) in progress.** See [PROGRESS.md](PROGRESS.md)
> and the verified [PROTOCOL.md](PROTOCOL.md).

## Architecture

```
K8s objects ─▶ cache/watch ─▶ Builder ─▶ IR ─▶ Translator ─▶ protobuf cmds ─▶ Sōzu socket
```

Control plane (this repo) and data plane (Sōzu) are **separate processes**; in-cluster
they share the command socket via an `emptyDir` volume in the same Pod.

| Crate | Role | I/O? |
|---|---|---|
| [`crates/ir`](crates/ir) | neutral IR structs (`Listener`/`Cluster`/`Frontend`/`Backend`/`Certificate`) | none |
| [`crates/builder`](crates/builder) | K8s objects → IR (+ status), resolves Service→EndpointSlice & TLS Secrets | none (typed objects in) |
| [`crates/translator`](crates/translator) | pure IR → Sōzu commands, diffs vs last-applied | none |
| [`crates/sozu-agent`](crates/sozu-agent) | thin wrapper around `sozu-command-lib` (socket, send, LoadState) | **socket** |
| [`crates/controller`](crates/controller) | `kube-rs` Controller runtime, wires it all together | **kube + socket** |

`ir`, `builder`, `translator` are kept free of `kube`/socket I/O so they're unit-testable in isolation.

## Key facts (verified)

- `sozu-command-lib` **2.1.0** (LGPL-3.0), Sōzu **2.1.0**, `kube` **4.0**, `k8s-openapi` **0.28** (`v1_36`).
- The Sōzu command socket takes a **bare `Request`** (length-prefixed prost); responses
  come back as `Processing` → `Ok`/`Failure`. Full protocol notes in [PROTOCOL.md](PROTOCOL.md).

## Local development

Prereqs: Rust (stable), Docker. (Sōzu's release binary is musl-linked, so we run it via the
`clevercloud/sozu:2.1.0` image.)

```bash
cargo check --workspace        # build everything
cargo test --workspace         # unit / golden tests (Translator, Builder)

# Étape-1 protocol probe: starts a live Sōzu + a backend, applies config over the
# socket, and proves HTTP/HTTPS traffic flows (curl + openssl SNI check).
bash .scratch/run-probe.sh
```

The end-to-end on a real cluster (deploy the add-on + a demo app + Ingress) is wired up
in Étape 5 (`make e2e` / `justfile`).
