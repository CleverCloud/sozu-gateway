# Container restart detection (2026-09-10)

A Sōzu container restart can reuse its worker PIDs and leave the controller
holding a stale shadow indefinitely. These checks exercised that failure and
its recovery on the dedicated `sozu-gateway-upgrade` Kubernetes cluster.

## Environment

- Kubernetes v1.36.3, three amd64 nodes, Cilium, Gateway API v1.6.1 standard CRDs,
  two gateway replicas, and the unmodified `clevercloud/sozu:2.2.1` image.
- Initial controller: revision `c8c0226`. Corrected controller: the same base
  with the socket generation fix, rebuilt and deployed with a new image digest.
- Docker was unavailable. The controller binary was built with
  `cargo build --release -p sozu-gw-controller` and packaged with `crane` on
  Ubuntu 24.04 for GLIBC 2.39, with public CA certificates and uid/gid 1000.
  The repository's Debian bookworm Dockerfile was not built or validated.
- Restart injection was graceful: `SIGTERM` for the controller and Sōzu's
  `shutdown` command for the proxy. Kubernetes restarted each container in the
  same Pod. These checks did not inject an abrupt process crash.

## Observations

| Check | Result |
| ----- | ------ |
| Initial controller, Sōzu-only restart | Workers reused PIDs `7, 8`; HTTP returned **404 throughout the 180 s recovery check**, while readiness stayed green and reconcile failures stayed zero. The Pod was replaced to restore service. |
| Corrected controller, Sōzu-only restart | Worker PIDs remained `7, 8`, socket inode changed **3 → 8**, and shadow resets increased **0 → 1**. Routing recovered in **58.1 s** without a controller restart; reconcile failures stayed zero. |
| Corrected controller, controller-only restart | The persisted shadow resumed; readiness returned after **66.8 s**, including cache synchronization. The Sōzu container's restart count was unchanged. |
| Corrected controller after recovery | Route, header, redirect path/query, readiness and metrics checks passed; `/readyz` and `/metrics` returned **200**, with zero reconcile failures. |

The other container's restart count stayed unchanged in both corrected restart
checks. Recovery after the Sōzu restart followed the configured **60 s** resync
period: the fix removes the indefinite 404 failure, but requests can still fail
while the restarted Pod waits for detection and reprogramming. Two replicas
keep another Pod available; they do not make the affected Pod's recovery gap-free.

The prior combined working tree passed **204 workspace tests**, formatting,
Clippy with `-D warnings`, chart lint, and the complete `REPLICAS=2 just e2e-all`
run. That tree also contained the image upgrade and a separate TCP echo probe
correction; these results describe that tested tree, not an isolated run of
this fix. The restart regression tests cover reused worker PIDs, socket
replacement, a live old connection after pathname replacement, a transient
reconnect to the same socket, and failed generation probes.

HTTP/TLS suite probes used Pod port-forwarding, and the continuous HTTP probe
used the in-cluster Service. External LoadBalancer traffic was not validated.
The Gateway API conformance profile was not rerun.

## Deployment

This fix requires an updated controller image. Changing only the Sōzu image
does not update restart detection. No configuration or persisted-shadow
migration is required.
