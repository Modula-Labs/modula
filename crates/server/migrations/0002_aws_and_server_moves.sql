-- AWS as a second server provider; server migrations to existing servers,
-- with DNS following the moved server.

-- Prices of provisioned servers are the monthly price in USD cents as billed
-- (Hetzner's gross euros converted at MODULA_EUR_USD_RATE). Rows from before
-- held euro cents: converted at the default rate.
ALTER TABLE provisioned_servers DROP CONSTRAINT provisioned_servers_provider_check;
ALTER TABLE provisioned_servers ADD CONSTRAINT provisioned_servers_provider_check
    CHECK (provider IN ('hetzner', 'aws'));
UPDATE provisioned_servers SET monthly_price_cents = round(monthly_price_cents * 1.08);

-- Catalog location id and provider server type of the target server;
-- unknown (NULL) for an existing target that isn't at a provider.
ALTER TABLE server_migrations ALTER COLUMN target_location DROP NOT NULL;
ALTER TABLE server_migrations ALTER COLUMN target_server_type DROP NOT NULL;
-- The target already existed: nothing here ever deletes it. Then
-- target_server_id stays NULL and target_node_id is set from the start.
ALTER TABLE server_migrations ADD COLUMN target_existing boolean NOT NULL DEFAULT false;
ALTER TABLE server_migrations DROP CONSTRAINT server_migrations_step_check;
ALTER TABLE server_migrations ADD CONSTRAINT server_migrations_step_check
    CHECK (step IN ('create', 'join', 'move', 'promote', 'dns', 'finish'));
-- Step dns (migrate/dns.rs): the old server's address when the migration
-- began, the DNS record ids the user opted out of repointing, the records
-- changed so far ({zoneId, zone, recordId, name, type, from, to, action}),
-- and the retries. A failure leaves step = 'dns' for a retry.
ALTER TABLE server_migrations ADD COLUMN source_ip text;
ALTER TABLE server_migrations ADD COLUMN dns_skip jsonb NOT NULL DEFAULT '[]'::jsonb;
ALTER TABLE server_migrations ADD COLUMN dns_changes jsonb NOT NULL DEFAULT '[]'::jsonb;
ALTER TABLE server_migrations ADD COLUMN dns_attempts integer NOT NULL DEFAULT 0;
ALTER TABLE server_migrations ADD COLUMN dns_next_at timestamptz;
-- Since when the step before the first copy has been waiting for the
-- target's address or the provider firewalls (migrate/server.rs).
ALTER TABLE server_migrations ADD COLUMN mesh_wait_since timestamptz;
-- Services whose variables still hold a moved TCP proxy's old address
-- (they were resolved at deploy time): they need a redeploy.
ALTER TABLE server_migrations ADD COLUMN stale_tcp jsonb NOT NULL DEFAULT '[]'::jsonb;

-- The TCP proxy's public port before the switch (it can change).
ALTER TABLE instance_moves ADD COLUMN old_tcp_port integer;
