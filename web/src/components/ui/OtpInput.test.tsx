import { fireEvent, render, screen } from "@testing-library/react";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";
import { OtpInput } from "./OtpInput";

function Harness({ onComplete }: { onComplete: (v: string) => void }) {
  const [v, setV] = useState("");
  return <OtpInput value={v} onChange={setV} onComplete={onComplete} />;
}

describe("OtpInput", () => {
  it("fills all boxes from a paste and completes", () => {
    const done = vi.fn();
    render(<Harness onComplete={done} />);
    fireEvent.paste(screen.getByLabelText("Digit 1"), { clipboardData: { getData: () => "12 34-56" } });
    expect(done).toHaveBeenCalledWith("123456");
    expect((screen.getByLabelText("Digit 6") as HTMLInputElement).value).toBe("6");
  });

  it("advances on typing, ignores non-digits, and backspaces", () => {
    const done = vi.fn();
    render(<Harness onComplete={done} />);
    const d1 = screen.getByLabelText("Digit 1") as HTMLInputElement;
    fireEvent.change(d1, { target: { value: "a" } });
    expect(d1.value).toBe("");
    fireEvent.change(d1, { target: { value: "7" } });
    expect(d1.value).toBe("7");
    const d2 = screen.getByLabelText("Digit 2") as HTMLInputElement;
    fireEvent.keyDown(d2, { key: "Backspace" });
    expect(d1.value).toBe("");
    expect(done).not.toHaveBeenCalled();
  });
});
