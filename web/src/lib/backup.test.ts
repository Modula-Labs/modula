import { describe, expect, it } from "vitest";
import type { BackupDestinations, BackupTarget } from "./api";
import { defaultDestination, destinationChoice, destinationGroups, destinationValue } from "./backup";

const target = (id: string, kind: BackupTarget["kind"]): BackupTarget => ({
  id,
  name: `t-${id}`,
  kind,
  provider: kind === "local" ? null : "s3",
  endpoint: "",
  region: "",
  bucket: "",
  prefix: "",
  accessKeyId: "",
  bucketInstanceId: null,
  serviceName: null,
  environmentName: null,
  projectName: null,
  nodeId: null,
  nodeName: null,
  createdAt: "2026-10-22T00:00:00Z",
});

const dest: BackupDestinations = {
  targets: [target("a", "s3"), target("b", "bucket"), target("c", "local")],
  buckets: [{ instanceId: "i1", template: "r2", serviceName: "files", environmentName: "prod", projectName: "app", targetId: "b" }],
  servers: [{ nodeId: "n1", name: "node-1", targetId: "c" }],
};

describe("backup destinations", () => {
  it("groups s3 targets, buckets and the disk without duplicates", () => {
    const groups = destinationGroups(dest);
    expect(groups.map((g) => g.label)).toEqual(["Storage targets", "Bucket services", "This server's disk"]);
    expect(groups[0]!.options.map((o) => o.value)).toEqual(["target:a"]);
    expect(groups[1]!.options[0]).toMatchObject({ value: "bucket:i1", label: "app / files (prod)", hint: "Cloudflare R2" });
    expect(groups[2]!.options[0]).toMatchObject({ value: "local", label: "Disk of node-1" });
  });
  it("omits empty groups", () => {
    expect(destinationGroups({ targets: [], buckets: [], servers: [] })).toEqual([]);
  });
  it("derives the selection from a saved target id", () => {
    expect(destinationValue("a", dest)).toBe("target:a");
    expect(destinationValue("b", dest)).toBe("bucket:i1");
    expect(destinationValue("c", dest)).toBe("local");
  });
  it("maps values to request fields", () => {
    expect(destinationChoice("target:a")).toEqual({ targetId: "a" });
    expect(destinationChoice("bucket:i1")).toEqual({ bucketInstanceId: "i1" });
    expect(destinationChoice("local")).toEqual({ localDisk: true });
    expect(destinationChoice("")).toBeNull();
  });
  it("defaults to the first option", () => {
    expect(defaultDestination(destinationGroups(dest))).toBe("target:a");
    expect(defaultDestination([])).toBe("");
  });
});
