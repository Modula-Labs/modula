# Alerts and reports

Alerts tell you when something needs a look: a failed deployment, a crashed
service, a full disk, a server gone quiet, a backup that failed or finished.
The weekly traffic report summarises how your services were used. Both go to
**notification channels**: Slack, Discord, a signed JSON webhook, or email.

Set them up in the workspace's **Settings → Notifications** (channels,
workspace-wide rules, the weekly report, history) or a project's **Settings →
Alerts** (rules scoped to that project or one of its services).

## Channels

| Kind | What you enter | What it gets |
|---|---|---|
| Slack | An incoming webhook URL | A Block Kit message with an "Open in Modula" button |
| Discord | A channel webhook URL | An embed, red while firing and green when resolved |
| Webhook | Any HTTPS URL | Modula's JSON, signed in `X-Modula-Signature` with the secret shown once |
| Email | One email address | A plain-text email with a simple HTML version |

URLs and email addresses are stored encrypted; the dashboard shows only the
host (or a masked address like `m•••@gmail.com`). **Send test** delivers a
test notification right away and shows what the channel answered.

### Email

Email goes out through **Cloudflare Email Service** on the workspace's
connected Cloudflare account (see
[integrations → Cloudflare email](integrations.md#email)). Before an email
channel can deliver:

1. Connect Cloudflare to the workspace with a token that has
   **Email Sending · Edit**.
2. In Cloudflare, onboard the sender's domain: **Compute → Email Service →
   Email Sending → Onboard Domain** (the domain must use Cloudflare DNS;
   Cloudflare adds the records itself).
3. Under **Settings → Integrations → Cloudflare → Email**, set the sender
   address (for example `alerts@example.com`). The page checks the token and
   the domain and says what's missing.
4. On Cloudflare's Workers **Free** plan only verified destination addresses
   receive mail: verify the recipient under **Email Routing → Destination
   addresses**, or use Workers **Paid** to send to anyone.

A failed email delivery shows the reason on the channel and in the history,
for example the missing token permission or an unverified recipient.

## Rules

| Kind | Fires when | Settings |
|---|---|---|
| `deployment_failed` | A deployment fails after its build (pull, start, health check, pre-deploy) | — |
| `build_failed` | A build fails | — |
| `deployment_crashed` | A running service crashes | — |
| `deployment_degraded` | One replica crashes while the others serve | — |
| `backup_failed` | A database backup fails | — |
| `backup_completed` | A database backup finishes: the database, size, duration and destination | — |
| `cpu` | CPU above the threshold (percent of the limit, else of the server's CPUs) for the window | threshold, 1–60 min |
| `memory` | Memory above the threshold (percent of the limit) for the window | threshold, 1–60 min |
| `disk` | A server's disk more than the threshold percent full | threshold |
| `node_offline` | A server out of contact for the window | 1–1440 min |

Each rule notifies its channels at most once per **cooldown** (default 30
minutes) for the same service or server. Metric rules (`cpu`, `memory`,
`disk`, `node_offline`) send a "resolved" message when the condition clears.

### Suggested setup

- **Failures:** one rule each for `deployment_failed`, `build_failed` and
  `deployment_crashed`, workspace-wide.
- **Servers:** `node_offline` after 5 minutes.
- **Storage:** `disk` above 85%. Servers report the filesystems of Modula's
  data directory and of Docker's data (where volumes live). Backups to a
  server's own disk are written under `/var/lib/modula/backups`, which is on
  one of those filesystems in the standard install, so the same rule warns
  before backups run out of space. If you mounted the backup directory on a
  separate disk, that disk isn't reported yet.
- **Backups:** `backup_failed`, and `backup_completed` if you want a note for
  every nightly backup (scheduled backups run once a day, at the hour you
  set).

## Weekly traffic report

Opt-in per workspace. Every **Monday at 09:00** in the timezone you choose
(for example `Asia/Kolkata`, which is 03:30 UTC), the report goes to the
channels you pick. It covers the 7 days before that time, compared with the 7
days before those:

- requests, with the change from the week before;
- error rate (share of 5xx responses), server and client error counts;
- latency p50 and p95;
- the busiest services, with their requests, change, error rate and p95.

It's built from the edge's per-minute request rollups (only traffic through
Modula's edge counts). Rollups are kept 15 days for this; individual requests
are kept 7. **Send now** delivers the report for the last 7 days immediately
without changing the schedule. If the server was down at 09:00, the missed
report is sent once when it's back.

## History

Every notification sent (including tests and reports) is listed with what the
channel answered; the last 100 are shown and a month is kept.

## API

See [API → Alerts](api.md#alerts-and-reports).

```bash
# An email channel
POST /api/workspaces/{workspace_id}/notification-channels
{ "kind": "email", "name": "Me", "url": "me@example.com" }

# Notify on every finished backup
POST /api/workspaces/{workspace_id}/alert-rules
{ "name": "Backups", "kind": "backup_completed", "channelIds": ["<channel id>"] }

# The weekly report, Mondays 09:00 India time
PUT /api/workspaces/{workspace_id}/traffic-report
{ "enabled": true, "timezone": "Asia/Kolkata", "channelIds": ["<channel id>"] }
```
