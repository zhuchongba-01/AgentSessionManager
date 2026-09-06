import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { SessionMinimap } from "@/components/sessions/SessionMinimap";
import type { SessionMessage } from "@/types";

const buildConversation = (turnCount: number) => {
  const messages: SessionMessage[] = [];
  const messageIndexes: number[] = [];

  for (let turn = 1; turn <= turnCount; turn += 1) {
    messageIndexes.push(messages.length);
    messages.push({ role: "user", content: `用户问题 ${turn}` });
    messages.push({ role: "assistant", content: `助手回答 ${turn}` });
  }

  return { messages, messageIndexes };
};

describe("SessionMinimap", () => {
  it("renders a bounded window around the current turn for long sessions", () => {
    const { messages, messageIndexes } = buildConversation(40);
    const onTickClick = vi.fn();

    render(
      <SessionMinimap
        messages={messages}
        messageIndexes={messageIndexes}
        visibleMessageIndexes={[40, 41, 42, 43]}
        onTickClick={onTickClick}
      />,
    );

    const ticks = screen.getAllByRole("button");
    expect(ticks).toHaveLength(9);
    expect(
      screen.getByRole("button", { name: "跳转到第 21 个用户问题" }),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "跳转到第 1 个用户问题" }),
    ).not.toBeInTheDocument();

    const target = screen.getByRole("button", {
      name: "跳转到第 21 个用户问题",
    });
    const secondVisibleTurn = screen.getByRole("button", {
      name: "跳转到第 22 个用户问题",
    });
    expect(target.firstElementChild).toHaveStyle({ width: "8px" });
    expect(target.firstElementChild).toHaveClass("is-current");
    expect(secondVisibleTurn.firstElementChild).toHaveClass("is-current");
    expect(
      screen.getByRole("button", {
        name: "跳转到第 20 个用户问题",
      }).firstElementChild,
    ).not.toHaveClass("is-current");
    fireEvent.mouseEnter(target);
    expect(target.firstElementChild).toHaveStyle({ width: "34px" });
    expect(screen.getByRole("tooltip")).toHaveTextContent("用户问题 21");
    expect(screen.getByRole("tooltip")).toHaveTextContent("助手回答 21");

    fireEvent.click(target);
    expect(onTickClick).toHaveBeenCalledWith(40);
  });

  it("uses only the caller-provided real user-message indexes", () => {
    const messages: SessionMessage[] = [
      { role: "user", content: "注入上下文" },
      { role: "developer", content: "内部开发者提示" },
      { role: "user", content: "真实问题" },
      { role: "assistant", content: "真实回答" },
    ];

    render(
      <SessionMinimap
        messages={messages}
        messageIndexes={[2]}
        visibleMessageIndexes={[2]}
        onTickClick={vi.fn()}
      />,
    );

    expect(screen.getAllByRole("button")).toHaveLength(1);
    fireEvent.mouseEnter(screen.getByRole("button"));
    expect(screen.getByRole("tooltip")).toHaveTextContent("真实问题");
    expect(screen.getByRole("tooltip")).not.toHaveTextContent("注入上下文");
  });
});
