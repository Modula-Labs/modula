import { act, fireEvent, render, screen } from "@testing-library/react";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";
import { ErrorBoundary, Toaster } from "./ErrorBoundary";

function Bomb() {
  const [armed, setArmed] = useState(false);
  if (armed) throw new Error("kaboom");
  return <button onClick={() => setArmed(true)}>arm</button>;
}

describe("ErrorBoundary", () => {
  it("turns a crash into a toast and re-renders the part", () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    render(
      <>
        <ErrorBoundary>
          <Bomb />
        </ErrorBoundary>
        <Toaster />
      </>,
    );
    act(() => fireEvent.click(screen.getByText("arm")));
    expect(screen.getByRole("alert").textContent).toContain("kaboom");
    // Remounted with fresh state, not a blank page.
    expect(screen.getByText("arm")).toBeTruthy();
  });
});
