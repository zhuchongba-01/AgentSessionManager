import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { TooltipProvider } from "@/components/ui/tooltip";
import { getAgentReaderStyle } from "@/components/sessions/agentReaderStyles";
import {
  ReaderAgentLane,
  ReaderFinalReplyRow,
  ReaderQuestionRow,
  ReaderStepRow,
} from "@/components/sessions/ReaderTurnRows";
import {
  extractDiffPayload,
  parseUnifiedDiff,
  SessionDiffView,
} from "@/components/sessions/SessionDiffView";

describe("cc-switch style conversation reader components", () => {
  it("parses unified diffs and apply_patch payloads with old/new line numbers", () => {
    const patch = [
      "[Tool: apply_patch]",
      "*** Begin Patch",
      "*** Update File: /repo/app/src/login.ts",
      "@@ -12,2 +12,3 @@",
      " const ok = true;",
      "-return false;",
      "+const verified = ok;",
      "+return verified;",
      "*** End Patch",
    ].join("\n");

    const payload = extractDiffPayload(patch);
    expect(payload).not.toBeNull();
    expect(payload?.added).toBe(2);
    expect(payload?.removed).toBe(1);

    const parsed = parseUnifiedDiff(payload!.diffText);
    expect(parsed[0]).toEqual({
      type: "file",
      path: "/repo/app/src/login.ts",
    });

    const { container } = render(
      <SessionDiffView diffText={payload!.diffText} projectDir="/repo/app" />,
    );
    expect(screen.getByText("src/login.ts")).toBeInTheDocument();
    const addRows = container.querySelectorAll(".asm-ccs-diff-row.is-add");
    const delRows = container.querySelectorAll(".asm-ccs-diff-row.is-del");
    expect(addRows).toHaveLength(2);
    expect(delRows).toHaveLength(1);
    expect(delRows[0]).toHaveTextContent("13");
    expect(addRows[0]).toHaveTextContent("13");
    expect(addRows[1]).toHaveTextContent("14");
  });

  it("renders per-agent native step verbs, right-aligned user bubble, and borderless final reply", () => {
    const onCopy = vi.fn();
    const claudeStyle = getAgentReaderStyle("claude");
    const codexStyle = getAgentReaderStyle("codex");

    render(
      <TooltipProvider>
        <div>
          <ReaderQuestionRow
            message={{ role: "user", content: "请重构会话阅读器", ts: 1000 }}
            onCopy={onCopy}
          />
          <ReaderAgentLane
            providerId="claude"
            providerLabel="Claude Code"
            isAgentStart={true}
            agentStartTs={1005}
          >
            <ReaderStepRow
              message={{
                role: "tool",
                content: "[Tool: Bash]\ncargo test --offline\ntest result: ok",
              }}
              style={claudeStyle}
              onCopy={onCopy}
            />
          </ReaderAgentLane>
          <ReaderAgentLane
            providerId="claude"
            providerLabel="Claude Code"
            isAgentStart={false}
          >
            <ReaderStepRow
              message={{
                role: "tool",
                content: "[Tool: exec_command]\npnpm test\nall green",
              }}
              style={codexStyle}
              onCopy={onCopy}
            />
          </ReaderAgentLane>
          <ReaderAgentLane
            providerId="claude"
            providerLabel="Claude Code"
            isAgentStart={false}
          >
            <ReaderFinalReplyRow
              message={{
                role: "assistant",
                content: "已完成左右对话流重构。",
                ts: 1010,
              }}
              onCopy={onCopy}
            />
          </ReaderAgentLane>
        </div>
      </TooltipProvider>,
    );

    expect(screen.getByText("请重构会话阅读器")).toBeInTheDocument();
    // 同一轮仅在首条 Agent 行渲染一次头像与 Agent 名称标题
    expect(screen.getAllByText("Claude Code")).toHaveLength(1);

    // Claude 使用函数调用签名 Bash(cargo test --offline)，Codex 使用动词短语 Ran pnpm test
    const claudeStepBtn = screen.getByRole("button", {
      name: /Bash\s*\(\s*cargo test --offline\s*\)/,
    });
    expect(claudeStepBtn).toHaveTextContent("Bash(cargo test --offline)");
    expect(claudeStepBtn).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByText("test result: ok")).not.toBeInTheDocument();

    fireEvent.click(claudeStepBtn);
    expect(claudeStepBtn).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByText("test result: ok")).toBeInTheDocument();

    expect(
      screen.getByRole("button", { name: /Ran pnpm test/ }),
    ).toBeInTheDocument();
    expect(screen.getByText("已完成左右对话流重构。")).toBeInTheDocument();
  });
});
