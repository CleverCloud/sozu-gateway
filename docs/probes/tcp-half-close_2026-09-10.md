# TCP echo probe and client half-close (2026-09-10)

The layer-4 suite reported an empty TCP echo while the route and listener
statuses were healthy. Its `printf | socat -` client reached stdin EOF and
half-closed the connection before the reply returned; Sōzu then closed the
session. A direct backend probe returned its echo.

These observations were recorded during the original test campaign on
`sozu-gateway-upgrade` (Kubernetes v1.36.3, Cilium, Gateway API v1.6.1), before
the changes were split into separate branches:

| Probe | Sōzu 2.2.0 | Sōzu 2.2.1 |
| ----- | ---------- | ---------- |
| TCP echo with stdin EOF (`socat -`) | Empty reply | Empty reply |
| TCP echo with write side kept open (`socat -,ignoreeof`) | Echo returned | Echo returned |

The 2.2.0 comparison used an isolated proxy. Direct backend TCP echo and UDP
probes succeeded, so the empty reply was not evidence of a TCPRoute regression
in 2.2.1. Neither result establishes support for TCP half-close in the proxy.

[`scripts/e2e-l4-routes.sh`](../../scripts/e2e-l4-routes.sh) now uses
`-,ignoreeof` for TCP. The existing `-T5` five-second inactivity timeout remains,
including when no reply arrives. UDP retains its original stdin and
`UDP-SENDTO` arguments. Probe stderr remains visible for failed runs.

The complete `just e2e-l4-routes` recipe passed after this probe correction,
using the unmodified controller at revision `c8c0226` and the published
`clevercloud/sozu:2.2.1` image. TCP and UDP echo, route/listener statuses and
continued TCP traffic after a conflicting route was rejected all passed.
The controller image was shared with the other suites in that campaign: a
locally built release binary assembled with `crane` on Ubuntu 24.04 because
Docker was unavailable. This did not validate the repository's Dockerfile.
