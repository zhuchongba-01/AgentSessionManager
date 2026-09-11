import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { SessionMessageItem } from "@/components/sessions/SessionMessageItem";

describe("SessionMessageItem", () => {
  it("marks user messages for right-side bubble layout and copies full content", () => {
    const onCopy = vi.fn();
    const { container } = render(
      <SessionMessageItem
        message={{ role: "user", content: "真实用户消息", ts: 1_788_755_233_000 }}
        isActive
        onCopy={onCopy}
      />,
    );

    expect(container.firstElementChild).toHaveClass(
      "asm-message",
      "is-user",
      "is-active",
    );
    fireEvent.click(screen.getByRole("button", { name: "复制消息" }));
    expect(onCopy).toHaveBeenCalledTimes(1);
    expect(onCopy).toHaveBeenCalledWith("真实用户消息");
  });

  it("keeps assistant messages in the wide document lane", () => {
    const { container } = render(
      <SessionMessageItem
        message={{ role: "assistant", content: "较长的 AI 正文" }}
        isActive={false}
        onCopy={vi.fn()}
      />,
    );

    expect(container.firstElementChild).toHaveClass(
      "asm-message",
      "is-assistant",
    );
    expect(container.firstElementChild).not.toHaveClass("is-user");
  });
});
