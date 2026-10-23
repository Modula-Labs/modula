# Domains

Services are private until you give them a domain. Every server runs Caddy as
its edge: it routes each hostname to the service's active deployment and gets
HTTPS certificates automatically.

## Generated domains

**Settings → Networking → Generate domain** gives a service a hostname under
your instance's apps domain, routed immediately:

```
<service>-<random>.apps.example.com             (production)
<service>-<environment>-<random>.apps.example.com
```

- Requires the instance to have an apps domain (`--apps-domain` at install,
  `MODULA_APPS_DOMAIN`) with a wildcard DNS record `*.apps.example.com` pointing
  at your server(s). Without it, generating fails with
  `generated domains are off: set MODULA_APPS_DOMAIN`.
- Up to 5 generated domains per service instance.
- The service's first domain is available to it (and to others by reference)
  as `MODULA_PUBLIC_DOMAIN`.

```bash
curl -fsS -X POST -H "Authorization: Bearer $MODULA_TOKEN" \
  "$MODULA_URL/api/service-instances/$INSTANCE/domains"
```

## Custom domains

**Settings → Networking → Custom domain**, enter `api.example.com`. A custom
domain is only routed (and only gets a certificate) after you prove you own it.

1. Add the domain. The dashboard shows the DNS records to create:

   | Type | Name | Value |
   |---|---|---|
   | `CNAME` | `api.example.com` | the service's generated domain, e.g. `api-x7k2.apps.example.com` |
   | `TXT` | `_modula-challenge.api.example.com` | `modula-verify=<32 hex characters>` |

   For an apex domain (`example.com`), or a service without a generated domain,
   the routing record is an `A` record to each server running the service
   instead of a CNAME. (Names like `example.co.uk` are apex domains too; use your
   DNS host's ALIAS or CNAME flattening.)
2. Create the records at your DNS host.
3. Click **Check now**, or wait: domains are re-checked every minute for 7
   days. Status goes `pending` → `verified` (or `failed` if the TXT record has a
   different value).
4. Once verified, the edge routes the name and gets a certificate on the first
   HTTPS request.

Rules:

- Ownership is proven by the TXT record alone. Whether the CNAME/A record
  points at us is checked and shown (`dnsTargetOk`), but a proxied record
  (Cloudflare's orange cloud) legitimately points elsewhere.
- A hostname can belong to only one service on the instance. Up to 20 custom
  domains per service instance.
- Names under the apps domain, the dashboard's own hostname, the TCP proxy host,
  IP addresses and `.localhost`/`.local`/`.internal` names are rejected. Enter
  internationalized names in punycode (`xn--…`).
- Wildcard domains (`*.example.com`) need DNS-01 certificates, which aren't
  available yet (not even with Cloudflare connected). Add each subdomain
  instead.

With [Cloudflare connected](integrations.md#cloudflare) to the workspace and
the domain's zone in its account, steps 1–2 happen for you: the records are
written at Cloudflare (DNS only) when you add the domain, and removed when you
delete it. If that fails, the domain carries a `providerNote` and you create
the records by hand.

### Domain settings

- **Target port**: the container port the domain routes to (default: the
  service's `port`). Lets one service serve several ports on different names.
- **Rate limit**: requests per minute per domain (1–1,000,000). Stored now, but
  only enforced once the edge's Caddy build includes the rate-limit module; the
  API reports `rateLimitEnforced`.

## Behind Cloudflare

To put a custom domain behind Cloudflare's proxy:

1. Have the CNAME (or A) record as **DNS only** (grey cloud) plus the TXT
   record (written for you when Cloudflare is connected). Wait until the
   domain shows `verified` and `https://api.example.com` serves your app with
   a valid certificate.
2. Switch the routing record to **Proxied** (orange cloud): in Cloudflare by
   hand, or with Cloudflare connected, `PUT /api/domains/{id}/ingress`
   `{ "proxied": true }`.
3. In Cloudflare **SSL/TLS → Overview**, set the mode to **Full (strict)**. The
   origin has a real Let's Encrypt certificate, so strict validation works.
   Never use **Flexible**: it sends traffic to your server over plain HTTP and
   HTTPS redirects loop. With Cloudflare connected,
   `GET /api/domains/{id}/ingress` reports the zone's mode and an
   `sslWarning` if it isn't Full (strict).

Keep the TXT record in place; it's re-checked. Locking the origin to
Cloudflare's IPs and Cloudflare Tunnel ingress aren't available yet.

## API

| Method | Path | Body |
|---|---|---|
| GET | `/api/service-instances/{id}/domains` | — |
| POST | `/api/service-instances/{id}/domains` | — (generate) |
| POST | `/api/service-instances/{id}/custom-domains` | `{ "hostname": "api.example.com", "targetPort": null }` |
| POST | `/api/domains/{id}/check` | — (custom domains; at most once per 5 seconds) |
| PATCH | `/api/domains/{id}` | `{ "targetPort": null, "rateLimitPerMinute": null }` (both replace the stored values) |
| DELETE | `/api/domains/{id}` | — |
| GET | `/api/domains/{id}/ingress` | — (Cloudflare: `managed`, `zone`, `proxied`, `sslMode`, `sslWarning`, `note`) |
| PUT | `/api/domains/{id}/ingress` | `{ "proxied": true }` (custom domains in a Cloudflare-managed zone) |

Each domain has `hostname`, `kind` (`generated`/`custom`), `status`
(`pending`/`verified`/`failed`), `dnsRecords` (`{ type, name, value }`),
`lastError`, `dnsTargetOk`, `targetPort`, `rateLimitPerMinute`,
`rateLimitEnforced`, `checkedAt`, `verifiedAt`. Creating, checking, updating and
deleting need the **Member** role.
