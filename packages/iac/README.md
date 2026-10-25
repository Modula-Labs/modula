# modula-deploy

Types and helpers for [Modula Deploy](../../guide/config-as-code.md) config as
code in TypeScript (`.modula/modula.ts`).

You don't need this package to run a `modula.ts`: the `modula` CLI has the
same code built in and resolves `modula/iac` itself. Install it for editor
autocomplete and type checking, under the name `modula` so the import in your
file matches:

```bash
npm i -D modula@npm:modula-deploy
# or: bun add -d modula@npm:modula-deploy
```

```ts
import { defineModula, github, project, service } from "modula/iac";

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
