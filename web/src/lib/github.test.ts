import { describe, expect, it } from "vitest";
import type { Role } from "@/lib/api";
import { serviceNameFromRepo } from "@/pages/project/NewServiceDialog";
import {
  appOwner,
  installLabel,
  publicApp,
  connectMode,
  connectStep,
  githubAvatar,
  defaultAccount,
  effectiveAccount,
  filterReposByAccount,
  repoOwner,
  type GithubInstallation,
  githubReturnNotice,
  moveHighlight,
  projectReturn,
  projectReturnPath,
  rankBranches,
  rankRepos,
  type GithubRepo,
} from "./github";

const repo = (fullName: string): GithubRepo => ({
  fullName,
  private: false,
  defaultBranch: "main",
  updatedAt: null,
  ownerAvatar: null,
});

describe("rankRepos", () => {
  // Server order is most recently pushed first.
  const list = [
    "acme/api-gateway",
    "acme/web",
    "webco/site",
    "acme/legacy-web-app",
    "Acme/Webhooks",
  ].map(repo);

  it("keeps the server order when nothing is typed", () => {
    expect(rankRepos(list, "  ")).toEqual(list);
  });

  it("puts name prefixes first, then owner prefixes, then substrings, case-insensitively", () => {
    expect(rankRepos(list, "WEB").map((r) => r.fullName)).toEqual([
      "acme/web",
      "Acme/Webhooks",
      "webco/site",
      "acme/legacy-web-app",
    ]);
  });

  it("matches owner/name and drops what doesn't match", () => {
    expect(rankRepos(list, "acme/w").map((r) => r.fullName)).toEqual([
      "acme/web",
      "Acme/Webhooks",
    ]);
    expect(rankRepos(list, "nothing")).toEqual([]);
  });

  it("filters nothing for an exact match, so a picked repository can be switched", () => {
    expect(rankRepos(list, "ACME/web")).toEqual(list);
  });
});

describe("rankBranches", () => {
  it("prefers branches starting with the query", () => {
    const b = ["feature/release-notes", "main", "release/1.2"].map((name) => ({
      name,
      protected: false,
    }));
    expect(rankBranches(b, "rel").map((x) => x.name)).toEqual([
      "release/1.2",
      "feature/release-notes",
    ]);
  });
});

describe("moveHighlight", () => {
  it("wraps around and starts at either end", () => {
    expect(moveHighlight(-1, 3, 1)).toBe(0);
    expect(moveHighlight(-1, 3, -1)).toBe(2);
    expect(moveHighlight(2, 3, 1)).toBe(0);
    expect(moveHighlight(0, 3, -1)).toBe(2);
    expect(moveHighlight(0, 0, 1)).toBe(-1);
  });
});

describe("serviceNameFromRepo", () => {
  it("slugifies the repository name", () => {
    expect(serviceNameFromRepo("acme/My_App")).toBe("my-app");
    expect(serviceNameFromRepo("acme/web.site")).toBe("web-site");
    expect(serviceNameFromRepo("acme/2fa-service")).toBe("fa-service");
  });
});

describe("githubReturnNotice", () => {
  it("explains each return from GitHub and ignores the rest", () => {
    expect(githubReturnNotice("installed")?.tone).toBe("success");
    expect(githubReturnNotice("requested")?.text).toMatch(/approve/);
    expect(githubReturnNotice(null)).toBeNull();
    expect(githubReturnNotice("bogus")).toBeNull();
  });
});

describe("connectMode", () => {
  const m = (
    appCount: number,
    isInstanceAdmin: boolean,
    role: Role | undefined,
  ) => connectMode({ appCount, isInstanceAdmin, role });
  it("lets workspace admins connect, adding the first App only as an instance admin", () => {
    expect(m(1, false, "admin")).toBe("connect");
    expect(m(3, false, "admin")).toBe("connect");
    expect(m(0, true, "admin")).toBe("connect");
    expect(m(0, false, "admin")).toBe("ask-instance-admin");
    expect(m(1, true, "member")).toBe("ask-workspace-admin");
    expect(m(1, false, undefined)).toBe("ask-workspace-admin");
  });
});

describe("connectStep", () => {
  it("adds the first App, installs the only one, asks which with several", () => {
    expect(connectStep(0)).toBe("add-app");
    expect(connectStep(1)).toBe("install");
    expect(connectStep(2)).toBe("pick");
  });
});

describe("appOwner / githubAvatar", () => {
  it("falls back to the slug and encodes the login", () => {
    expect(appOwner({ ownerLogin: "acme", slug: "modula-acme" })).toBe("acme");
    expect(appOwner({ ownerLogin: null, slug: "modula-x" })).toBe("modula-x");
    expect(githubAvatar("a b")).toBe(
      "https://avatars.githubusercontent.com/a%20b?s=64",
    );
  });
});

describe("return paths", () => {
  it("comes back to the project with the dialog on the GitHub step", () => {
    expect(projectReturnPath("p1", "e1")).toBe("/projects/p1?env=e1&add=repo");
    expect(projectReturnPath("p/../x")).toBe("/projects/p%2F..%2Fx?add=repo");
    // A same-origin path, as the server requires.
    expect(projectReturnPath("p1", "e1").startsWith("/")).toBe(true);
    expect(projectReturnPath("p1", "e1").startsWith("//")).toBe(false);
  });

  it("reopens the dialog and says how it went", () => {
    expect(projectReturn({ add: "repo", github: "installed" })).toEqual({
      reopen: true,
      notice: { tone: "success", text: "GitHub connected — pick a repo." },
    });
    expect(
      projectReturn({ add: "repo", github: "requested" }).notice?.tone,
    ).toBe("info");
    expect(projectReturn({ github: "installed" }).reopen).toBe(false);
    expect(projectReturn({ add: "repo", github: "bogus" }).notice).toBeNull();
    expect(projectReturn({})).toEqual({ reopen: false, notice: null });
  });
});

describe("accounts", () => {
  const inst = (accountLogin: string): GithubInstallation => ({
    id: 1,
    accountLogin,
    accountType: "Organization",
    avatarUrl: "",
    configureUrl: "",
    createdAt: "",
    appId: 1,
    appSlug: "app",
  });
  const r = (fullName: string, updatedAt: string | null): GithubRepo => ({
    ...repo(fullName),
    updatedAt,
  });
  const installs = [inst("me"), inst("Acme")];
  const repos = [
    r("other/x", "2026-03-01T00:00:00Z"),
    r("acme/api", "2026-02-01T00:00:00Z"),
    r("me/blog", "2026-01-01T00:00:00Z"),
  ];

  it("filters repositories to one account, case-insensitively", () => {
    expect(repoOwner("acme/api")).toBe("acme");
    expect(filterReposByAccount(repos, "ACME").map((x) => x.fullName)).toEqual([
      "acme/api",
    ]);
    expect(filterReposByAccount(repos, null)).toEqual(repos);
  });

  it("defaults to the account with the most recent push, else the first", () => {
    expect(defaultAccount(installs, repos)).toBe("Acme");
    expect(defaultAccount(installs, [])).toBe("me");
    expect(defaultAccount([], repos)).toBeNull();
  });

  it("keeps a still-installed selection and falls back otherwise", () => {
    expect(effectiveAccount("me", installs, repos)).toBe("me");
    expect(effectiveAccount("gone", installs, repos)).toBe("Acme");
  });
});

describe("publicApp / installLabel", () => {
  const app = (pub: boolean, ownerLogin: string | null = "acme") => ({
    public: pub,
    ownerLogin,
    slug: "modula-x",
  });
  it("finds the App that installs on any account", () => {
    expect(publicApp([app(false), app(true, "me")])?.ownerLogin).toBe("me");
    expect(publicApp([app(false)])).toBeUndefined();
  });
  it("installs a public App on any account, a private one on its owner", () => {
    expect(installLabel(app(true))).toBe("Install on another account");
    expect(installLabel(app(false))).toBe("Install on acme");
  });
});
