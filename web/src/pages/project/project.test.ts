import { describe, expect, it } from "vitest";
import { serviceNameFromImage } from "./NewServiceDialog";
import { joinArgs, splitArgs } from "./SettingsTab";

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
