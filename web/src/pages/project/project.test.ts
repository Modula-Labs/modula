import { describe, expect, it } from "vitest";
import { parseRepo, serviceNameFromImage } from "./NewServiceDialog";
import { joinArgs, splitArgs, switchSource, toConfig, toForm } from "./SettingsTab";
import type { ServiceConfig } from "@/lib/api";

describe("serviceNameFromImage", () => {
  it.each([
    ["nginx:1.27", "nginx"],
    ["ghcr.io/acme/My_API:2", "my-api"],
    ["registry:5000/team/web@sha256:abc", "web"],
    ["9lives", "lives"],
    ["", "service"],
  ])("%s → %s", (image, name) => expect(serviceNameFromImage(image)).toBe(name));
});

describe("start command argv", () => {
  it("splits on spaces and honours quotes", () => {
    expect(splitArgs(`node  server.js --name "a b" 'c d' "x\\"y"`)).toEqual(["node", "server.js", "--name", "a b", "c d", 'x"y']);
  });
  it("round-trips through joinArgs", () => {
    const argv = ["sh", "-c", "echo hi", ""];
    expect(splitArgs(joinArgs(argv))).toEqual(argv);
  });
});

describe("parseRepo", () => {
  it.each([
    ["acme/app", "acme/app"],
    [" https://github.com/acme/app ", "acme/app"],
    ["https://github.com/acme/my.app.git", "acme/my.app"],
    ["github.com/acme/app/tree/main/web", "acme/app"],
    ["git@github.com:acme/app.git", "acme/app"],
    ["acme", null],
    ["https://gitlab.com/acme/app", null],
    ["", null],
  ])("%s → %s", (input, repo) => expect(parseRepo(input)).toBe(repo));
});

describe("service config payload", () => {
  const base = { port: 8080, healthcheckTimeoutSecs: 300, restartPolicy: "on_failure", replicas: 1 } as const;
  const keys = (c: ServiceConfig) => Object.keys(JSON.parse(JSON.stringify(c))).sort();

  it("keeps image configs free of build fields", () => {
    const c: ServiceConfig = { ...base, source: { type: "image", image: "nginx:1" } };
    expect(keys(toConfig(toForm(c)))).toEqual(["healthcheckTimeoutSecs", "port", "replicas", "restartPolicy", "source"]);
  });

  it("round-trips repo configs and drops empty optionals", () => {
    const c: ServiceConfig = {
      ...base,
      source: { type: "repo", repo: "acme/app", branch: "main", rootDir: "web" },
      builder: "dockerfile",
      dockerfilePath: "Dockerfile.prod",
      autoDeploy: false,
      watchPaths: ["web/**", "!web/docs/**"],
    };
    expect(JSON.parse(JSON.stringify(toConfig(toForm(c))))).toEqual(c);
    const bare = toConfig({ ...toForm(c), rootDir: "", dockerfilePath: "", watchPaths: " \n" });
    expect(JSON.parse(JSON.stringify(bare)).source).toEqual({ type: "repo", repo: "acme/app", branch: "main" });
    expect(bare.watchPaths).toBeUndefined();
  });

  it("round-trips deploy settings and drops retries without on_failure", () => {
    const c: ServiceConfig = {
      ...base,
      source: { type: "image", image: "api:1" },
      preDeployCommand: ["bunx", "prisma", "migrate", "deploy"],
      restartPolicyMaxRetries: 5,
      drainingSeconds: 30,
    };
    expect(JSON.parse(JSON.stringify(toConfig(toForm(c))))).toEqual(c);
    const always = toConfig({ ...toForm(c), restartPolicy: "always", drainingSeconds: "0", preDeployCommand: " " });
    expect(always.restartPolicyMaxRetries).toBeUndefined();
    expect(always.drainingSeconds).toBeUndefined();
    expect(always.preDeployCommand).toBeUndefined();
  });

  it("omits dockerfilePath for the railpack builder", () => {
    const f = { ...toForm({ ...base, source: { type: "repo", repo: "a/b", branch: "main" } }), dockerfilePath: "X" };
    expect(toConfig(f).dockerfilePath).toBeUndefined();
  });

  describe("switching the source kind", () => {
    const sw = (c: ServiceConfig, kind: "image" | "repo" | "local", patch = {}) =>
      JSON.parse(JSON.stringify(toConfig({ ...switchSource(toForm(c), kind), ...patch })));

    it("local → repo adds repo/branch and the repo-only fields, keeps the rest", () => {
      const c: ServiceConfig = { ...base, port: 3000, source: { type: "local", rootDir: "api" }, builder: "dockerfile", dockerfilePath: "D" };
      const out = sw(c, "repo", { repo: "https://github.com/acme/app.git", branch: "dev" });
      expect(out.source).toEqual({ type: "repo", repo: "acme/app", branch: "dev", rootDir: "api" });
      expect(out).toMatchObject({ port: 3000, builder: "dockerfile", dockerfilePath: "D", autoDeploy: true });
    });

    it("repo → local drops repo, branch, autoDeploy and watchPaths", () => {
      const c: ServiceConfig = {
        ...base,
        source: { type: "repo", repo: "acme/app", branch: "main", rootDir: "web" },
        autoDeploy: false,
        watchPaths: ["web/**"],
        buildCommand: "npm run build",
      };
      const out = sw(c, "local");
      expect(out.source).toEqual({ type: "local", rootDir: "web" });
      expect(out.autoDeploy).toBeUndefined();
      expect(out.watchPaths).toBeUndefined();
      expect(out.buildCommand).toBe("npm run build");
    });

    it("image → repo gets a build config; switching back restores the image", () => {
      const c: ServiceConfig = { ...base, source: { type: "image", image: "nginx:1" } };
      const out = sw(c, "repo", { repo: "acme/app" });
      expect(out.source).toEqual({ type: "repo", repo: "acme/app", branch: "main" });
      expect(out.builder).toBe("railpack");
      expect(sw(c, "repo")).not.toEqual(c);
      expect(toConfig(switchSource(switchSource(toForm(c), "repo"), "image"))).toEqual(toConfig(toForm(c)));
    });
  });
});
