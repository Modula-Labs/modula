import { describe, expect, it } from "vitest";
import type { Environment, InstanceSummary, ProjectDetail } from "@/lib/api";
import { branchOverrides, pickEnvironment, repoServices, serviceDeletion, servicesIn } from "./environments";

const env = (id: string, isDefault = false): Environment => ({
  id,
  name: id,
  createdAt: "",
  isDefault,
  ephemeral: false,
  prNumber: null,
  prRepo: null,
});

describe("pickEnvironment", () => {
  const envs = [env("prod", true), env("staging")];
  it("prefers the URL, then the remembered one, then the default", () => {
    expect(pickEnvironment(envs, "staging", "prod")?.id).toBe("staging");
    expect(pickEnvironment(envs, undefined, "staging")?.id).toBe("staging");
    expect(pickEnvironment(envs, undefined, null)?.id).toBe("prod");
  });
  it("skips deleted environments", () => {
    expect(pickEnvironment(envs, "gone", "gone-too")?.id).toBe("prod");
  });
});

describe("branches of a new environment", () => {
  const instance = (serviceId: string, environmentId: string, repo: string | null, branch: string | null): InstanceSummary => ({
    id: `${serviceId}-${environmentId}`,
    serviceId,
    environmentId,
    nodeId: null,
    latestDeploymentId: null,
    latestDeploymentStatus: null,
    domain: null,
    sourceType: repo ? "repo" : "image",
    repo,
    branch,
    cronSchedule: null,
    cronNextAt: null,
  });
  const project = {
    environments: [env("prod", true), env("staging")],
    services: [
      { id: "api", name: "api", icon: null },
      { id: "db", name: "postgres", icon: null },
    ],
    instances: [instance("api", "prod", "acme/app", "main"), instance("api", "staging", "acme/app", "staging"), instance("db", "prod", null, null)],
  } as unknown as ProjectDetail;

  it("lists the source environment's repo services", () => {
    expect(repoServices(project, "staging")).toEqual([{ serviceId: "api", name: "api", repo: "acme/app", branch: "staging" }]);
  });
  it("sends only changed branches", () => {
    const services = repoServices(project, "prod");
    expect(branchOverrides(services, {})).toEqual({});
    expect(branchOverrides(services, { api: " main " })).toEqual({});
    expect(branchOverrides(services, { api: "" })).toEqual({});
    expect(branchOverrides(services, { api: "dev" })).toEqual({ api: "dev" });
  });
});

describe("deleting a service from one environment", () => {
  const inst = (id: string, serviceId: string, environmentId: string, volume = false) =>
    ({
      id,
      serviceId,
      environmentId,
      volume: volume ? { id: `v-${id}`, mountPath: "/data", createdAt: "" } : null,
    }) as unknown as InstanceSummary;
  const project = {
    environments: [env("prod", true), env("staging")],
    services: [
      { id: "pg", name: "postgres", icon: null, template: "postgres" },
      { id: "web", name: "web", icon: null },
    ],
    // `web` was deleted from staging.
    instances: [inst("pg-p", "pg", "prod", true), inst("pg-s", "pg", "staging", true), inst("web-p", "web", "prod")],
  } as unknown as ProjectDetail;

  it("shows only the services that exist in the environment", () => {
    expect(servicesIn(project, "prod").map((s) => s.id)).toEqual(["pg", "web"]);
    expect(servicesIn(project, "staging").map((s) => s.id)).toEqual(["pg"]);
  });

  it("names the environment's instance, its volume and every environment", () => {
    expect(serviceDeletion(project, "pg", "staging")).toEqual({
      instanceId: "pg-s",
      environmentName: "staging",
      volume: true,
      r2: false,
      environments: ["prod", "staging"],
    });
    expect(serviceDeletion(project, "web", "staging").instanceId).toBeUndefined();
    expect(serviceDeletion(project, "web", "prod").environments).toEqual(["prod"]);
  });
});
