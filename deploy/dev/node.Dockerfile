# A simulated server for local development and e2e tests: a Linux container
# with its own Docker engine, Caddy and modula-node. Lets a Mac run several
# "servers" with real Linux networking (health checks, routing) without VMs.
#
# Built by scripts/dev-node.sh, which compiles modula-node for Linux first.
FROM caddy:2-alpine AS caddy

FROM docker:28-dind
COPY --from=caddy /usr/bin/caddy /usr/bin/caddy
COPY modula-node /usr/local/bin/modula-node
COPY entrypoint.sh /usr/local/bin/modula-dev-node
ENV DOCKER_TLS_CERTDIR=""
EXPOSE 80 443
ENTRYPOINT ["/usr/local/bin/modula-dev-node"]
