# GATEWAY-HTTP, CRD/suite v1.6.1, 2026-08-04 — setup blocked, no report produced

The conformance suite writes no report when it fails before the profile runs, so
this file stands in for one. It is part of the immutable run log.

```
--- FAIL: TestConformance
    Error:    context deadline exceeded
    Messages: error waiting for gateway-conformance-infra, gateway-conformance-app-backend,
              gateway-conformance-web-backend namespaces to be ready
FAIL    sigs.k8s.io/gateway-api/conformance    324s
```

Every one of the 2 530 polling lines names the same object:

```
helpers.go:285: gateway-conformance-infra/backend-namespaces Gateway not Programmed yet
```

State at the time, for the four base Gateways:

```
all-namespaces                     Accepted=True  Programmed=True
backend-namespaces                 Accepted=True  Programmed=False (Invalid)
same-namespace                     Accepted=True  Programmed=True
same-namespace-with-https-listener Accepted=True  Programmed=True

backend-namespaces .spec.listeners[*].allowedRoutes.namespaces.from = Selector
backend-namespaces .status.listeners[http] = Accepted=True Programmed=False(Invalid) ResolvedRefs=True
```

No pod-readiness and no `Accepted` failures were logged; the only unmet condition
was `Programmed` on that one Gateway. `conformance/utils/kubernetes/helpers.go`
`NamespacesMustBeReady` requires **every** Gateway in the conformance namespaces
to be `Programmed: True`, and the suite calls it during setup — so one Gateway
this controller fails closed stops the entire profile.

Run command: identical to the row above it in the log, `--version=e7-e11`.
