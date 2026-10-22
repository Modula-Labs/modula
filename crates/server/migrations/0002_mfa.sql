-- C0b: TOTP second factor and backup codes.
-- Secrets are sealed with the instance data key; AAD binds them to (users, column, user id).

ALTER TABLE users
    ADD COLUMN totp_secret              bytea,
    ADD COLUMN totp_enabled_at          timestamptz,
    -- Highest accepted TOTP time step; codes at or below it are replays.
    ADD COLUMN totp_last_step           bigint,
    ADD COLUMN totp_pending_secret      bytea,
    ADD COLUMN totp_pending_created_at  timestamptz,
    ADD CONSTRAINT users_totp_consistent
        CHECK ((totp_secret IS NULL) = (totp_enabled_at IS NULL));

-- HMAC-SHA256 under a server-held pepper; single use.
CREATE TABLE backup_codes (
    user_id     uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    code_hash   bytea NOT NULL,
    used_at     timestamptz,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, code_hash)
);
