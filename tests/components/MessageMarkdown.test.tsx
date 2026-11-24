import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { MessageMarkdown } from "@/components/sessions/MessageMarkdown";

describe("MessageMarkdown", () => {
  it("renders fenced code blocks with language label and copies exact code", () => {
    const onCopyCode = vi.fn();
    render(
      <MessageMarkdown
        content={["Here is a fix:", "```ts", "const answer = 42;", "```"].join(
          "\n",
        )}
        onCopyCode={onCopyCode}
      />,
    );

    expect(screen.getByText("Here is a fix:")).toBeInTheDocument();
    expect(screen.getByText("ts")).toBeInTheDocument();
    expect(screen.getByText("const answer = 42;")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "复制代码" }));
    expect(onCopyCode).toHaveBeenCalledTimes(1);
    expect(onCopyCode).toHaveBeenCalledWith("const answer = 42;", 1);
  });

  it("awaits async onCopyCode before showing copied state", async () => {
    let resolveCopy!: () => void;
    const onCopyCode = vi.fn(
      () =>
        new Promise<void>((resolve) => {
          resolveCopy = resolve;
        }),
    );

    render(
      <MessageMarkdown
        content={["```ts", "const delayed = true;", "```"].join("\n")}
        onCopyCode={onCopyCode}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "复制代码" }));
    expect(screen.queryByRole("button", { name: "已复制" })).toBeNull();

    resolveCopy();
    expect(
      await screen.findByRole("button", { name: "已复制" }),
    ).toBeInTheDocument();
  });

  it("does not pollute unclosed collapsed code blocks with ellipsis inside code", () => {
    const { container } = render(
      <MessageMarkdown
        content={["```ts", "const partial = 1;"].join("\n")}
        collapsed
        onCopyCode={vi.fn()}
      />,
    );

    expect(container.querySelector("pre.asm-code-pre code")?.textContent).toBe(
      "const partial = 1;",
    );
    expect(container.querySelector(".asm-code-truncated")).toHaveTextContent(
      "…",
    );
  });

  it("renders double-backtick inline code spans including nested backticks", () => {
    const { container } = render(
      <MessageMarkdown
        content="Use ``double`` and `` `nested` `` inline code."
        onCopyCode={vi.fn()}
      />,
    );

    const codes = Array.from(
      container.querySelectorAll("code.asm-inline-code"),
    );
    expect(codes.map((el) => el.textContent)).toEqual(["double", "`nested`"]);
  });

  it("applies diff line classes for diff code blocks", () => {
    const { container } = render(
      <MessageMarkdown
        content={[
          "```diff",
          "@@ -1,2 +1,2 @@",
          "-const a = 1;",
          "+const a = 2;",
          "```",
        ].join("\n")}
        onCopyCode={vi.fn()}
      />,
    );

    expect(container.querySelector(".asm-diff-line.is-hunk")).toHaveTextContent(
      "@@ -1,2 +1,2 @@",
    );
    expect(container.querySelector(".asm-diff-line.is-del")).toHaveTextContent(
      "-const a = 1;",
    );
    expect(container.querySelector(".asm-diff-line.is-add")).toHaveTextContent(
      "+const a = 2;",
    );
  });

  it("renders headings without semantic h1-h6, plus lists, quotes, inline code, bold, and highlights", () => {
    const { container } = render(
      <MessageMarkdown
        content={[
          "## 修复小结",
          "",
          "> 引用说明 `config.toml` 的改动",
          "",
          "- 使用 **纯组件** 解析 `alpha`",
          "- 第二项",
        ].join("\n")}
        highlightTerms={["alpha"]}
        onCopyCode={vi.fn()}
      />,
    );

    expect(screen.queryAllByRole("heading")).toHaveLength(0);
    expect(container.querySelector(".asm-md-heading.is-h2")).toHaveTextContent(
      "修复小结",
    );
    expect(container.querySelector("blockquote.asm-md-quote")).toBeTruthy();
    expect(container.querySelector("code.asm-inline-code")).toBeTruthy();
    expect(container.querySelector("strong")).toHaveTextContent("纯组件");
    const mark = container.querySelector("mark.asm-search-mark");
    expect(mark).toHaveTextContent("alpha");
    expect(container.querySelectorAll("ul.asm-md-list li")).toHaveLength(2);
  });

  it("passes plain text through cleanly", () => {
    render(
      <MessageMarkdown
        content="普通回复文本，没有任何格式标记"
        onCopyCode={vi.fn()}
      />,
    );
    expect(
      screen.getByText("普通回复文本，没有任何格式标记"),
    ).toBeInTheDocument();
  });

  it("renders GFM tables, task lists, italic, strikethrough, and safe HTTPS links", () => {
    const onOpenUrl = vi.fn();
    const { container } = render(
      <MessageMarkdown
        content={[
          "| 功能 | 状态 |",
          "| :--- | :---: |",
          "| 全文搜索 | *已支持* |",
          "| 旧版接口 | ~~已废弃~~ |",
          "",
          "- [x] 完成 FTS5 索引",
          "- [ ] 待验证安装包",
          "",
          "参考 [官方文档](https://example.com/docs) 与 [不安全链接](javascript:alert(1))",
        ].join("\n")}
        onCopyCode={vi.fn()}
        onOpenUrl={onOpenUrl}
      />,
    );

    expect(container.querySelector("table.asm-md-table")).toBeTruthy();
    expect(container.querySelector("em")).toHaveTextContent("已支持");
    expect(container.querySelector("del.asm-md-del")).toHaveTextContent(
      "已废弃",
    );
    expect(container.querySelectorAll("li.asm-md-task-item")).toHaveLength(2);
    expect(
      container.querySelectorAll(".asm-md-task-box.is-checked"),
    ).toHaveLength(1);

    const links = container.querySelectorAll("a.asm-md-link");
    expect(links).toHaveLength(1);
    expect(links[0]).toHaveTextContent("官方文档");
    fireEvent.click(links[0]);
    expect(onOpenUrl).toHaveBeenCalledWith("https://example.com/docs");
  });
});
