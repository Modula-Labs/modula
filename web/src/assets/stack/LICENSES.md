# Stack icon sources

Logos are trademarks of their respective owners. They are included only to identify the technology a
service uses; use does not imply endorsement. The icon sets' licences cover the artwork files, not the
brands.

## Simple Icons 16.35.0 (CC0 1.0) - https://github.com/simple-icons/simple-icons
URL pattern: https://cdn.jsdelivr.net/npm/simple-icons@16.35.0/icons/<slug>.svg

Single colour (`currentColor`): nextjs (nextdotjs), remix, express, fastify, django, github, rust, flask, deno.
Brand-colour fill added by us: hono (#E36002), clickhouse (#FFCC01), minio (#C72E49), meilisearch (#FF5CAA),
caddy (#1F88C0), rails (rubyonrails, #CC0000).

## Devicon 2.17.0 (MIT) - https://github.com/devicons/devicon
URL pattern: https://cdn.jsdelivr.net/gh/devicons/devicon@2.17.0/icons/<name>/<name>-original.svg

nuxt (nuxtjs), svelte, astro, nestjs, vue (vuejs), react, vite (vitejs), fastapi, laravel, go, bun, nodejs,
python, ruby, php, java, kotlin, elixir, swift, html (html5), postgres (postgresql), mysql, mariadb, mongodb,
redis, elasticsearch, rabbitmq, nginx, docker.

## Fallbacks
- valkey: no icon in either set; mapped to the Redis icon in `ServiceIcon.tsx`.

Files were whitespace-minified; `<title>` and XML prolog removed. No scripts, styles or external references.

## Recoloured for dark surfaces
The tiles are transparent, so near-black brand fills were lightened to stay legible in both themes:
docker (#3A4D54 -> #0B7FAE), mariadb (#003545 -> #1E7F9C), elasticsearch (#343741 -> #8A93A3),
vue (#35495e -> #5F7F9F), nuxt (#2f495e -> #5F7F9F), mysql (#00618A -> #3A93C4),
elixir (#330A4C -> #7E4FA3).
