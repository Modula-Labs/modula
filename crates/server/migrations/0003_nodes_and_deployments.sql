-- C1a: nodes (modula-node), internal PKI, and the project/environment/service/deployment model.

-- Internal CA (single row). Keys are sealed with the instance data key.
CREATE TABLE pki (
    id          boolean PRIMARY KEY DEFAULT true CHECK (id),
    root_cert   bytea NOT NULL,
    root_key    bytea NOT NULL,
    int_cert    bytea NOT NULL,
    int_key     bytea NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- Nodes are instance-level in self-hosted mode (workspace_id NULL = shared).
CREATE TABLE nodes (
    id                  uuid PRIMARY KEY,
    workspace_id        uuid REFERENCES workspaces (id) ON DELETE CASCADE,
    name                text NOT NULL,
    status              text NOT NULL DEFAULT 'offline'
                        CHECK (status IN ('online', 'offline', 'revoked')),
    cert_serial         text NOT NULL,
    cert_expires_at     timestamptz NOT NULL,
    node_version        text,
    runtime             text,
    protocol            integer,
    info                jsonb NOT NULL DEFAULT '{}'::jsonb,
    -- Desired-state generation the server wants vs. the one the node applied.
    desired_generation  bigint NOT NULL DEFAULT 1,
    applied_generation  bigint NOT NULL DEFAULT 0,
    last_seen_at        timestamptz,
    created_at          timestamptz NOT NULL DEFAULT now(),
    revoked_at          timestamptz
);

CREATE UNIQUE INDEX nodes_cert_serial_key ON nodes (cert_serial);

CREATE TABLE node_join_tokens (
    id          uuid PRIMARY KEY,
    token_hash  bytea NOT NULL UNIQUE,
    created_by  uuid REFERENCES users (id) ON DELETE SET NULL,
    expires_at  timestamptz NOT NULL,
    used_at     timestamptz,
    node_id     uuid REFERENCES nodes (id) ON DELETE SET NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE projects (
    id              uuid PRIMARY KEY,
    workspace_id    uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    name            text NOT NULL,
    description     text NOT NULL DEFAULT '',
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX projects_workspace_idx ON projects (workspace_id);

CREATE TABLE environments (
    id          uuid PRIMARY KEY,
    project_id  uuid NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    name        text NOT NULL,
    ephemeral   boolean NOT NULL DEFAULT false,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (project_id, name)
);

-- Service names are DNS labels: they become <name>.modula.internal.
CREATE TABLE services (
    id          uuid PRIMARY KEY,
    project_id  uuid NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    name        text NOT NULL CHECK (name ~ '^[a-z]([a-z0-9-]{0,61}[a-z0-9])?$'),
    icon        text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (project_id, name)
);

-- A service's configuration within one environment.
CREATE TABLE service_instances (
    id              uuid PRIMARY KEY,
    service_id      uuid NOT NULL REFERENCES services (id) ON DELETE CASCADE,
    environment_id  uuid NOT NULL REFERENCES environments (id) ON DELETE CASCADE,
    node_id         uuid REFERENCES nodes (id) ON DELETE SET NULL,
    config          jsonb NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (service_id, environment_id)
);
CREATE INDEX service_instances_env_idx ON service_instances (environment_id);
CREATE INDEX service_instances_node_idx ON service_instances (node_id);

-- Values sealed with the *environment's* data key (data_keys.scope = environment id;
-- AAD: variables/value/<id>), so environments never share a key.
CREATE TABLE variables (
    id                  uuid PRIMARY KEY,
    service_instance_id uuid NOT NULL REFERENCES service_instances (id) ON DELETE CASCADE,
    name                text NOT NULL CHECK (name ~ '^[A-Za-z_][A-Za-z0-9_]{0,254}$'),
    value_sealed        bytea NOT NULL,
    -- Sealed variables are write-only: never returned by the API.
    sealed              boolean NOT NULL DEFAULT false,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    UNIQUE (service_instance_id, name)
);

-- Immutable record of one rollout. config/env are snapshots taken at creation,
-- so a redeploy or rollback reproduces exactly what ran.
CREATE TABLE deployments (
    id                  uuid PRIMARY KEY,
    service_instance_id uuid NOT NULL REFERENCES service_instances (id) ON DELETE CASCADE,
    node_id             uuid REFERENCES nodes (id) ON DELETE SET NULL,
    status              text NOT NULL DEFAULT 'queued'
                        CHECK (status IN ('queued', 'deploying', 'active', 'failed',
                                          'crashed', 'removed', 'cancelled')),
    image               text NOT NULL,
    config              jsonb NOT NULL,
    -- Resolved variables at creation, sealed with the environment's data key
    -- (AAD: deployments/env/<id>).
    env_sealed          bytea,
    trigger             text NOT NULL DEFAULT 'manual',
    created_by          uuid REFERENCES users (id) ON DELETE SET NULL,
    error               text,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    finished_at         timestamptz
);
CREATE INDEX deployments_instance_idx ON deployments (service_instance_id, created_at DESC);
CREATE INDEX deployments_node_live_idx ON deployments (node_id) WHERE status IN ('deploying', 'active');
-- At most one active deployment per service instance.
CREATE UNIQUE INDEX deployments_one_active ON deployments (service_instance_id) WHERE status = 'active';

CREATE TABLE deployment_events (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    deployment_id   uuid NOT NULL REFERENCES deployments (id) ON DELETE CASCADE,
    phase           text NOT NULL,
    message         text NOT NULL DEFAULT '',
    at              timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX deployment_events_dep_idx ON deployment_events (deployment_id, id);

-- Staged configuration changes per environment ("N staged changes · Deploy").
CREATE TABLE change_sets (
    id              uuid PRIMARY KEY,
    environment_id  uuid NOT NULL REFERENCES environments (id) ON DELETE CASCADE,
    status          text NOT NULL DEFAULT 'staged' CHECK (status IN ('staged', 'applied', 'discarded')),
    created_by      uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    applied_at      timestamptz,
    applied_by      uuid REFERENCES users (id) ON DELETE SET NULL
);
CREATE UNIQUE INDEX change_sets_one_open ON change_sets (environment_id) WHERE status = 'staged';

CREATE TABLE changes (
    id                  bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    change_set_id       uuid NOT NULL REFERENCES change_sets (id) ON DELETE CASCADE,
    service_instance_id uuid REFERENCES service_instances (id) ON DELETE CASCADE,
    kind                text NOT NULL,
    summary             jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at          timestamptz NOT NULL DEFAULT now()
);
