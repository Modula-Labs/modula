# Environment for running modula-server against the local dev services.
# Sourced by dev-server.sh and dev-node.sh.
MODULA_DATABASE_URL=postgres://modula:modula@127.0.0.1:55432/modula_dev
MODULA_REDIS_URL=redis://:modula@127.0.0.1:56379
MODULA_PUBLIC_URL=${MODULA_PUBLIC_URL:-http://localhost:7100}
MODULA_LISTEN=${MODULA_LISTEN:-127.0.0.1:7100}
MODULA_MASTER_KEY_FILE=.dev/master.key
# Simulated servers are containers; they reach the host as host.docker.internal.
MODULA_NODE_URL=${MODULA_NODE_URL:-https://host.docker.internal:7101}
MODULA_NODE_LISTEN=${MODULA_NODE_LISTEN:-0.0.0.0:7101}
# Generated app domains are <name>.localhost, served over plain HTTP locally.
MODULA_EDGE_TLS=${MODULA_EDGE_TLS:-off}
