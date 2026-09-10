FROM golang:1.26.0-bookworm AS build
ENV CGO_ENABLED=0 GOTOOLCHAIN=local
WORKDIR /source
RUN git init gateway-api \
    && cd gateway-api \
    && git remote add origin https://github.com/kubernetes-sigs/gateway-api.git \
    && git fetch --depth=1 origin ca6c2a65454737236fb7a937bd9b17e42b07e9de \
    && git checkout --detach FETCH_HEAD \
    && test "$(git rev-parse HEAD)" = ca6c2a65454737236fb7a937bd9b17e42b07e9de
WORKDIR /source/gateway-api/conformance
COPY runner ./sozu-runner
RUN go test -mod=readonly ./sozu-runner -run '^TestSelection$' \
    && go test -mod=readonly -c -o /gateway-api.test ./sozu-runner

FROM alpine:3.22@sha256:7c8cb692ae09657cbc4a3f3cbd0e8d5a2690ba38386aaaf252dbb060bf5eb2e6
LABEL org.opencontainers.image.source="https://github.com/kubernetes-sigs/gateway-api" \
      org.opencontainers.image.revision="ca6c2a65454737236fb7a937bd9b17e42b07e9de"
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /gateway-api.test /usr/local/bin/gateway-api.test
USER 1000:1000
ENTRYPOINT ["/usr/local/bin/gateway-api.test"]
