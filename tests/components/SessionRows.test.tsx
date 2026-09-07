import { fireEvent, render, screen } from "@testing-library/react";
import { expect, it } from "vitest";
import { SessionRows } from "@/components/sessions/SessionRows";

it("bounds rendered sessions and allows paging through the entire result", () => {
  const sessions = Array.from({ length: 201 }, (_, index) => ({
    providerId: "codex",
    sessionId: String(index),
    title: `Session ${index}`,
  }));
  render(
    <SessionRows
      sessions={sessions}
      renderItem={(session) => (
        <div key={session.sessionId} data-testid="session-row">
          {session.title}
        </div>
      )}
    />,
  );
  expect(screen.getAllByTestId("session-row")).toHaveLength(100);
  expect(screen.queryByText("Session 100")).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "下一页" }));
  expect(screen.getByText("Session 100")).toBeInTheDocument();
  expect(screen.queryByText("Session 0")).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "下一页" }));
  expect(screen.getAllByTestId("session-row")).toHaveLength(1);
  expect(screen.getByRole("button", { name: "下一页" })).toBeDisabled();
});
