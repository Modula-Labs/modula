import { fireEvent, render, screen } from "@testing-library/react";
import { useState } from "react";
import { describe, expect, it } from "vitest";
import type { ReferenceOption } from "@/lib/references";
import { ReferenceInput } from "./ReferenceInput";

const options: ReferenceOption[] = [
  { group: "Shared", label: "shared.API_KEY", path: "shared.API_KEY" },
  { group: "postgres", label: "postgres.DATABASE_URL", path: "postgres.DATABASE_URL" },
];

function Harness() {
  const [v, setV] = useState("");
  return <ReferenceInput aria-label="Value" value={v} onValueChange={setV} options={options} />;
}

Element.prototype.scrollIntoView ??= () => {};

describe("ReferenceInput", () => {
  it.each(["Tab", "Enter"])("completes the highlighted reference on %s", (key) => {
    render(<Harness />);
    const input = screen.getByLabelText("Value") as HTMLInputElement;
    input.focus();
    fireEvent.change(input, { target: { value: "${{post", selectionStart: 7 } });
    fireEvent.keyDown(input, { key });
    expect(input.value).toBe("${{postgres.DATABASE_URL}}");
  });
});
