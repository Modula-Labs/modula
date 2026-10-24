# Bundles a JavaScript/TypeScript function (BUILDER_FUNCTION): installs the
# repository's dependencies with its lockfile's package manager (no install
# scripts), then esbuild bundles the entry into one ES module for workerd and
# Cloudflare Workers. The image holds only the bundle, at /fn/worker.js
# (FUNCTION_OUTPUT_DIR / FUNCTION_MODULE); nothing ever runs it.
#
# Pinned: node:22-alpine by digest (multi-arch index), esbuild by version.
FROM node:22-alpine@sha256:fe4f6d74e20370acb04ad889077d14d0f64a9c8ea6a42bc60cddf1e844699914 AS bundle
ARG entry=index.ts
ENV NPM_CONFIG_UPDATE_NOTIFIER=false NPM_CONFIG_FUND=false NPM_CONFIG_AUDIT=false
WORKDIR /src
COPY . .
RUN set -eu; \
    if [ -f pnpm-lock.yaml ]; then corepack enable && pnpm install --frozen-lockfile --ignore-scripts; \
    elif [ -f yarn.lock ]; then corepack enable && yarn install --frozen-lockfile --ignore-scripts; \
    elif [ -f package-lock.json ]; then npm ci --ignore-scripts; \
    elif [ -f package.json ]; then npm install --ignore-scripts; fi
RUN npx --yes esbuild@0.28.2 "$entry" --bundle --format=esm --platform=neutral --target=es2022 \
      --main-fields=workerd,worker,browser,module,main --conditions=workerd,worker,browser \
      '--external:cloudflare:*' '--external:node:*' --log-level=warning --outfile=/out/worker.js

FROM scratch
COPY --from=bundle /out/ /fn/
CMD ["/fn/worker.js"]
