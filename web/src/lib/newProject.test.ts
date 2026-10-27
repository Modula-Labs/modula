import { describe, expect, it } from "vitest";
import { addStepFrom, defaultProjectName, filterNewProjectOptions, githubRepoFromLink, NEW_PROJECT_OPTIONS } from "./newProject";

describe("githubRepoFromLink", () => {
  it("reads GitHub links", () => {
    expect(githubRepoFromLink("https://github.com/acme/web")).toBe("acme/web");
    expect(githubRepoFromLink(" github.com/acme/web.git ")).toBe("acme/web");
    expect(githubRepoFromLink("https://github.com/acme/web/tree/main/src?x=1")).toBe("acme/web");
    expect(githubRepoFromLink("git@github.com:acme/web.git")).toBe("acme/web");
  });
  it("leaves everything else as a search", () => {
    expect(githubRepoFromLink("acme/web")).toBeNull();
    expect(githubRepoFromLink("postgres")).toBeNull();
    expect(githubRepoFromLink("https://gitlab.com/acme/web")).toBeNull();
    expect(githubRepoFromLink("https://github.com/acme")).toBeNull();
  });
});

describe("filterNewProjectOptions", () => {
  it("returns everything for an empty query", () => {
    expect(filterNewProjectOptions("  ")).toEqual(NEW_PROJECT_OPTIONS);
  });
  it("matches labels and keywords", () => {
    expect(filterNewProjectOptions("data").map((o) => o.kind)).toEqual(["database"]);
    expect(filterNewProjectOptions("redis").map((o) => o.kind)).toEqual(["database"]);
    expect(filterNewProjectOptions("s3").map((o) => o.kind)).toEqual(["bucket"]);
    expect(filterNewProjectOptions("docker image").map((o) => o.kind)).toEqual(["image"]);
    expect(filterNewProjectOptions("zzz")).toEqual([]);
  });
});

describe("defaultProjectName", () => {
  it("uses the repo name, numbered when taken", () => {
    expect(defaultProjectName([], "acme/My_App")).toBe("my_app");
    expect(defaultProjectName(["web"], "acme/web")).toBe("web-2");
    expect(defaultProjectName(["my-project"])).toBe("my-project-2");
  });
});

describe("addStepFrom", () => {
  it("accepts only known steps", () => {
    expect(addStepFrom("database")).toBe("database");
    expect(addStepFrom("nope")).toBeUndefined();
  });
});
