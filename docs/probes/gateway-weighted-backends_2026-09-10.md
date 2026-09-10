# Weighted backend observations — 2026-09-10

These measurements used Sōzu 2.2.1 on `sozu-gateway-upgrade`, with controller
revision `6d0c57a2bdd7789713e8f23bd2369fc32ffb5638` and image digest
`sha256:43f89d06070203a98b174edf98f275e531e3d5284918fc72892f3d13122ab1bf`.
That integration image combined the initial weighted compiler with the UDP
source-port fix. It predates the weighted HTTP 500/503 interaction and the final
branch stack; these observations do not validate those later changes.

## Official suite

The unmodified Gateway API v1.6.2 tests ran individually with `--gateway-class=sozu`,
the HTTP/TCP/UDP profiles, `--disable-parallel-tests=true`, and API QPS/Burst
100/200. `HTTPRouteWeight` and `TCPRouteWeightedRouting` passed. The first
`UDPRouteWeightedRouting` run failed at 12:56 UTC; a repeat with the same image
passed at 13:00 UTC. Both outcomes are retained here.

The failed UDP distribution subtest exhausted ten attempts in 0.66 seconds.
Attempts 1–8 sent every sampled flow to `udp-backend-v2`; attempt 9 measured
`udp-backend-v1` at 0.412 and attempt 10 at 0.648, outside its required
0.7 ± 0.05. The repeat passed without changing the test or its tolerances.
The changing shares are consistent with initial convergence, but the test log
alone does not establish the precise cause. A passing repeat does not establish
reliable behavior during startup.

## HTTP connections and unequal replica counts

The manual probe used one `probe-a` replica, three `probe-b` replicas and one
zero-weight replica. Each table row sent 400 GETs over 400 distinct HTTP/1
connections. The probe's absolute tolerance was 0.12, wider than the official
distribution test's 0.05; the exact counts are more useful than a pass label.

| Expected A share | VIP | A | B | Zero-weight |
| --- | --- | ---: | ---: | ---: |
| 0.50 | 82.47.250.4 | 176 | 224 | 0 |
| 0.50 | 82.47.250.5 | 223 | 177 | 0 |
| 0.25 | 82.47.250.4 | 91 | 309 | 0 |
| 0.25 | 82.47.250.5 | 103 | 297 | 0 |
| 1.00 | 82.47.250.4 | 400 | 0 | 0 |
| 1.00 | 82.47.250.5 | 400 | 0 | 0 |

Earlier sweeps reused one HTTP/1 connection for all 400 requests. All eight
sweeps, four per VIP, stayed on a single backend: four chose A and four chose B.
Those were eight independent selections, not 3,200 independent selections.

Sōzu [reuses an existing backend before selecting again](https://github.com/sozu-proxy/sozu/blob/2.2.1/lib/src/protocol/kawa_h1/mod.rs#L1693-L1735).
The official client's [disabled keep-alives](https://github.com/kubernetes-sigs/gateway-api/blob/v1.6.2/conformance/utils/roundtripper/roundtripper.go#L133-L143)
leave that behavior outside the suite's coverage. The same limitation applies
to weighted HTTP error shares: after an error backend closes its connection,
a persistent client can select a healthy backend and keep reusing it. See the
[support matrix](../features.md) for this and the responder availability limits.
