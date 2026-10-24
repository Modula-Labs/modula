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
    -- Set in the dashboard after install (the server starts on its IP):
    -- another name for the dashboard, and the parent of generated domains
    -- (MODULA_APPS_DOMAIN until set).
    dashboard_domain        text,
    apps_domain             text,
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

-- id addresses one membership in routes (/api/members/{member_id}).
CREATE TABLE workspace_members (
    id              id NOT NULL UNIQUE DEFAULT new_id(),
    workspace_id    id NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    user_id         id NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    role            text NOT NULL CHECK (role IN ('admin', 'member', 'deployer', 'viewer')),
    created_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, user_id)
);
CREATE INDEX workspace_members_user_idx ON workspace_members (user_id);

-- Single-use invite links. Only the token's hash is stored; the link is
-- shown once, to the admin who created it.
CREATE TABLE workspace_invites (
    id              id PRIMARY KEY,
    workspace_id    id NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    role            text NOT NULL CHECK (role IN ('admin', 'member', 'deployer', 'viewer')),
    token_hash      bytea NOT NULL UNIQUE,
    created_by      id REFERENCES users (id) ON DELETE SET NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    expires_at      timestamptz NOT NULL,
    used_at         timestamptz,
    used_by         id REFERENCES users (id) ON DELETE SET NULL,
    revoked_at      timestamptz,
    CONSTRAINT workspace_invites_used_consistent CHECK (used_by IS NULL OR used_at IS NOT NULL)
);
CREATE INDEX workspace_invites_workspace_idx ON workspace_invites (workspace_id, created_at);

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
    -- Set instead of actor_session_id when the caller used an API token.
    actor_token_id      id,
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
    -- Prefixed "revoked:" on revoke, so the old certificate never matches.
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
    -- The public IPv4 the node reaches the internet from (TCP proxy host).
    public_ip           text,
    -- Private networking: the node's WireGuard public key and UDP port,
    -- its /24 of the mesh (10.80.<mesh_index>.0/24), its own interface
    -- addresses ("ip/prefix", for picking a private path to peers) and its
    -- last report on the tunnels to its peers.
    wg_public_key       text,
    wg_port             integer,
    mesh_index          smallint UNIQUE CHECK (mesh_index BETWEEN 1 AND 254),
    addresses           jsonb NOT NULL DEFAULT '[]'::jsonb,
    mesh_status         jsonb NOT NULL DEFAULT '{}'::jsonb,
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
    -- The node it enrolls belongs to this workspace (Hetzner provisioning);
    -- NULL = shared (an instance admin's manual or CLI token).
    workspace_id id REFERENCES workspaces (id) ON DELETE CASCADE,
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
    -- The template it was created from (icon, Connect, backups): a database,
    -- a self-hosted S3 bucket ('bucket') or a Cloudflare R2 bucket ('r2', no
    -- container: see r2_buckets).
    template    text CHECK (template IN ('postgres', 'mysql', 'redis', 'mongo', 'bucket', 'r2')),
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

-- Persistent storage: one Docker volume per service instance, on the node it
-- runs on (placement sticks to that node). Services with a volume redeploy
-- with a short stop-then-start instead of overlapping.
CREATE TABLE volumes (
    id                  id PRIMARY KEY,
    service_instance_id id NOT NULL UNIQUE REFERENCES service_instances (id) ON DELETE CASCADE,
    mount_path          text NOT NULL CHECK (mount_path ~ '^/[A-Za-z0-9._/-]{0,254}$' AND mount_path NOT LIKE '%..%'),
    created_at          timestamptz NOT NULL DEFAULT now()
);

-- Volumes whose data must be deleted on a node (the volume or its service was
-- deleted). The node removes the Docker volume; rows are pruned after a week.
CREATE TABLE volume_removals (
    volume_id   id PRIMARY KEY,
    node_id     id NOT NULL REFERENCES nodes (id) ON DELETE CASCADE,
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- Public TCP access to a service (databases): node public_port → container target_port.
CREATE TABLE tcp_proxies (
    id                  id PRIMARY KEY,
    service_instance_id id NOT NULL UNIQUE REFERENCES service_instances (id) ON DELETE CASCADE,
    node_id             id NOT NULL REFERENCES nodes (id) ON DELETE CASCADE,
    target_port         integer NOT NULL CHECK (target_port BETWEEN 1 AND 65535),
    public_port         integer NOT NULL CHECK (public_port BETWEEN 20000 AND 29999),
    created_at          timestamptz NOT NULL DEFAULT now(),
    UNIQUE (node_id, public_port)
);

-- Public hostnames for service instances. Generated domains live under the
-- instance's apps domain and are verified from the start. Custom domains start
-- pending: only once a TXT record at _modula-challenge.<hostname> holds their
-- token are they routed (and so allowed a certificate by the node's `ask`).
CREATE TABLE domains (
    id                  id PRIMARY KEY,
    service_instance_id id NOT NULL REFERENCES service_instances (id) ON DELETE CASCADE,
    hostname            text NOT NULL UNIQUE CHECK (hostname = lower(hostname)),
    kind                text NOT NULL CHECK (kind IN ('generated', 'custom')),
    -- Container port to route to; NULL = the service's configured port.
    target_port         integer CHECK (target_port BETWEEN 1 AND 65535),
    verification_token  text,
    status              text NOT NULL DEFAULT 'verified'
                        CHECK (status IN ('pending', 'verified', 'failed')),
    last_error          text,
    -- Whether the CNAME/A record pointed at us on the last check. Informational
    -- only: a proxied (e.g. Cloudflare) record legitimately points elsewhere.
    dns_target_ok       boolean,
    checked_at          timestamptz,
    verified_at         timestamptz,
    -- Edge settings. The rate limit is stored and shown, but the edge does not
    -- enforce it.
    rate_limit_per_minute integer CHECK (rate_limit_per_minute BETWEEN 1 AND 1000000),
    -- Cloudflare ingress mode when the workspace's Cloudflare token manages
    -- the zone: false = DNS only, true = proxied (orange cloud; the zone's SSL
    -- mode must then be Full (strict)).
    proxied             boolean NOT NULL DEFAULT false,
    created_at          timestamptz NOT NULL DEFAULT now(),
    CHECK (kind = 'generated' OR verification_token IS NOT NULL)
);
CREATE INDEX domains_instance_idx ON domains (service_instance_id);

---------------------------------------------------------------------------
-- Deployments
---------------------------------------------------------------------------

-- Source archives uploaded with `modula up` (gzipped tarballs). The
-- bytes live in a file under MODULA_UPLOADS_DIR named after the id; the row
-- outlives the file (purged_at) so deployments keep their history. Only
-- nodes building a deployment of it can download one (signed, short-lived).
CREATE TABLE source_uploads (
    id                  id PRIMARY KEY,
    workspace_id        id NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    service_instance_id id NOT NULL REFERENCES service_instances (id) ON DELETE CASCADE,
    sha256              text NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    size_bytes          bigint NOT NULL CHECK (size_bytes > 0),
    created_by          id REFERENCES users (id) ON DELETE SET NULL,
    created_at          timestamptz NOT NULL DEFAULT now(),
    -- Set when the file was deleted by the retention job.
    purged_at           timestamptz
);
CREATE INDEX source_uploads_instance_idx ON source_uploads (service_instance_id, created_at DESC);
CREATE INDEX source_uploads_kept_idx ON source_uploads (created_at) WHERE purged_at IS NULL;
-- The per-workspace daily upload quota sums the last day's sizes.
CREATE INDEX source_uploads_workspace_idx ON source_uploads (workspace_id, created_at);

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
    -- The instance's own variables as written (references unexpanded) and
    -- whether each was sealed, sealed with the environment's data key (AAD:
    -- deployments/vars/<id>). Discarding a pending change restores from it.
    vars_sealed         bytea,
    -- What the person deploying wrote ("Apply N changes"); not a commit message.
    message             text,
    trigger             text NOT NULL DEFAULT 'manual'
                        CHECK (trigger IN ('manual', 'push', 'redeploy', 'rollback', 'upload')),
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
    -- Uploaded sources (`modula up`): the archive, and the local Git
    -- state it was packed from as the CLI reported it (display only).
    upload_id           id REFERENCES source_uploads (id) ON DELETE SET NULL,
    source_branch       text,
    source_dirty        boolean,
    -- The built image's id on the node (sha256:…), recorded after the build.
    image_digest        text,
    build_started_at    timestamptz,
    build_finished_at   timestamptz,
    -- Bumped to restart the running containers without a new deployment.
    restart_generation  integer NOT NULL DEFAULT 0,
    -- Where a deployment hosted elsewhere (config.hosting) is served, e.g.
    -- https://web-production.acme.workers.dev.
    url                 text,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    finished_at         timestamptz
);
CREATE INDEX deployments_instance_idx ON deployments (service_instance_id, created_at DESC);
CREATE INDEX deployments_node_live_idx ON deployments (node_id)
    WHERE status IN ('building', 'deploying', 'active', 'crashed');
-- At most one active deployment per service instance.
CREATE UNIQUE INDEX deployments_one_active ON deployments (service_instance_id) WHERE status = 'active';

-- Where a deployment runs: one row per replica, each on a different server.
-- Replica 0 is the deployment's own node (deployments.node_id; it builds,
-- serves the TCP proxy, backups and the Database tab) and is created with
-- the deployment by the trigger below.
CREATE TABLE deployment_replicas (
    deployment_id   id NOT NULL REFERENCES deployments (id) ON DELETE CASCADE,
    ordinal         smallint NOT NULL CHECK (ordinal BETWEEN 0 AND 63),
    node_id         id REFERENCES nodes (id) ON DELETE SET NULL,
    -- The container's address on its node's mesh network (in the node's /24).
    mesh_ip         text CHECK (mesh_ip ~ '^10\.80\.[0-9]{1,3}\.[0-9]{1,3}$'),
    status          text NOT NULL DEFAULT 'starting'
                    CHECK (status IN ('starting', 'active', 'crashed', 'failed')),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (deployment_id, ordinal),
    UNIQUE (deployment_id, node_id)
);
CREATE INDEX deployment_replicas_node_idx ON deployment_replicas (node_id);

CREATE FUNCTION deployment_primary_replica() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO deployment_replicas (deployment_id, ordinal, node_id, status)
    VALUES (NEW.id, 0, NEW.node_id, CASE WHEN NEW.status = 'active' THEN 'active' ELSE 'starting' END);
    RETURN NEW;
END
$$;
CREATE TRIGGER deployments_primary_replica AFTER INSERT ON deployments
    FOR EACH ROW EXECUTE FUNCTION deployment_primary_replica();

CREATE TABLE deployment_events (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    deployment_id   id NOT NULL REFERENCES deployments (id) ON DELETE CASCADE,
    phase           text NOT NULL,
    message         text NOT NULL DEFAULT '',
    at              timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX deployment_events_dep_idx ON deployment_events (deployment_id, id);

-- Resource usage samples from the nodes (every 30s per running deployment),
-- kept a week for the dashboard's charts.
CREATE TABLE metric_samples (
    deployment_id        id NOT NULL REFERENCES deployments (id) ON DELETE CASCADE,
    service_instance_id  id NOT NULL REFERENCES service_instances (id) ON DELETE CASCADE,
    at                   timestamptz NOT NULL,
    cpu_percent          real NOT NULL,
    memory_bytes         bigint NOT NULL,
    net_rx_bps           real NOT NULL,
    net_tx_bps           real NOT NULL
);
CREATE INDEX metric_samples_instance_at_idx ON metric_samples (service_instance_id, at);
CREATE INDEX metric_samples_at_idx ON metric_samples (at);

-- Every running workload's output, shipped by the nodes as it happens and
-- kept a week: logs outlive the containers (each deploy replaces them) and
-- are searchable across an environment.
CREATE TABLE log_lines (
    environment_id       id NOT NULL REFERENCES environments (id) ON DELETE CASCADE,
    service_instance_id  id NOT NULL REFERENCES service_instances (id) ON DELETE CASCADE,
    deployment_id        id NOT NULL REFERENCES deployments (id) ON DELETE CASCADE,
    at                   timestamptz NOT NULL,
    stderr               boolean NOT NULL,
    line                 text NOT NULL
);
CREATE INDEX log_lines_environment_at_idx ON log_lines (environment_id, at);
CREATE INDEX log_lines_at_idx ON log_lines (at);
CREATE INDEX log_lines_deployment_idx ON log_lines (deployment_id);

-- A project's outgoing webhooks: deployment events POSTed as JSON, signed
-- with HMAC-SHA256 of the body (X-Modula-Signature: sha256=<hex>) under a
-- secret shown once at creation and stored sealed.
CREATE TABLE webhooks (
    id             id PRIMARY KEY,
    project_id     id NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    url            text NOT NULL,
    events         text[] NOT NULL,
    secret_sealed  bytea NOT NULL,
    created_by     id REFERENCES users (id) ON DELETE SET NULL,
    created_at     timestamptz NOT NULL DEFAULT now(),
    last_status    integer,
    last_error     text,
    last_at        timestamptz
);
CREATE INDEX webhooks_project_idx ON webhooks (project_id);

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

-- GitHub Apps, created through GitHub's manifest flow: one per GitHub account
-- or organization (a private App installs only on the account that owns it).
-- Secrets are sealed with the instance data key.
CREATE TABLE github_apps (
    id                     bigint PRIMARY KEY,  -- GitHub's App id
    slug                   text NOT NULL,
    client_id              text NOT NULL,
    html_url               text NOT NULL,
    owner_login            text,
    public                 boolean NOT NULL DEFAULT false,
    private_key_sealed     bytea NOT NULL,
    webhook_secret_sealed  bytea NOT NULL,
    client_secret_sealed   bytea NOT NULL,
    created_by             id REFERENCES users (id) ON DELETE SET NULL,
    created_at             timestamptz NOT NULL DEFAULT now()
);

-- Installations of the App, each bound to one workspace. `id` is GitHub's.
CREATE TABLE github_installations (
    id             bigint PRIMARY KEY,
    app_id         bigint NOT NULL REFERENCES github_apps (id) ON DELETE CASCADE,
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
-- An App creation started from a workspace carries on to installing it there;
-- `return_to` is the dashboard path to come back to (validated, same origin).
CREATE TABLE github_flow_states (
    state_hash    bytea PRIMARY KEY,
    kind          text NOT NULL CHECK (kind IN ('manifest', 'install')),
    user_id       id NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    workspace_id  id REFERENCES workspaces (id) ON DELETE CASCADE,
    -- The App being installed ('install' flows).
    app_id        bigint REFERENCES github_apps (id) ON DELETE CASCADE,
    return_to     text CHECK (left(return_to, 1) = '/' AND length(return_to) <= 512),
    expires_at    timestamptz NOT NULL,
    CHECK (kind = 'manifest' OR (workspace_id IS NOT NULL AND app_id IS NOT NULL))
);

-- Webhook deliveries already processed (replay protection), pruned after 7 days.
CREATE TABLE github_deliveries (
    id           text PRIMARY KEY,
    received_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX github_deliveries_received_idx ON github_deliveries (received_at);

---------------------------------------------------------------------------
-- Backups
---------------------------------------------------------------------------

-- S3-compatible storage (AWS S3, Cloudflare R2, Backblaze B2, MinIO) for a
-- workspace's backups. The secret key is sealed with the workspace data key
-- (AAD: backup_targets/secret_access_key/<id>) and never returned.
CREATE TABLE backup_targets (
    id                  id PRIMARY KEY,
    workspace_id        id NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    name                text NOT NULL,
    provider            text NOT NULL CHECK (provider IN ('s3', 'r2', 'b2', 'minio')),
    endpoint            text NOT NULL,
    region              text NOT NULL,
    bucket              text NOT NULL,
    -- Object key prefix: empty, or ends with '/'.
    prefix              text NOT NULL DEFAULT '',
    access_key_id       text NOT NULL,
    secret_sealed       bytea NOT NULL,
    created_by          id REFERENCES users (id) ON DELETE SET NULL,
    created_at          timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, name)
);

-- The age X25519 key pair a workspace's backups are encrypted to. The
-- identity is sealed with the workspace data key (AAD: backup_keys/identity/<workspace>).
CREATE TABLE backup_keys (
    workspace_id        id PRIMARY KEY REFERENCES workspaces (id) ON DELETE CASCADE,
    recipient           text NOT NULL,
    identity_sealed     bytea NOT NULL,
    created_at          timestamptz NOT NULL DEFAULT now()
);

-- Daily backups of a database service instance, with GFS retention: the
-- newest backup of each day/ISO week/month is kept for that many days.
CREATE TABLE backup_schedules (
    service_instance_id id PRIMARY KEY REFERENCES service_instances (id) ON DELETE CASCADE,
    enabled             boolean NOT NULL DEFAULT true,
    target_id           id NOT NULL REFERENCES backup_targets (id) ON DELETE CASCADE,
    hour_utc            smallint NOT NULL CHECK (hour_utc BETWEEN 0 AND 23),
    daily_days          integer NOT NULL DEFAULT 6 CHECK (daily_days BETWEEN 0 AND 3660),
    weekly_days         integer NOT NULL DEFAULT 27 CHECK (weekly_days BETWEEN 0 AND 3660),
    monthly_days        integer NOT NULL DEFAULT 89 CHECK (monthly_days BETWEEN 0 AND 3660),
    last_scheduled_at   timestamptz,
    updated_at          timestamptz NOT NULL DEFAULT now()
);

-- One encrypted object in a target. service_instance_id NULL = a backup of
-- the control plane's own database (see instance_backup). Deleting a service
-- or target deletes these records but leaves the objects in the bucket.
CREATE TABLE backups (
    id                  id PRIMARY KEY,
    service_instance_id id REFERENCES service_instances (id) ON DELETE CASCADE,
    target_id           id NOT NULL REFERENCES backup_targets (id) ON DELETE CASCADE,
    -- The node running the dump (whose result alone is accepted).
    node_id             id REFERENCES nodes (id) ON DELETE SET NULL,
    kind                text NOT NULL CHECK (kind IN ('scheduled', 'manual')),
    engine              text NOT NULL CHECK (engine IN ('postgres', 'mysql', 'redis', 'mongo')),
    status              text NOT NULL DEFAULT 'running'
                        CHECK (status IN ('running', 'completed', 'failed', 'deleted')),
    object_key          text NOT NULL,
    -- The S3 multipart upload while running.
    upload_id           text,
    size_bytes          bigint,
    error               text,
    created_by          id REFERENCES users (id) ON DELETE SET NULL,
    started_at          timestamptz NOT NULL DEFAULT now(),
    finished_at         timestamptz,
    deleted_at          timestamptz
);
CREATE INDEX backups_instance_idx ON backups (service_instance_id, started_at DESC);
CREATE INDEX backups_running_idx ON backups (started_at) WHERE status = 'running';
-- One backup at a time per service instance (and for the control plane).
CREATE UNIQUE INDEX backups_one_running ON backups (service_instance_id) NULLS NOT DISTINCT
    WHERE status = 'running';

CREATE TABLE backup_restores (
    id                  id PRIMARY KEY,
    backup_id           id NOT NULL REFERENCES backups (id) ON DELETE CASCADE,
    service_instance_id id NOT NULL REFERENCES service_instances (id) ON DELETE CASCADE,
    node_id             id REFERENCES nodes (id) ON DELETE SET NULL,
    status              text NOT NULL DEFAULT 'running'
                        CHECK (status IN ('running', 'completed', 'failed')),
    error               text,
    created_by          id REFERENCES users (id) ON DELETE SET NULL,
    started_at          timestamptz NOT NULL DEFAULT now(),
    finished_at         timestamptz
);
CREATE INDEX backup_restores_instance_idx ON backup_restores (service_instance_id, started_at DESC);
CREATE UNIQUE INDEX backup_restores_one_running ON backup_restores (service_instance_id)
    WHERE status = 'running';

-- Daily backups of the control plane's database (single row), encrypted to a
-- key derived from the master key so they decrypt without this database.
CREATE TABLE instance_backup (
    id                  boolean PRIMARY KEY DEFAULT true CHECK (id),
    enabled             boolean NOT NULL DEFAULT false,
    target_id           id REFERENCES backup_targets (id) ON DELETE SET NULL,
    hour_utc            smallint NOT NULL DEFAULT 3 CHECK (hour_utc BETWEEN 0 AND 23),
    last_scheduled_at   timestamptz,
    updated_by          id REFERENCES users (id) ON DELETE SET NULL,
    updated_at          timestamptz NOT NULL DEFAULT now()
);
INSERT INTO instance_backup (id) VALUES (true);

---------------------------------------------------------------------------
-- API tokens (the modula CLI, automation)
---------------------------------------------------------------------------

-- Personal bearer tokens (mdt_…); only SHA-256 hashes are stored. A token acts
-- as its user without sudo, limited to one workspace when workspace_id is set.
CREATE TABLE api_tokens (
    id              id PRIMARY KEY,
    user_id         id NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    workspace_id    id REFERENCES workspaces (id) ON DELETE CASCADE,
    name            text NOT NULL,
    token_hash      bytea NOT NULL UNIQUE,
    last_used_at    timestamptz,
    expires_at      timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX api_tokens_user_idx ON api_tokens (user_id, created_at);

---------------------------------------------------------------------------
-- Integrations (Hetzner, Cloudflare): one per kind per workspace
---------------------------------------------------------------------------

-- The provider's API token is sealed with the workspace data key
-- (AAD: integrations/secret/<id>) and never returned. metadata holds
-- non-secret settings (e.g. Hetzner provisioning caps).
CREATE TABLE integrations (
    id              id PRIMARY KEY,
    workspace_id    id NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    kind            text NOT NULL,
    secret_sealed   bytea NOT NULL,
    metadata        jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_by      id REFERENCES users (id) ON DELETE SET NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, kind)
);

-- The Cloudflare R2 bucket behind an instance of an 'r2' service, and the
-- account-owned API token scoped to it whose derived S3 credentials are the
-- instance's variables. An instance without a row has no bucket yet.
-- `created`: Modula created the bucket (only then may it delete it).
CREATE TABLE r2_buckets (
    service_instance_id id PRIMARY KEY REFERENCES service_instances (id) ON DELETE CASCADE,
    account_id          text NOT NULL,
    bucket              text NOT NULL CHECK (bucket ~ '^[a-z0-9][a-z0-9-]{1,61}[a-z0-9]$'),
    token_id            text NOT NULL,
    created             boolean NOT NULL,
    created_at          timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX r2_buckets_bucket_idx ON r2_buckets (account_id, bucket);

---------------------------------------------------------------------------
-- Provisioning: servers created at a cloud provider on a user's request
---------------------------------------------------------------------------

-- One server created through a provider API. It joins with a single-use join
-- token (join_token_id); enrollment records the node on that token, which is
-- how the row learns its node_id. Prices are the provider's gross monthly
-- price in euro cents.
CREATE TABLE provisioned_servers (
    id                  id PRIMARY KEY,
    workspace_id        id NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    provider            text NOT NULL CHECK (provider IN ('hetzner')),
    -- The provider's server id, once created.
    provider_id         text,
    -- The provider's create action, followed until the server boots.
    provider_action_id  text,
    firewall_id         text,
    name                text NOT NULL,
    -- Catalog location id (locations.rs), e.g. hetzner:fsn1.
    location            text NOT NULL,
    server_type         text NOT NULL,
    monthly_price_cents integer NOT NULL CHECK (monthly_price_cents >= 0),
    status              text NOT NULL DEFAULT 'creating'
                        CHECK (status IN ('creating', 'booting', 'joining', 'ready', 'failed', 'deleting')),
    error               text,
    ssh_enabled         boolean NOT NULL DEFAULT false,
    ipv4                text,
    join_token_id       id REFERENCES node_join_tokens (id) ON DELETE SET NULL,
    node_id             id REFERENCES nodes (id) ON DELETE SET NULL,
    created_by          id REFERENCES users (id) ON DELETE SET NULL,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    UNIQUE (provider, provider_id)
);
CREATE INDEX provisioned_servers_workspace_idx ON provisioned_servers (workspace_id, created_at);

---------------------------------------------------------------------------
-- Updates: new releases, upgrades of the control plane and of the servers
---------------------------------------------------------------------------

-- One row. mode: off = no checks; notify = check daily and show it; auto =
-- also upgrade in the maintenance window. window_days is a bitmask of
-- weekdays (bit 0 = Monday … bit 6 = Sunday); the window opens window_minute
-- minutes after midnight in timezone (empty = the server's own) for an hour.
CREATE TABLE instance_updates (
    id                  boolean PRIMARY KEY DEFAULT true CHECK (id),
    mode                text NOT NULL DEFAULT 'notify' CHECK (mode IN ('off', 'notify', 'auto')),
    channel             text NOT NULL DEFAULT 'stable' CHECK (channel IN ('stable')),
    window_days         smallint NOT NULL DEFAULT 64 CHECK (window_days BETWEEN 1 AND 127),
    window_minute       smallint NOT NULL DEFAULT 180 CHECK (window_minute BETWEEN 0 AND 1439),
    timezone            text NOT NULL DEFAULT '',
    -- The newest release (signed VERSION file) and when it was last checked.
    latest_version      text,
    checked_at          timestamptz,
    check_error         text,
    -- The newest version the audit log announced (once per version).
    notified_version    text,
    -- Server upgrades that failed before this are retried (rollout resumed).
    rollout_cleared_at  timestamptz,
    updated_by          id REFERENCES users (id) ON DELETE SET NULL,
    updated_at          timestamptz NOT NULL DEFAULT now()
);
INSERT INTO instance_updates (id) VALUES (true);

-- Upgrades of the control plane requested from the dashboard ("Update now")
-- or in the maintenance window. install.sh runs them (modula-upgrade unit)
-- and reports back; log is the tail of its output.
CREATE TABLE instance_upgrades (
    id              id PRIMARY KEY,
    from_version    text NOT NULL,
    to_version      text NOT NULL,
    trigger         text NOT NULL CHECK (trigger IN ('manual', 'auto')),
    status          text NOT NULL DEFAULT 'requested'
                    CHECK (status IN ('requested', 'succeeded', 'failed', 'rolled_back')),
    log             text,
    requested_by    id REFERENCES users (id) ON DELETE SET NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    finished_at     timestamptz
);
CREATE INDEX instance_upgrades_created_idx ON instance_upgrades (created_at);

-- Rolling upgrades of the other servers to the control plane's version,
-- one at a time (updates::rollout).
CREATE TABLE node_upgrades (
    id              id PRIMARY KEY,
    node_id         id NOT NULL REFERENCES nodes (id) ON DELETE CASCADE,
    from_version    text,
    to_version      text NOT NULL,
    status          text NOT NULL DEFAULT 'running' CHECK (status IN ('running', 'succeeded', 'failed')),
    error           text,
    started_at      timestamptz NOT NULL DEFAULT now(),
    finished_at     timestamptz
);
CREATE INDEX node_upgrades_node_idx ON node_upgrades (node_id, started_at);

---------------------------------------------------------------------------
-- Private dashboard: Cloudflare Tunnel + Access in front of the dashboard,
-- the API and the node endpoint (crate::private)
---------------------------------------------------------------------------

-- One row. phase: off → enabling → on → disabling → off; a failed enable
-- rolls back to off (error says why). Every Cloudflare object created is
-- recorded as soon as it exists, so a rollback or a re-run finds it.
-- Secrets are sealed with the data key of workspace_id (whose Cloudflare
-- connection is used), AAD table "private_access".
CREATE TABLE private_access (
    id                      boolean PRIMARY KEY DEFAULT true CHECK (id),
    phase                   text NOT NULL DEFAULT 'off'
                            CHECK (phase IN ('off', 'enabling', 'on', 'disabling')),
    workspace_id            id REFERENCES workspaces (id) ON DELETE SET NULL,
    account_id              text,
    zone_id                 text,
    dashboard_host          text,
    node_host               text,
    -- Who may sign in at Cloudflare Access.
    allowed_emails          text[] NOT NULL DEFAULT '{}',
    allowed_domains         text[] NOT NULL DEFAULT '{}',
    -- e.g. acme.cloudflareaccess.com (JWT issuer, signing keys).
    team_domain             text,
    tunnel_id               text,
    tunnel_token_sealed     bytea,
    dashboard_app_id        text,
    dashboard_aud           text,
    bypass_app_id           text,
    node_app_id             text,
    node_token_id           text,
    node_token_client_id    text,
    node_token_secret_sealed bytea,
    -- The node that runs this control plane (it runs cloudflared).
    main_node_id            id REFERENCES nodes (id) ON DELETE SET NULL,
    -- The dashboard's DNS records before the switch, restored on disable.
    previous_dns            jsonb,
    dns_switched            boolean NOT NULL DEFAULT false,
    node_dns_created        boolean NOT NULL DEFAULT false,
    -- Servers were asked to change (cloudflared, routes); undone on teardown.
    nodes_configured        boolean NOT NULL DEFAULT false,
    -- Progress of the current enable/disable, for the dashboard.
    steps                   jsonb NOT NULL DEFAULT '[]'::jsonb,
    error                   text,
    updated_by              id REFERENCES users (id) ON DELETE SET NULL,
    updated_at              timestamptz NOT NULL DEFAULT now()
);
INSERT INTO private_access (id) VALUES (true);
