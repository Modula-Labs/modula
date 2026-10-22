-- C0a foundations: instance setup, users, sessions, workspaces, data keys, audit log, jobs.

CREATE TABLE instance (
    id                      boolean PRIMARY KEY DEFAULT true CHECK (id),
    setup_token_hash        bytea,
    setup_token_expires_at  timestamptz,
    setup_completed_at      timestamptz,
    created_at              timestamptz NOT NULL DEFAULT now()
);
INSERT INTO instance (id) VALUES (true);

CREATE TABLE users (
    id                  uuid PRIMARY KEY,
    email               text NOT NULL CHECK (email = lower(email)),
    name                text NOT NULL,
    password_hash       text,
    is_instance_admin   boolean NOT NULL DEFAULT false,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX users_email_key ON users (email);

-- Opaque session tokens; only SHA-256 hashes are stored.
-- kind = 'mfa_pending' sessions can only complete a second factor.
CREATE TABLE sessions (
    id              uuid PRIMARY KEY,
    user_id         uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
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
    id          uuid PRIMARY KEY,
    name        text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE workspace_members (
    workspace_id    uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    user_id         uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    role            text NOT NULL CHECK (role IN ('admin', 'member', 'deployer', 'viewer')),
    created_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, user_id)
);
CREATE INDEX workspace_members_user_idx ON workspace_members (user_id);

-- Per-scope data keys wrapped by the master key. scope = workspace id, or the
-- nil uuid for instance-level secrets (so no FK).
CREATE TABLE data_keys (
    scope       uuid PRIMARY KEY,
    kek_id      text NOT NULL,
    wrapped     bytea NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- Append-only. No FKs: entries must outlive the users/workspaces they mention.
CREATE TABLE audit_log (
    id                  bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id        uuid,
    actor_user_id       uuid,
    actor_session_id    uuid,
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
