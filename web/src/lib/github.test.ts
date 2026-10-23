import { describe, expect, it } from "vitest";
import { serviceNameFromRepo } from "@/pages/project/NewServiceDialog";
import {
  connectMode,
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
  const list = ["acme/api-gateway", "acme/web", "webco/site", "acme/legacy-web-app", "Acme/Webhooks"].map(repo);

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
    expect(rankRepos(list, "acme/w").map((r) => r.fullName)).toEqual(["acme/web", "Acme/Webhooks"]);
    expect(rankRepos(list, "nothing")).toEqual([]);
  });

  it("filters nothing for an exact match, so a picked repository can be switched", () => {
    expect(rankRepos(list, "ACME/web")).toEqual(list);
  });
});

describe("rankBranches", () => {
  it("prefers branches starting with the query", () => {
    const b = ["feature/release-notes", "main", "release/1.2"].map((name) => ({ name, protected: false }));
    expect(rankBranches(b, "rel").map((x) => x.name)).toEqual(["release/1.2", "feature/release-notes"]);
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
  it("lets workspace admins connect, creating the App only as an instance admin", () => {
    expect(connectMode({ appConfigured: true, isInstanceAdmin: false, role: "admin" })).toBe("connect");
    expect(connectMode({ appConfigured: false, isInstanceAdmin: true, role: "admin" })).toBe("connect");
    expect(connectMode({ appConfigured: false, isInstanceAdmin: false, role: "admin" })).toBe("ask-instance-admin");
    expect(connectMode({ appConfigured: true, isInstanceAdmin: true, role: "member" })).toBe("ask-workspace-admin");
    expect(connectMode({ appConfigured: true, isInstanceAdmin: false, role: undefined })).toBe("ask-workspace-admin");
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
    expect(projectReturn({ add: "repo", github: "requested" }).notice?.tone).toBe("info");
    expect(projectReturn({ github: "installed" }).reopen).toBe(false);
    expect(projectReturn({ add: "repo", github: "bogus" }).notice).toBeNull();
    expect(projectReturn({})).toEqual({ reopen: false, notice: null });
  });
});
