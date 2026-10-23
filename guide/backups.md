# Backups

Database services (from the PostgreSQL, MySQL, Redis and MongoDB templates)
can be backed up on a daily schedule or on demand to S3-compatible storage
you own. Backups are encrypted before they leave the server.

## How it works

- The server running the database makes the dump, encrypts it with the
  workspace's own [age](https://age-encryption.org) key, and uploads it
  straight to your bucket through presigned URLs. The control plane never sees
  the plaintext.
- Formats: `pg_dump` archive (Postgres), SQL (MySQL), RDB file (Redis),
  gzipped `mongodump` archive (MongoDB).
- Each workspace has its own key pair; the private half is sealed with the
  instance's master key. Keep the [recovery kit](install.md#recovery-kit):
  without the master key, backups can't be decrypted.

## 1. Add a backup target

**Settings → Backup storage → Add backup storage** (Admin). Providers:

| `provider` | Endpoint | Region |
|---|---|---|
| `s3` (AWS) | optional (default `https://s3.<region>.amazonaws.com`) | required |
| `r2` (Cloudflare R2) | required, e.g. `https://<account>.r2.cloudflarestorage.com` | default `auto` |
| `b2` (Backblaze B2) | required | required |
| `minio` | required | default `us-east-1` |

The credentials are checked by writing and deleting a small probe object before
the target is saved. Use a bucket (and optional prefix) dedicated to backups and
a key limited to it.

```bash
curl -fsS -X POST -H "Authorization: Bearer $MODULA_TOKEN" -H 'content-type: application/json' \
  "$MODULA_URL/api/workspaces/$WORKSPACE/backup-targets" -d '{
    "name": "r2-backups", "provider": "r2",
    "endpoint": "https://<account>.r2.cloudflarestorage.com",
    "bucket": "modula-backups", "prefix": "prod",
    "accessKeyId": "…", "secretAccessKey": "…"
  }'
```

## 2. Set a schedule

In the database service's **Backups** tab pick the target, the hour (UTC) and
retention. A new schedule's first backup runs at its next slot; a slot missed
while the server was down still runs the same day.

```bash
curl -fsS -X PUT -H "Authorization: Bearer $MODULA_TOKEN" -H 'content-type: application/json' \
  "$MODULA_URL/api/service-instances/$INSTANCE/backup-schedule" -d '{
    "enabled": true, "targetId": "<target id>", "hourUtc": 3,
    "retention": { "dailyDays": 6, "weeklyDays": 27, "monthlyDays": 89 }
  }'
```

Retention is grandfather-father-son: a backup is kept while it's the newest of
its UTC day (for `dailyDays`), ISO week (`weeklyDays`) or month
(`monthlyDays`). The default (6/27/89) keeps about 7 daily, 4 weekly and 3
monthly backups. Each value is 0–3660. The newest backup is always kept.

## Back up now

**Back up now** (Deployer) uploads to the schedule's target, so set a schedule
(it can be disabled) first. `POST /api/service-instances/{id}/backups`.

## Restore

**Restore** on a completed backup loads it into the service's running database
in the same environment. It needs the **Member** role and a recent
re-authentication, so it can't be started with an API token. Redis backups
can't be restored automatically yet: download the RDB file and load it
manually.

Restoring overwrites data. Take a fresh backup first.

## Delete

**Delete** removes the object from the bucket (Member + sudo). Deleting a
target (Admin + sudo) is only possible when no schedule uses it; its objects
stay in the bucket.

## Backing up the control plane

Instance admins can back up the instance's own Postgres database daily
(**Settings → Backup storage → Control-plane backup**, `PUT /api/instance/self-backup` with
`{ "enabled": true, "targetId": "…", "hourUtc": 2 }`). These dumps are
encrypted to a key derived from the master key, so members of the target's
workspace can see the objects but not read them.
