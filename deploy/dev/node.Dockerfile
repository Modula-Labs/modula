# A simulated server for local development and e2e tests: a Linux container
# with its own Docker engine, Caddy and modula-node. Lets a Mac run several
# "servers" with real Linux networking (health checks, routing) without VMs.
#
# Built by scripts/dev-node.sh, which compiles modula-node for Linux first.
# Caddy as the release builds it (scripts/release.sh CADDY_PLUGINS): the
# rate limit and cache edge rules need its plugins.
FROM caddy:2.11.1-builder-alpine AS caddy
RUN xcaddy build v2.11.1 \
      --with github.com/caddy-dns/cloudflare@v0.2.4 \
      --with github.com/mholt/caddy-ratelimit@v0.1.1-0.20260612195517-5625512f24f6 \
      --with github.com/caddyserver/cache-handler@v0.16.0 \
      --output /usr/bin/caddy

# workerd is built against glibc; the dind image is Alpine (musl). It only
# needs libc and libm, so it runs through glibc's own loader.
FROM debian:bookworm-slim AS glibc
RUN mkdir /glibc && cp /lib/*-linux-gnu/libc.so.6 /lib/*-linux-gnu/libm.so.6 /lib/ld-linux-*.so.* /glibc/

FROM docker:28-dind
# Private networking: nftables and iproute2 (WireGuard is the host kernel's).
RUN apk add --no-cache nftables iproute2
COPY --from=caddy /usr/bin/caddy /usr/bin/caddy
COPY modula-node /usr/local/bin/modula-node
COPY buildctl /usr/local/bin/buildctl
COPY railpack /usr/local/bin/railpack
COPY wal-g /usr/local/bin/wal-g
COPY --from=glibc /glibc /opt/glibc
COPY workerd /usr/local/lib/workerd
COPY workerd.sh /usr/local/bin/workerd
COPY entrypoint.sh /usr/local/bin/modula-dev-node
ENV DOCKER_TLS_CERTDIR=""
EXPOSE 80 443
ENTRYPOINT ["/usr/local/bin/modula-dev-node"]
