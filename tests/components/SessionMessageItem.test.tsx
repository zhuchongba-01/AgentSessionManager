import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { SessionMessageItem } from "@/components/sessions/SessionMessageItem";
import type { SessionMessage } from "@/types";

/** 后端只下发预览的消息：正文要靠 onLoadFullContent 取回 */
const truncatedMessage: SessionMessage = {
  role: "tool",
  content: "预览正文",
  index: 4,
  truncated: true,
  contentChars: 12000,
};

describe("SessionMessageItem", () => {
  it("marks user messages for right-side bubble layout and copies full content", () => {
    const onCopy = vi.fn();
    const { container } = render(
      <SessionMessageItem
        message={{
          role: "user",
          content: "真实用户消息",
          ts: 1_788_755_233_000,
        }}
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

  it("fetches the full content of a truncated message only when asked", async () => {
    const onLoadFullContent = vi.fn(async () => "完整正文");
    render(
      <SessionMessageItem
        message={truncatedMessage}
        isActive={false}
        onCopy={vi.fn()}
        onLoadFullContent={onLoadFullContent}
      />,
    );

    // 展开前只渲染后端下发的预览，不请求全文
    expect(screen.getByText(/预览正文/)).toBeInTheDocument();
    expect(onLoadFullContent).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: /展开完整内容/ }));
    expect(await screen.findByText("完整正文")).toBeInTheDocument();
    expect(onLoadFullContent).toHaveBeenCalledTimes(1);

    // 收起后再展开复用已取回的正文，不重复请求
    fireEvent.click(screen.getByRole("button", { name: /收起/ }));
    fireEvent.click(screen.getByRole("button", { name: /展开完整内容/ }));
    expect(await screen.findByText("完整正文")).toBeInTheDocument();
    expect(onLoadFullContent).toHaveBeenCalledTimes(1);
  });

  it("copies the full content rather than the preview of a truncated message", async () => {
    const onLoadFullContent = vi.fn(async () => "完整正文");
    const onCopy = vi.fn();
    render(
      <SessionMessageItem
        message={truncatedMessage}
        isActive={false}
        onCopy={onCopy}
        onLoadFullContent={onLoadFullContent}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "复制消息" }));

    await waitFor(() => expect(onCopy).toHaveBeenCalledWith("完整正文"));
  });

  it("stays collapsed when the full content cannot be read", async () => {
    const onLoadFullContent = vi.fn(async () => {
      throw new Error("会话已更新，请重新打开");
    });
    render(
      <SessionMessageItem
        message={truncatedMessage}
        isActive={false}
        onCopy={vi.fn()}
        onLoadFullContent={onLoadFullContent}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: /展开完整内容/ }));

    await waitFor(() => expect(onLoadFullContent).toHaveBeenCalledTimes(1));
    // 读不到就保持折叠，不做假的展开
    expect(
      screen.getByRole("button", { name: /展开完整内容/ }),
    ).toHaveAttribute("aria-expanded", "false");
    expect(screen.getByText(/预览正文/)).toBeInTheDocument();
  });

  it("still collapses long content locally when nothing was truncated", () => {
    const long = "字".repeat(4000);
    render(
      <SessionMessageItem
        message={{ role: "assistant", content: long }}
        isActive={false}
        onCopy={vi.fn()}
      />,
    );

    // 未截断的长正文就地折叠：正文已经在前端，不受按需取全文影响
    expect(screen.getByText(/展开完整内容/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /展开完整内容/ }));
    expect(screen.getByText(long)).toBeInTheDocument();
  });

  it("folds Codex IDE setup context by default while copying the full text", () => {
    const onCopy = vi.fn();
    const content = [
      "# Context from my IDE setup:",
      "",
      "## Active file: src/App.tsx",
      "",
      "## My request for Codex:",
      "真正想问的问题",
    ].join("\n");

    render(
      <SessionMessageItem
        message={{ role: "user", content }}
        isActive={false}
        isCodexSession
        onCopy={onCopy}
      />,
    );

    expect(screen.getByText("真正想问的问题")).toBeInTheDocument();
    expect(
      screen.queryByText(/Active file: src\/App\.tsx/),
    ).not.toBeInTheDocument();

    const toggle = screen.getByRole("button", { name: "查看 IDE 上下文" });
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    fireEvent.click(toggle);

    expect(
      screen.getByRole("button", { name: "收起 IDE 上下文" }),
    ).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByText(/Active file: src\/App\.tsx/)).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "收起 IDE 上下文" }));
    expect(
      screen.queryByText(/Active file: src\/App\.tsx/),
    ).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "复制消息" }));
    expect(onCopy).toHaveBeenCalledWith(content);
  });

  it("renders Markdown with code block copy and raw text toggle", () => {
    const onCopy = vi.fn();
    const onCopyCode = vi.fn();
    const content = [
      "查看 `utils.ts` 修改：",
      "```ts",
      "export const ready = true;",
      "```",
    ].join("\n");

    const { container } = render(
      <SessionMessageItem
        message={{ role: "assistant", content }}
        isActive={false}
        onCopy={onCopy}
        onCopyCode={onCopyCode}
      />,
    );

    expect(container.querySelector("code.asm-inline-code")).toHaveTextContent(
      "utils.ts",
    );
    fireEvent.click(screen.getByRole("button", { name: "复制代码" }));
    expect(onCopyCode).toHaveBeenCalledWith("export const ready = true;");

    const rawToggle = screen.getByRole("button", { name: "查看原文" });
    expect(rawToggle).toHaveAttribute("aria-pressed", "false");
    fireEvent.click(rawToggle);

    expect(
      screen.getByRole("button", { name: "Markdown 视图" }),
    ).toHaveAttribute("aria-pressed", "true");
    expect(container.querySelector(".asm-code-block")).toBeNull();
    expect(screen.getByText(/```ts/)).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "复制消息" }));
    expect(onCopy).toHaveBeenCalledWith(content);
  });

  it("folds multi-line non-truncated tool output and toggles expansion", () => {
    const lines = Array.from(
      { length: 12 },
      (_, idx) => `tool line ${idx + 1}`,
    );
    render(
      <SessionMessageItem
        message={{ role: "tool", content: lines.join("\n") }}
        isActive={false}
        onCopy={vi.fn()}
      />,
    );

    expect(screen.getByText(/tool line 1/)).toBeInTheDocument();
    expect(screen.queryByText(/tool line 12/)).not.toBeInTheDocument();

    const expandButton = screen.getByRole("button", {
      name: "展开工具输出（共 12 行）",
    });
    expect(expandButton).toHaveAttribute("aria-expanded", "false");
    fireEvent.click(expandButton);

    expect(screen.getByText(/tool line 12/)).toBeInTheDocument();
    const collapseButton = screen.getByRole("button", {
      name: "收起工具输出",
    });
    expect(collapseButton).toHaveAttribute("aria-expanded", "true");
    fireEvent.click(collapseButton);
    expect(screen.queryByText(/tool line 12/)).not.toBeInTheDocument();
  });

  it("renders thinking messages collapsed by default and expands or loads full content on demand", async () => {
    const { container, rerender } = render(
      <SessionMessageItem
        message={{
          role: "thinking",
          content: "第一行思考\n第二行详细推理细节",
        }}
        isActive={false}
        onCopy={vi.fn()}
      />,
    );

    expect(container.firstElementChild).toHaveClass(
      "asm-message",
      "is-thinking",
    );
    expect(screen.getByText("思考过程")).toBeInTheDocument();
    expect(
      screen.getByText("第一行思考 第二行详细推理细节"),
    ).toBeInTheDocument();

    const expandBtn = screen.getByRole("button", { name: "展开思考过程" });
    expect(expandBtn).toHaveAttribute("aria-expanded", "false");
    fireEvent.click(expandBtn);

    expect(
      screen.getByRole("button", { name: "收起思考过程" }),
    ).toHaveAttribute("aria-expanded", "true");

    // Truncated thinking block fetches full content on expand
    const onLoadFullContent = vi.fn(async () => "完整长思考内容：最终结论");
    rerender(
      <SessionMessageItem
        message={{
          role: "thinking",
          content: "截断思考预览",
          index: 2,
          truncated: true,
          contentChars: 6000,
        }}
        isActive={false}
        onCopy={vi.fn()}
        onLoadFullContent={onLoadFullContent}
      />,
    );

    expect(onLoadFullContent).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "展开思考过程" }));
    expect(
      await screen.findByText("完整长思考内容：最终结论"),
    ).toBeInTheDocument();
    expect(onLoadFullContent).toHaveBeenCalledTimes(1);
  });

  it("copies full un-sliced code block when collapsed locally or truncated on backend", async () => {
    const onCopyCode = vi.fn();
    const fullCode = Array.from(
      { length: 120 },
      (_, idx) => `const line_${idx} = ${idx};`,
    ).join("\n");
    const longMarkdown = [
      "完整实现如下：",
      "```ts",
      fullCode,
      "```",
      "收尾说明".repeat(300),
    ].join("\n");
    expect(longMarkdown.length).toBeGreaterThan(3000);

    // 1. Locally collapsed (> 3000 chars, not server-truncated)
    const { unmount } = render(
      <SessionMessageItem
        message={{ role: "assistant", content: longMarkdown }}
        isActive={false}
        onCopy={vi.fn()}
        onCopyCode={onCopyCode}
      />,
    );

    expect(
      screen.getByRole("button", { name: /展开完整内容/ }),
    ).toHaveAttribute("aria-expanded", "false");
    fireEvent.click(screen.getByRole("button", { name: "复制代码" }));
    expect(onCopyCode).toHaveBeenCalledWith(fullCode);
    unmount();

    // 2. Server-truncated (> 4096 chars, preview sliced at 1500 chars)
    onCopyCode.mockReset();
    let resolveFull!: (val: string) => void;
    const onLoadFullContent = vi.fn(
      () =>
        new Promise<string>((resolve) => {
          resolveFull = resolve;
        }),
    );

    render(
      <SessionMessageItem
        message={{
          role: "assistant",
          content: longMarkdown.slice(0, 1500),
          index: 1,
          truncated: true,
          contentChars: longMarkdown.length,
        }}
        isActive={false}
        onCopy={vi.fn()}
        onCopyCode={onCopyCode}
        onLoadFullContent={onLoadFullContent}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "复制代码" }));
    await waitFor(() => expect(onLoadFullContent).toHaveBeenCalledTimes(1));
    expect(screen.queryByRole("button", { name: "已复制" })).toBeNull();

    resolveFull(longMarkdown);
    await waitFor(() => expect(onCopyCode).toHaveBeenCalledWith(fullCode));
    expect(
      await screen.findByRole("button", { name: "已复制" }),
    ).toBeInTheDocument();
  });
});
