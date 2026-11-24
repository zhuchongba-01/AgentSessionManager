import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { SessionRows } from "@/components/sessions/SessionRows";
import { getSessionKey } from "@/components/sessions/utils";

describe("SessionRows", () => {
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

  it("preserves current page across same-filter rescans and syncs page when selectedKey moves across pages", () => {
    const makeSessions = () =>
      Array.from({ length: 150 }, (_, index) => ({
        providerId: "codex",
        sessionId: `s-${index}`,
        sourcePath: `/mock/${index}.jsonl`,
        title: `Session ${index}`,
      }));

    const initial = makeSessions();
    const { rerender } = render(
      <SessionRows
        sessions={initial}
        selectedKey={getSessionKey(initial[0])}
        resetKey="codex:all::flat"
        renderItem={(session) => (
          <div key={session.sessionId}>{session.title}</div>
        )}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "下一页" }));
    expect(screen.getByText("Session 100")).toBeInTheDocument();

    // Rescan produces a fresh array reference with identical resetKey: stays on page 2
    const rescanned = makeSessions();
    rerender(
      <SessionRows
        sessions={rescanned}
        selectedKey={getSessionKey(rescanned[0])}
        resetKey="codex:all::flat"
        renderItem={(session) => (
          <div key={session.sessionId}>{session.title}</div>
        )}
      />,
    );
    expect(screen.getByText("Session 100")).toBeInTheDocument();

    // Keyboard navigation changes selectedKey to item 10 on page 1: syncs to page 1
    rerender(
      <SessionRows
        sessions={rescanned}
        selectedKey={getSessionKey(rescanned[10])}
        resetKey="codex:all::flat"
        renderItem={(session) => (
          <div key={session.sessionId}>{session.title}</div>
        )}
      />,
    );
    expect(screen.getByText("Session 10")).toBeInTheDocument();
    expect(screen.queryByText("Session 100")).not.toBeInTheDocument();
  });
});
