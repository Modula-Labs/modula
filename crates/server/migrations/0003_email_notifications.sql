-- Email notifications and reports: an `email` channel kind, sent through the workspace's
-- Cloudflare Email Sending (the recipient is sealed in `url_sealed`, the
-- sender lives in the Cloudflare integration's metadata as `emailFrom`).
ALTER TABLE notification_channels DROP CONSTRAINT notification_channels_kind_check;
ALTER TABLE notification_channels ADD CONSTRAINT notification_channels_kind_check
    CHECK (kind IN ('slack', 'discord', 'webhook', 'email'));

-- A finished backup can notify too (the nightly "backup done" email).
ALTER TABLE alert_rules DROP CONSTRAINT alert_rules_kind_check;
ALTER TABLE alert_rules ADD CONSTRAINT alert_rules_kind_check
    CHECK (kind IN ('deployment_failed', 'build_failed', 'deployment_crashed',
                    'deployment_degraded', 'backup_failed', 'backup_completed', 'cpu', 'memory',
                    'disk', 'node_offline'));

-- The weekly traffic report shows in the history as phase 'report'.
ALTER TABLE alert_sends DROP CONSTRAINT alert_sends_phase_check;
ALTER TABLE alert_sends ADD CONSTRAINT alert_sends_phase_check
    CHECK (phase IN ('fired', 'resolved', 'test', 'report'));

-- The weekly traffic report: opt-in per workspace, every Monday 09:00 in
-- `timezone` (an IANA name). `next_at` is the next send in UTC; the minute
-- alert evaluation sends the due ones (see alerts::report).
CREATE TABLE traffic_reports (
    workspace_id  id PRIMARY KEY REFERENCES workspaces (id) ON DELETE CASCADE,
    enabled       boolean NOT NULL DEFAULT true,
    timezone      text NOT NULL DEFAULT 'UTC' CHECK (length(timezone) BETWEEN 1 AND 64),
    next_at       timestamptz NOT NULL,
    last_sent_at  timestamptz,
    updated_by    id REFERENCES users (id) ON DELETE SET NULL,
    updated_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX traffic_reports_due_idx ON traffic_reports (next_at) WHERE enabled;

CREATE TABLE traffic_report_channels (
    workspace_id  id NOT NULL REFERENCES traffic_reports (workspace_id) ON DELETE CASCADE,
    channel_id    id NOT NULL REFERENCES notification_channels (id) ON DELETE CASCADE,
    PRIMARY KEY (workspace_id, channel_id)
);
CREATE INDEX traffic_report_channels_channel_idx ON traffic_report_channels (channel_id);
