# Changelog

All notable changes to Modula Deploy are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) (pre-1.0: minor versions
may break things).

## [Unreleased]

### Security

- Security hardening ahead of the open-source release (details to be filled in):
  - _TODO_
- CI builds now install `protoc`, pin every GitHub Action to a commit SHA, and run
  `cargo-deny` and `cargo-audit` on every push and pull request.

### Added

- Open-source release under AGPL-3.0-only: `LICENSE`, `THIRD_PARTY_NOTICES.md`,
  `SECURITY.md`, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, issue and PR templates.
- Dashboard: Settings → Domain, a domain step after setup and a reminder banner;
  dashboard and apps domains can be set after install (API and CLI).
- No domain needed at install: the dashboard is served on `https://<public IP>`
  (sslip.io addresses removed).
- Project shell with a left rail, a Project Settings page, and an environment Logs
  page; stored logs and webhooks.
- Move a service to another server; settings sliders, rail and server picker.
- Change a service's source: connect a GitHub repo, switch repos, or disconnect.
- Metrics: CPU, memory and network per service, with charts and `modula metrics`.
- CLI: `modula deployments` lists deployments; rollback prompts without an empty commit.
- Project cards show their services and how many are online; canvas cards snap to
  the dot grid.

### Changed

- Caddy 2.11.1 (fixes CVE-2026-27586; better IP certificates).
- Custom domains get A records instead of CNAMEs to sslip.io.

## [0.1.3] - 2026-10-24

### Added

- Local-files services: `modula up` creates the service.
- `modula server create`: `--domain auto` for an instant `<ip>.sslip.io` address;
  key-only SSH when a key is installed.
- `modula.toml`: `partial = true` leaves the project's other services alone, and
  unlisted variables are left untouched.
- Railpack builds see the service's variables.
- Failed starts and health checks include the container's last output.
- `modula status` shows local sources.

### Changed

- Logs mask only credential-looking variable values.
- Edge: plain HTTP redirects to HTTPS under socket activation.

### Fixed

- Builds on Ubuntu 23.10+: AppArmor profile for rootless BuildKit.
- Hetzner: dropped the removed `GET /datacenters`; availability now comes from
  `server_types` locations.

## [0.1.1]

Releases up to 0.1.1 predate this changelog (highlights: one-click GitHub connect,
canvas polish, a private dashboard via Cloudflare Tunnel, zero-downtime upgrades and
`modula server create`).

[Unreleased]: https://github.com/Modula-Labs/modula/compare/v0.1.3...HEAD
[0.1.3]: https://github.com/Modula-Labs/modula/compare/v0.1.1...v0.1.3
[0.1.1]: https://github.com/Modula-Labs/modula/releases/tag/v0.1.1
