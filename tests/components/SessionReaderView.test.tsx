import { useState } from "react";
import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { TooltipProvider } from "@/components/ui/tooltip";
import { getAgentReaderStyle } from "@/components/sessions/agentReaderStyles";
import {
  ReaderAgentLane,
  ReaderEventRow,
  ReaderFinalReplyRow,
  ReaderQuestionRow,
  ReaderStepRow,
  ReaderTimelineRow,
} from "@/components/sessions/ReaderTurnRows";
import {
  extractDiffPayload,
  parseUnifiedDiff,
  SessionDiffView,
} from "@/components/sessions/SessionDiffView";
import {
  buildReaderLayout,
  groupMessagesIntoTurns,
} from "@/components/sessions/turnGrouping";
import type { SessionMessage } from "@/types";

function HarnessReader({
  messages,
  providerId,
  providerLabel,
}: {
  messages: SessionMessage[];
  providerId: string;
  providerLabel: string;
}) {
  const [expandedTurns, setExpandedTurns] = useState<Set<string>>(
    () => new Set(),
  );
  const [collapsedTurns, setCollapsedTurns] = useState<Set<string>>(
    () => new Set(),
  );
  const [stepOverrides, setStepOverrides] = useState<Map<string, boolean>>(
    () => new Map(),
  );

  const style = getAgentReaderStyle(providerId);
  const turns = groupMessagesIntoTurns(messages, providerId);
  const layout = buildReaderLayout({
    messages,
    turns,
    viewMode: "all",
    expandAllTimelines: false,
    expandedTurnKeys: expandedTurns,
    collapsedTurnKeys: collapsedTurns,
    stepOverrides,
    matchedMessageSet: new Set(),
  });
  const turnsByIndex = new Map(turns.map((t) => [t.turnIndex, t]));
  const onCopy = vi.fn();

  return (
    <TooltipProvider>
      <div>
        {layout.items.map((item) => {
          if (item.kind === "divider") {
            return <div key={item.key} className="asm-ccs-turn-divider" />;
          }
          if (item.kind === "event") {
            return <ReaderEventRow key={item.key} event={item.event} />;
          }
          if (item.kind === "question") {
            return (
              <ReaderQuestionRow
                key={item.key}
                message={item.message}
                onCopy={onCopy}
              />
            );
          }
          if (item.kind === "timeline") {
            return (
              <ReaderAgentLane
                key={item.key}
                providerId={providerId}
                providerLabel={providerLabel}
                isAgentStart={item.isAgentStart}
                agentStartTs={item.turn.agentStartTs}
                model={item.turn.model}
              >
                <ReaderTimelineRow
                  turn={item.turn}
                  expanded={item.expanded}
                  hiddenFailures={item.hiddenFailures}
                  onToggle={() => {
                    if (item.expanded) {
                      setCollapsedTurns(
                        (prev) => new Set([...prev, item.turnKey]),
                      );
                      setExpandedTurns((prev) => {
                        const next = new Set(prev);
                        next.delete(item.turnKey);
                        return next;
                      });
                    } else {
                      setExpandedTurns(
                        (prev) => new Set([...prev, item.turnKey]),
                      );
                      setCollapsedTurns((prev) => {
                        const next = new Set(prev);
                        next.delete(item.turnKey);
                        return next;
                      });
                    }
                  }}
                />
              </ReaderAgentLane>
            );
          }
          if (item.kind === "step") {
            return (
              <ReaderAgentLane
                key={item.key}
                providerId={providerId}
                providerLabel={providerLabel}
                isAgentStart={item.isAgentStart}
                agentStartTs={turnsByIndex.get(item.turnIndex)?.agentStartTs}
                model={turnsByIndex.get(item.turnIndex)?.model}
              >
                <ReaderStepRow
                  step={item.step}
                  style={style}
                  depth={item.depth}
                  inTimeline={item.inTimeline}
                  expanded={item.expanded}
                  previewLines={item.previewLines}
                  onToggleStep={(stepId, cur) => {
                    setStepOverrides((prev) => {
                      const next = new Map(prev);
                      next.set(stepId, !cur);
                      return next;
                    });
                  }}
                  onCopy={onCopy}
                />
              </ReaderAgentLane>
            );
          }
          if (item.kind === "final") {
            return (
              <ReaderAgentLane
                key={item.key}
                providerId={providerId}
                providerLabel={providerLabel}
                isAgentStart={item.isAgentStart}
                agentStartTs={turnsByIndex.get(item.turnIndex)?.agentStartTs}
                model={turnsByIndex.get(item.turnIndex)?.model}
              >
                <ReaderFinalReplyRow
                  message={item.message}
                  overrideText={item.overrideText}
                  onCopy={onCopy}
                />
              </ReaderAgentLane>
            );
          }
          return null;
        })}
      </div>
    </TooltipProvider>
  );
}

describe("cc-switch style conversation & chain-of-thought reader", () => {
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

  it("renders per-agent thinking labels, inline summaries, redacted static rows, and expandable italic reasoning", () => {
    const messages: SessionMessage[] = [
      { role: "user", content: "为什么 JWT 校验失败？", ts: 1000 },
      // 单步思考（无工具）：按 cc-switch 规则直接平铺思考行，不套一层多余的「执行过程 · 1 步」
      {
        role: "thinking",
        content:
          "**核对时钟偏移与签名密钥**\n先排查服务端 exp 与签发端密钥是否一致。",
        ts: 1002,
        meta: {
          thinkingSummary: "核对时钟偏移与签名密钥",
          durationMs: 4200,
          model: "gpt-5-codex",
        },
      },
      { role: "assistant", content: "原因是签发端使用了旧版私钥。", ts: 1006 },
      { role: "user", content: "再检查加密推理块", ts: 2000 },
      {
        role: "thinking",
        content: "[Redacted Thinking]",
        ts: 2001,
        meta: { redacted: true, durationMs: 3000 },
      },
      { role: "assistant", content: "加密思考块已安全处理。", ts: 2005 },
    ];

    render(
      <HarnessReader
        messages={messages}
        providerId="codex"
        providerLabel="Codex"
      />,
    );

    // 模型名展示在 Agent 标题行
    expect(screen.getByText("· gpt-5-codex")).toBeInTheDocument();

    // 单步思考不包「执行过程 · 1 步」，直接渲染 Codex 原生「推理」标签 + 同行摘要 + 字数与耗时
    expect(screen.queryByText(/执行过程/)).not.toBeInTheDocument();
    const thinkingBtn = screen.getByRole("button", { name: "展开思考过程" });
    expect(thinkingBtn).toHaveTextContent("推理");
    expect(thinkingBtn).toHaveTextContent("核对时钟偏移与签名密钥");
    expect(thinkingBtn).toHaveTextContent("4.2s");
    expect(
      screen.queryByText("先排查服务端 exp 与签发端密钥是否一致。"),
    ).not.toBeInTheDocument();

    // 点击展开次级斜体思考区域（role="region"）
    fireEvent.click(thinkingBtn);
    const region = screen.getByRole("region", { name: "推理" });
    expect(region).toHaveTextContent("先排查服务端 exp 与签发端密钥是否一致。");

    // 第二轮 redacted_thinking 静态展示「思考内容不可见」，不可点开
    expect(screen.getByText("思考内容不可见")).toBeInTheDocument();
  });

  it("pairs tool_call + tool_result by callId, merges Codex Explored runs, clamps intermediate notes, and pins failures", () => {
    const messages: SessionMessage[] = [
      {
        role: "system",
        content: "[Compacted Summary]\n已归纳前 20 轮架构讨论摘要。",
        ts: 500,
      },
      { role: "user", content: "排查并运行单测", ts: 1000 },
      {
        role: "assistant",
        content:
          "第一行中间过程说明\n第二行中间过程说明\n第三行中间过程说明\n第四行超长中间说明用于触发三行渐隐折叠",
        ts: 1002,
        meta: {
          blocks: [
            {
              type: "text",
              text: "第一行中间过程说明\n第二行中间过程说明\n第三行中间过程说明\n第四行超长中间说明用于触发三行渐隐折叠",
            },
            {
              type: "tool_call",
              id: "call_r1",
              rawName: "read_file",
              title: "src/a.ts",
              inputPreview: '{"path":"src/a.ts"}',
              inputTruncated: false,
            },
            {
              type: "tool_call",
              id: "call_r2",
              rawName: "grep",
              title: "verifyToken",
              inputPreview: '{"pattern":"verifyToken"}',
              inputTruncated: false,
            },
            {
              type: "tool_call",
              id: "call_sh",
              rawName: "exec_command",
              title: "pnpm test",
              inputPreview: '{"cmd":"pnpm test"}',
              inputTruncated: false,
            },
          ],
        },
      },
      {
        role: "tool",
        content: "file a content",
        meta: {
          blocks: [
            {
              type: "tool_result",
              callId: "call_r1",
              status: "success",
              output: "export const a = 1;",
              lineCount: 1,
            },
            {
              type: "tool_result",
              callId: "call_r2",
              status: "success",
              output: "src/a.ts:1:verifyToken",
              lineCount: 1,
            },
            {
              type: "tool_result",
              callId: "call_sh",
              status: "error",
              output: "FAIL src/a.test.ts\nExpected 200, received 401",
              lineCount: 2,
              exitCode: 1,
              durationMs: 1500,
            },
          ],
        },
      },
      { role: "assistant", content: "发现单测返回 401，准备修复。", ts: 1010 },
    ];

    render(
      <HarnessReader
        messages={messages}
        providerId="codex"
        providerLabel="Codex"
      />,
    );

    // 1. 顶部居中可展开事件分隔行「上下文已压缩」
    const compactBtn = screen.getByRole("button", { name: /上下文已压缩/ });
    expect(
      screen.queryByText("已归纳前 20 轮架构讨论摘要。"),
    ).not.toBeInTheDocument();
    fireEvent.click(compactBtn);
    expect(
      screen.getByText("已归纳前 20 轮架构讨论摘要。"),
    ).toBeInTheDocument();

    // 2. 折叠态时间线：1 条说明 + 2 条探索 + 1 条命令 = 4 步，1 个命令，1 处异常
    const timelineBtn = screen.getByRole("button", {
      name: /执行过程 · 4 步/,
    });
    expect(timelineBtn).toHaveAttribute("aria-expanded", "false");
    expect(timelineBtn).toHaveTextContent("1 个命令");
    expect(timelineBtn).toHaveTextContent("1 处异常");

    // 3. 折叠态下失败步骤「Ran pnpm test」按 cc-switch 规则直接拎在摘要条下方常显并展开报错预览！
    expect(
      screen.getByRole("button", { name: /Ran pnpm test/ }),
    ).toBeInTheDocument();
    expect(screen.getByText(/Expected 200, received 401/)).toBeInTheDocument();
    expect(screen.getByText("exit 1")).toBeInTheDocument();

    // 4. 点开时间线：看到连续读/搜合并成的 Explored 行 + 3 行渐隐折叠的中间说明
    fireEvent.click(timelineBtn);
    const exploredBtn = screen.getByRole("button", {
      name: /Explored.*查看了 1 个文件、搜索 1 次/,
    });
    expect(exploredBtn).toBeInTheDocument();

    const expandNoteBtn = screen.getByRole("button", { name: "展开说明" });
    expect(expandNoteBtn).toBeInTheDocument();
    fireEvent.click(expandNoteBtn);
    expect(screen.getByRole("button", { name: "收起" })).toBeInTheDocument();

    // 5. 点开合并的 Explored 步骤：展开缩进子步骤 Read src/a.ts 与 Search verifyToken
    fireEvent.click(exploredBtn);
    expect(
      screen.getByRole("button", { name: /Read src\/a\.ts/ }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /Search verifyToken/ }),
    ).toBeInTheDocument();
  });
});
