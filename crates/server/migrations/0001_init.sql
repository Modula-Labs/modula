-- Modula Deploy schema.
--
-- Row ids are 16-character, time-sortable base36 strings (see crates/id). The
-- server mints them; new_id() makes the same format for rows created in SQL.

CREATE FUNCTION new_id() RETURNS text
LANGUAGE plpgsql VOLATILE AS $$
DECLARE
    alphabet constant text := '0123456789abcdefghijklmnopqrstuvwxyz';
    ms bigint := floor(extract(epoch FROM clock_timestamp()) * 1000);
    out text := '';
BEGIN
    FOR i IN 1..9 LOOP
        out := substr(alphabet, (ms % 36)::int + 1, 1) || out;
        ms := ms / 36;
    END LOOP;
    FOR i IN 1..7 LOOP
        out := out || substr(alphabet, floor(random() * 36)::int + 1, 1);
    END LOOP;
    RETURN out;
END
$$;

CREATE DOMAIN id AS text CHECK (VALUE ~ '^[0-9a-z]{16}$');

---------------------------------------------------------------------------
-- Instance, accounts, sessions
---------------------------------------------------------------------------

CREATE TABLE instance (
    id                      boolean PRIMARY KEY DEFAULT true CHECK (id),
    setup_token_hash        bytea,
    setup_token_expires_at  timestamptz,
    setup_completed_at      timestamptz,
    created_at              timestamptz NOT NULL DEFAULT now()
);
INSERT INTO instance (id) VALUES (true);

CREATE TABLE users (
    id                  id PRIMARY KEY,
    email               text NOT NULL CHECK (email = lower(email)),
    name                text NOT NULL,
    password_hash       text,
    is_instance_admin   boolean NOT NULL DEFAULT false,
    -- TOTP secrets are sealed with the instance data key; AAD binds them to
    -- (users, column, user id).
    totp_secret              bytea,
    totp_enabled_at          timestamptz,
    -- Highest accepted TOTP time step; codes at or below it are replays.
    totp_last_step           bigint,
    totp_pending_secret      bytea,
    totp_pending_created_at  timestamptz,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT users_totp_consistent CHECK ((totp_secret IS NULL) = (totp_enabled_at IS NULL))
);
CREATE UNIQUE INDEX users_email_key ON users (email);

-- HMAC-SHA256 under a server-held pepper; single use.
CREATE TABLE backup_codes (
    user_id     id NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    code_hash   bytea NOT NULL,
    used_at     timestamptz,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, code_hash)
);

-- Opaque session tokens; only SHA-256 hashes are stored.
-- kind = 'mfa_pending' sessions can only complete a second factor.
CREATE TABLE sessions (
    id              id PRIMARY KEY,
    user_id         id NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    token_hash      bytea NOT NULL UNIQUE,
    kind            text NOT NULL CHECK (kind IN ('full', 'mfa_pending')),
    ip              text,
    user_agent      text,
    created_at      timestamptz NOT NULL DEFAULT now(),
    last_seen_at    timestamptz NOT NULL DEFAULT now(),
    expires_at      timestamptz NOT NULL,
    sudo_until      timestamptz
);
CREATE INDEX sessions_user_idx ON sessions (user_id);
CREATE INDEX sessions_expires_idx ON sessions (expires_at);

CREATE TABLE workspaces (
    id          id PRIMARY KEY,
    name        text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE workspace_members (
    workspace_id    id NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    user_id         id NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    role            text NOT NULL CHECK (role IN ('admin', 'member', 'deployer', 'viewer')),
    created_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, user_id)
);
CREATE INDEX workspace_members_user_idx ON workspace_members (user_id);

-- Per-scope data keys wrapped by the master key. scope = workspace or
-- environment id, or all zeros for instance-level secrets (so no FK).
CREATE TABLE data_keys (
    scope       id PRIMARY KEY,
    kek_id      text NOT NULL,
    wrapped     bytea NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- Append-only. No FKs: entries must outlive the users/workspaces they mention.
CREATE TABLE audit_log (
    id                  bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id        id,
    actor_user_id       id,
    actor_session_id    id,
    action              text NOT NULL,
    target_type         text,
    target_id           text,
    ip                  text,
    user_agent          text,
    metadata            jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at          timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX audit_log_workspace_idx ON audit_log (workspace_id, created_at DESC);
CREATE INDEX audit_log_actor_idx ON audit_log (actor_user_id, created_at DESC);

-- Durable job queue (claimed with FOR UPDATE SKIP LOCKED).
CREATE TABLE jobs (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    kind            text NOT NULL,
    payload         jsonb NOT NULL DEFAULT '{}'::jsonb,
    status          text NOT NULL DEFAULT 'queued'
                    CHECK (status IN ('queued', 'running', 'succeeded', 'failed', 'dead')),
    attempts        integer NOT NULL DEFAULT 0,
    max_attempts    integer NOT NULL DEFAULT 5 CHECK (max_attempts > 0),
    run_at          timestamptz NOT NULL DEFAULT now(),
    locked_by       text,
    locked_until    timestamptz,
    last_error      text,
    dedupe_key      text,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    finished_at     timestamptz
);
CREATE INDEX jobs_ready_idx ON jobs (run_at) WHERE status = 'queued';
CREATE INDEX jobs_running_idx ON jobs (locked_until) WHERE status = 'running';
-- At most one pending job per dedupe key (debounced work like "reconcile node X").
CREATE UNIQUE INDEX jobs_dedupe_idx ON jobs (dedupe_key)
    WHERE dedupe_key IS NOT NULL AND status IN ('queued', 'running');

---------------------------------------------------------------------------
-- Nodes and internal PKI
---------------------------------------------------------------------------

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
    id                  id PRIMARY KEY,
    workspace_id        id REFERENCES workspaces (id) ON DELETE CASCADE,
    name                text NOT NULL,
    status              text NOT NULL DEFAULT 'offline'
                        CHECK (status IN ('online', 'offline', 'revoked')),
    cert_serial         text NOT NULL,
    cert_expires_at     timestamptz NOT NULL,
    node_version        text,
    runtime             text,
    protocol            integer,
    info                jsonb NOT NULL DEFAULT '{}'::jsonb,
    -- Catalog location id (see locations.rs): detected from the node's
    -- provider metadata unless an admin set it (location_pinned).
    location            text,
    location_pinned     boolean NOT NULL DEFAULT false,
    -- Desired-state generation the server wants vs. the one the node applied.
    desired_generation  bigint NOT NULL DEFAULT 1,
    applied_generation  bigint NOT NULL DEFAULT 0,
    last_seen_at        timestamptz,
    created_at          timestamptz NOT NULL DEFAULT now(),
    revoked_at          timestamptz
);
CREATE UNIQUE INDEX nodes_cert_serial_key ON nodes (cert_serial);

CREATE TABLE node_join_tokens (
    id          id PRIMARY KEY,
    token_hash  bytea NOT NULL UNIQUE,
    created_by  id REFERENCES users (id) ON DELETE SET NULL,
    expires_at  timestamptz NOT NULL,
    used_at     timestamptz,
    node_id     id REFERENCES nodes (id) ON DELETE SET NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);

---------------------------------------------------------------------------
-- Projects, environments, services
---------------------------------------------------------------------------

CREATE TABLE projects (
    id              id PRIMARY KEY,
    workspace_id    id NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    name            text NOT NULL,
    description     text NOT NULL DEFAULT '',
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX projects_workspace_idx ON projects (workspace_id);

CREATE TABLE environments (
    id          id PRIMARY KEY,
    project_id  id NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    name        text NOT NULL,
    ephemeral   boolean NOT NULL DEFAULT false,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (project_id, name)
);

-- Service names are DNS labels: they become <name>.modula.internal.
CREATE TABLE services (
    id          id PRIMARY KEY,
    project_id  id NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    name        text NOT NULL CHECK (name ~ '^[a-z]([a-z0-9-]{0,61}[a-z0-9])?$'),
    icon        text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (project_id, name)
);

-- A service's configuration within one environment.
CREATE TABLE service_instances (
    id              id PRIMARY KEY,
    service_id      id NOT NULL REFERENCES services (id) ON DELETE CASCADE,
    environment_id  id NOT NULL REFERENCES environments (id) ON DELETE CASCADE,
    node_id         id REFERENCES nodes (id) ON DELETE SET NULL,
    config          jsonb NOT NULL,
    -- The app's stack (e.g. "nextjs") detected by the last build; drives the icon.
    stack           text,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (service_id, environment_id)
);
CREATE INDEX service_instances_env_idx ON service_instances (environment_id);
CREATE INDEX service_instances_node_idx ON service_instances (node_id);

-- Values sealed with the *environment's* data key (data_keys.scope = environment id;
-- AAD: variables/value/<id>), so environments never share a key.
-- service_instance_id NULL = a shared variable of the environment, used
-- through ${{shared.NAME}} references.
CREATE TABLE variables (
    id                  id PRIMARY KEY,
    environment_id      id NOT NULL REFERENCES environments (id) ON DELETE CASCADE,
    service_instance_id id REFERENCES service_instances (id) ON DELETE CASCADE,
    name                text NOT NULL CHECK (name ~ '^[A-Za-z_][A-Za-z0-9_]{0,254}$'),
    value_sealed        bytea NOT NULL,
    -- Sealed variables are write-only: never returned by the API.
    sealed              boolean NOT NULL DEFAULT false,
    -- The ${{…}} references in the value (`service.NAME`, `shared.NAME` or
    -- `NAME`), kept in clear so dependencies and cycles need no decryption.
    refs                text[] NOT NULL DEFAULT '{}',
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    UNIQUE NULLS NOT DISTINCT (environment_id, service_instance_id, name)
);
CREATE INDEX variables_instance_idx ON variables (service_instance_id);

-- Public hostnames for service instances. Generated domains live under the
-- instance's apps domain; custom domains (with ownership verification) are C5.
CREATE TABLE domains (
    id                  id PRIMARY KEY,
    service_instance_id id NOT NULL REFERENCES service_instances (id) ON DELETE CASCADE,
    hostname            text NOT NULL UNIQUE CHECK (hostname = lower(hostname)),
    kind                text NOT NULL CHECK (kind IN ('generated', 'custom')),
    created_at          timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX domains_instance_idx ON domains (service_instance_id);

---------------------------------------------------------------------------
-- Deployments
---------------------------------------------------------------------------

-- Immutable record of one rollout. config/env are snapshots taken at creation,
-- so a redeploy reproduces exactly what ran.
CREATE TABLE deployments (
    id                  id PRIMARY KEY,
    service_instance_id id NOT NULL REFERENCES service_instances (id) ON DELETE CASCADE,
    node_id             id REFERENCES nodes (id) ON DELETE SET NULL,
    status              text NOT NULL DEFAULT 'queued'
                        CHECK (status IN ('queued', 'building', 'deploying', 'active', 'failed',
                                          'crashed', 'removed', 'cancelled')),
    image               text NOT NULL,
    config              jsonb NOT NULL,
    -- Resolved variables at creation, sealed with the environment's data key
    -- (AAD: deployments/env/<id>).
    env_sealed          bytea,
    trigger             text NOT NULL DEFAULT 'manual'
                        CHECK (trigger IN ('manual', 'push', 'redeploy', 'rollback')),
    -- The deployment this one redeploys or rolls back to.
    source_deployment_id id REFERENCES deployments (id) ON DELETE SET NULL,
    created_by          id REFERENCES users (id) ON DELETE SET NULL,
    error               text,
    -- The commit a repository deployment builds (NULL for image sources).
    commit_sha          text CHECK (commit_sha ~ '^[0-9a-f]{40}$'),
    commit_message      text,
    commit_author       text,
    -- GitHub login of the commit author, for the avatar.
    commit_author_login text,
    -- The built image's id on the node (sha256:…), recorded after the build.
    image_digest        text,
    build_started_at    timestamptz,
    build_finished_at   timestamptz,
    -- Bumped to restart the running containers without a new deployment.
    restart_generation  integer NOT NULL DEFAULT 0,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    finished_at         timestamptz
);
CREATE INDEX deployments_instance_idx ON deployments (service_instance_id, created_at DESC);
CREATE INDEX deployments_node_live_idx ON deployments (node_id)
    WHERE status IN ('building', 'deploying', 'active', 'crashed');
-- At most one active deployment per service instance.
CREATE UNIQUE INDEX deployments_one_active ON deployments (service_instance_id) WHERE status = 'active';

CREATE TABLE deployment_events (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    deployment_id   id NOT NULL REFERENCES deployments (id) ON DELETE CASCADE,
    phase           text NOT NULL,
    message         text NOT NULL DEFAULT '',
    at              timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX deployment_events_dep_idx ON deployment_events (deployment_id, id);

---------------------------------------------------------------------------
-- Configuration changes (the project's activity feed)
---------------------------------------------------------------------------

-- Changes are grouped into sets: staged per environment ("N staged changes ·
-- Deploy"), or applied immediately as one set per request.
CREATE TABLE change_sets (
    id              id PRIMARY KEY,
    environment_id  id NOT NULL REFERENCES environments (id) ON DELETE CASCADE,
    status          text NOT NULL DEFAULT 'staged' CHECK (status IN ('staged', 'applied', 'discarded')),
    created_by      id REFERENCES users (id) ON DELETE SET NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    applied_at      timestamptz,
    applied_by      id REFERENCES users (id) ON DELETE SET NULL
);
CREATE UNIQUE INDEX change_sets_one_open ON change_sets (environment_id) WHERE status = 'staged';
CREATE INDEX change_sets_env_idx ON change_sets (environment_id, created_at DESC);

-- kind: variable | config | service; op: added | updated | removed | created | deleted.
-- key: the variable name or config field. Values are never recorded.
CREATE TABLE changes (
    id                  bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    change_set_id       id NOT NULL REFERENCES change_sets (id) ON DELETE CASCADE,
    service_id          id REFERENCES services (id) ON DELETE CASCADE,
    kind                text NOT NULL,
    op                  text NOT NULL,
    key                 text,
    created_at          timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX changes_set_idx ON changes (change_set_id);

---------------------------------------------------------------------------
-- GitHub App (private repositories, push-to-deploy, commit statuses)
---------------------------------------------------------------------------

-- One App per instance, created through GitHub's manifest flow. Secrets are
-- sealed with the instance data key.
CREATE TABLE github_app (
    id                     boolean PRIMARY KEY DEFAULT true CHECK (id),
    app_id                 bigint NOT NULL,
    slug                   text NOT NULL,
    client_id              text NOT NULL,
    html_url               text NOT NULL,
    owner_login            text,
    private_key_sealed     bytea NOT NULL,
    webhook_secret_sealed  bytea NOT NULL,
    client_secret_sealed   bytea NOT NULL,
    created_by             id REFERENCES users (id) ON DELETE SET NULL,
    created_at             timestamptz NOT NULL DEFAULT now()
);

-- Installations of the App, each bound to one workspace. `id` is GitHub's.
CREATE TABLE github_installations (
    id             bigint PRIMARY KEY,
    workspace_id   id NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    account_login  text NOT NULL,
    account_type   text NOT NULL,
    created_by     id REFERENCES users (id) ON DELETE SET NULL,
    created_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX github_installations_workspace_idx ON github_installations (workspace_id);
CREATE INDEX github_installations_login_idx ON github_installations (lower(account_login));

-- Single-use, short-lived `state` values for the browser round trips through
-- GitHub (App creation, installation), bound to the user who started them.
CREATE TABLE github_flow_states (
    state_hash    bytea PRIMARY KEY,
    kind          text NOT NULL CHECK (kind IN ('manifest', 'install')),
    user_id       id NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    workspace_id  id REFERENCES workspaces (id) ON DELETE CASCADE,
    expires_at    timestamptz NOT NULL,
    CHECK ((kind = 'install') = (workspace_id IS NOT NULL))
);

-- Webhook deliveries already processed (replay protection), pruned after 7 days.
CREATE TABLE github_deliveries (
    id           text PRIMARY KEY,
    received_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX github_deliveries_received_idx ON github_deliveries (received_at);
