import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { EdgeRule } from "@/lib/api";
import { EdgeRulesDialog } from "./EdgeRulesDialog";

const saved: EdgeRule[] = [
  { name: "Admin", enabled: true, when: [[{ field: "path", op: "prefix", value: "/admin" }]], then: { type: "block", status: 403 } },
  { name: "Static", enabled: true, when: [], then: { type: "cache", ttlSecs: 60 } },
];

describe("EdgeRulesDialog", () => {
  it("saves the cleaned rules after an edit", async () => {
    const onSave = vi.fn().mockResolvedValue(undefined);
    render(<EdgeRulesDialog open onOpenChange={() => {}} rules={saved} onSave={onSave} />);
    const save = screen.getByRole("button", { name: "Save" });
    expect((save as HTMLButtonElement).disabled).toBe(true);
    fireEvent.change(screen.getByLabelText("Name of rule 1"), { target: { value: " Admin area " } });
    expect((save as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(save);
    await waitFor(() => expect(onSave).toHaveBeenCalled());
    expect(onSave.mock.calls[0]![0][0].name).toBe("Admin area");
  });

  it("blocks saving an invalid rule and shows why", () => {
    render(<EdgeRulesDialog open onOpenChange={() => {}} rules={saved} onSave={vi.fn()} />);
    fireEvent.click(screen.getAllByRole("button", { name: "Expand rule" })[0]!);
    fireEvent.change(screen.getByLabelText("Path"), { target: { value: "admin" } });
    expect(screen.getByText("A path condition's value must start with /")).toBeTruthy();
    expect((screen.getByRole("button", { name: "Save" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("asks before discarding edits and moves a rule with the arrow keys", () => {
    const onOpenChange = vi.fn();
    render(<EdgeRulesDialog open onOpenChange={onOpenChange} rules={saved} onSave={vi.fn()} />);
    fireEvent.keyDown(screen.getAllByRole("button", { name: /^Reorder rule/ })[0]!, { key: "ArrowDown" });
    expect((screen.getByLabelText("Name of rule 1") as HTMLInputElement).value).toBe("Static");
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(screen.getByText("Discard unsaved changes?")).toBeTruthy();
    expect(onOpenChange).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Discard" }));
    expect(onOpenChange).toHaveBeenCalledWith(false);
  });

  it("applies valid JSON when switching back to the editor", () => {
    render(<EdgeRulesDialog open onOpenChange={() => {}} rules={[]} onSave={vi.fn()} />);
    fireEvent.click(screen.getByRole("button", { name: "Edit as JSON" }));
    fireEvent.change(screen.getByLabelText("Edge rules as JSON"), { target: { value: '[{"name":"Open","then":{"type":"allow"}}]' } });
    fireEvent.click(screen.getByRole("button", { name: "Back to editor" }));
    expect((screen.getByLabelText("Name of rule 1") as HTMLInputElement).value).toBe("Open");
  });
});
