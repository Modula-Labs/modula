-- C1b: public hostnames for service instances. Generated domains live under the
-- instance's apps domain; custom domains (with ownership verification) are C5.
CREATE TABLE domains (
    id                  uuid PRIMARY KEY,
    service_instance_id uuid NOT NULL REFERENCES service_instances (id) ON DELETE CASCADE,
    hostname            text NOT NULL UNIQUE CHECK (hostname = lower(hostname)),
    kind                text NOT NULL CHECK (kind IN ('generated', 'custom')),
    created_at          timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX domains_instance_idx ON domains (service_instance_id);
