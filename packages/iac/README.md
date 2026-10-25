# @modula-labs/iac

Types and helpers for [Modula Deploy](../../guide/config-as-code.md) config as
code in TypeScript (`.modula/modula.ts`).

You don't need this package to run a `modula.ts`: the `modula` CLI has the
same code built in and resolves `@modula-labs/iac` itself. Install it for
editor autocomplete and type checking:

```bash
npm i -D @modula-labs/iac
# or: bun add -d @modula-labs/iac
```

```ts
import { defineModula, github, project, service } from "@modula-labs/iac";

export default defineModula((ctx) =>
  project("shop", {
    resources: [
      service("api", {
        source: github("acme/shop", { branch: ctx.isEnvironment("production") ? "main" : "staging" }),
        healthcheck: "/health",
      }),
    ],
  }),
);
```

`iac.js` is the file the CLI embeds (`crates/cli/src/iac/script.rs`); keep
the two in one place by editing it here.
