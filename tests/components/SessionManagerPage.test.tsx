import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { http, HttpResponse } from "msw";
import { SessionManagerPage } from "@/components/sessions/SessionManagerPage";
import { ThemeProvider } from "@/components/theme-provider";
import {
  APP_VERSION,
  GITHUB_LATEST_RELEASE_API,
  GITHUB_LATEST_RELEASE_URL,
  GITHUB_REPOSITORY_URL,
} from "@/lib/githubRelease";
import { piApi } from "@/lib/api/pi";
import { sessionsApi } from "@/lib/api/sessions";
import type { SessionMessage, SessionMeta } from "@/types";
import { setSessionFixtures } from "../msw/state";
import { server } from "../msw/server";

const { openUrlMock } = vi.hoisted(() => ({ openUrlMock: vi.fn() }));
const toastSuccessMock = vi.fn();
const toastErrorMock = vi.fn();
const toastWarningMock = vi.fn();
const toastInfoMock = vi.fn();
const GROUP_EXPANSION_STORAGE_KEY = "agent-session-manager.groupExpansionState";

vi.mock("sonner", () => ({
  toast: {
    success: (...args: unknown[]) => toastSuccessMock(...args),
    error: (...args: unknown[]) => toastErrorMock(...args),
    warning: (...args: unknown[]) => toastWarningMock(...args),
    info: (...args: unknown[]) => toastInfoMock(...args),
  },
}));

vi.mock("@tauri-apps/plugin-opener", () => ({
  openUrl: openUrlMock,
}));

const renderPage = (appId = "codex") => {
  const client = new QueryClient({
    defaultOptions: {
      queries: { retry: false },
      mutations: { retry: false },
    },
  });

  return {
    client,
    ...render(
      <QueryClientProvider client={client}>
        <ThemeProvider defaultTheme="light" storageKey="test-session-theme">
          <SessionManagerPage appId={appId} />
        </ThemeProvider>
      </QueryClientProvider>,
    ),
  };
};

const switchToGroupedView = async () => {
  const groupedOption = screen.getByRole("button", { name: "分类" });
  if (groupedOption.getAttribute("aria-pressed") !== "true") {
    await userEvent.click(groupedOption);
  }
};

const switchProviderFilter = async (providerLabel: RegExp) => {
  const providerFilterTrigger = screen.getByRole("combobox", {
    name: /Agent 筛选/i,
  });

  await userEvent.click(providerFilterTrigger);
  await userEvent.click(
    await screen.findByRole("option", { name: providerLabel }),
  );
};

const enterGroupedBatchMode = async () => {
  await switchToGroupedView();
  fireEvent.click(screen.getByRole("button", { name: /批量管理/i }));
};

/**
 * 打开批量删除确认框。
 *
 * 批量工具条上已经没有删除按钮：唯一入口是右侧详情面板里那枚按钮，它在批量模式
 * 下显示「删除已勾选的 N 条」。这个 helper 就是那个入口，避免各用例各写一遍正则。
 */
const batchDeleteButton = (count?: number) =>
  screen.getByRole("button", {
    name:
      count === undefined
        ? /删除已勾选的 \d+ 条/
        : new RegExp(`删除已勾选的 ${count} 条`),
  });

const openBatchDeleteDialog = (count?: number) => {
  fireEvent.click(batchDeleteButton(count));
};

const expandDirectoryGroup = (provider: string, directory: string) => {
  let directoryToggle = screen.queryByRole("button", {
    name: new RegExp(`展开或折叠 ${directory} 目录分组`),
  });

  if (!directoryToggle) {
    fireEvent.click(
      screen.getByRole("button", {
        name: new RegExp(`展开或折叠 ${provider} 供应商分组`),
      }),
    );
    directoryToggle = screen.getByRole("button", {
      name: new RegExp(`展开或折叠 ${directory} 目录分组`),
    });
  }

  if (directoryToggle.getAttribute("aria-expanded") !== "true") {
    fireEvent.click(directoryToggle);
  }
};

describe("SessionManagerPage", () => {
  beforeEach(() => {
    openUrlMock.mockResolvedValue(undefined);
    toastSuccessMock.mockReset();
    toastErrorMock.mockReset();
    toastWarningMock.mockReset();
    toastInfoMock.mockReset();
    Element.prototype.scrollIntoView = vi.fn();
    window.localStorage.removeItem("agent-session-manager.listViewMode");
    window.localStorage.removeItem(GROUP_EXPANSION_STORAGE_KEY);

    const sessions: SessionMeta[] = [
      {
        providerId: "codex",
        sessionId: "codex-session-1",
        residual: true,
        title: "Alpha Session",
        summary: "Alpha summary",
        projectDir: "/mock/codex",
        createdAt: 2,
        lastActiveAt: 20,
        sourcePath: "/mock/codex/session-1.jsonl",
        resumeCommand: "codex resume codex-session-1",
      },
      {
        providerId: "codex",
        sessionId: "codex-session-2",
        title: "Beta Session",
        summary: "Beta summary",
        projectDir: "/mock/codex",
        createdAt: 1,
        lastActiveAt: 10,
        sourcePath: "/mock/codex/session-2.jsonl",
        resumeCommand: "codex resume codex-session-2",
      },
      {
        providerId: "claude",
        sessionId: "claude-session-1",
        title: "Claude Session",
        summary: "Claude summary",
        projectDir: "/mock/claude",
        createdAt: 3,
        lastActiveAt: 30,
        sourcePath: "/mock/claude/session-1.jsonl",
        resumeCommand: "claude --resume claude-session-1",
      },
      {
        providerId: "codex",
        sessionId: "codex-session-3",
        title: "Gamma Session",
        summary: "Gamma summary",
        projectDir: null,
        createdAt: 0,
        lastActiveAt: 5,
        sourcePath: "/mock/codex/session-3.jsonl",
        resumeCommand: "codex resume codex-session-3",
      },
    ];
    const messages: Record<string, SessionMessage[]> = {
      "codex:/mock/codex/session-1.jsonl": [
        { role: "user", content: "alpha", ts: 20 },
      ],
      "codex:/mock/codex/session-2.jsonl": [
        { role: "user", content: "beta", ts: 10 },
      ],
      "codex:/mock/codex/session-3.jsonl": [
        { role: "user", content: "gamma", ts: 5 },
      ],
      "claude:/mock/claude/session-1.jsonl": [
        { role: "user", content: "claude", ts: 30 },
      ],
    };

    setSessionFixtures(sessions, messages);
  });

  it("always links to the project and hides the update button on the current release", async () => {
    renderPage();

    const projectLink = await screen.findByRole("button", {
      name: "在 GitHub 查看项目",
    });
    expect(
      screen.queryByRole("button", { name: /发现新版本/ }),
    ).not.toBeInTheDocument();

    await userEvent.click(projectLink);
    expect(openUrlMock).toHaveBeenCalledWith(GITHUB_REPOSITORY_URL);
  });

  it("shows a download button only when GitHub has a newer stable release", async () => {
    const [major, minor, patch] = APP_VERSION.split(".").map(Number);
    const nextVersion = `${major}.${minor}.${patch + 1}`;
    server.use(
      http.get(GITHUB_LATEST_RELEASE_API, () =>
        HttpResponse.json({
          tag_name: `v${nextVersion}`,
          body: "## 更新内容\n- 修复更新提示\n- 优化 Agent 图标",
          draft: false,
          prerelease: false,
        }),
      ),
    );
    renderPage();

    const updateButton = await screen.findByRole("button", {
      name: `发现新版本 v${nextVersion}`,
    });
    expect(updateButton).toHaveTextContent("更新");
    await userEvent.hover(updateButton);
    expect(await screen.findByRole("tooltip")).toHaveTextContent(
      `发现新版本 v${nextVersion}`,
    );
    expect(screen.getByRole("tooltip")).toHaveTextContent(
      "修复更新提示 优化 Agent 图标",
    );
    await userEvent.click(updateButton);

    expect(openUrlMock).toHaveBeenCalledWith(GITHUB_LATEST_RELEASE_URL);
  });

  it("surfaces a relative Pi sessionDir instead of presenting an empty scan as authoritative", async () => {
    const discovery = vi.spyOn(piApi, "getSessionDiscovery").mockResolvedValue({
      status: "requires_project_context",
      configuredPath: ".pi/sessions",
    });

    renderPage("pi");

    const notice = await screen.findByRole("status");
    expect(notice).toHaveTextContent(".pi/sessions");
    expect(discovery).toHaveBeenCalledTimes(1);
    discovery.mockRestore();
  });

  it("surfaces Pi discovery problems from the standalone all-provider entry", async () => {
    const discovery = vi.spyOn(piApi, "getSessionDiscovery").mockResolvedValue({
      status: "requires_project_context",
      configuredPath: ".pi/sessions",
    });

    renderPage("all");

    const notice = await screen.findByRole("status");
    expect(notice).toHaveTextContent(".pi/sessions");
    expect(discovery).toHaveBeenCalledTimes(1);
    discovery.mockRestore();
  });

  it("labels sessions that are not present in the native agent list as residual", async () => {
    renderPage();

    expect((await screen.findAllByText("残留")).length).toBeGreaterThan(0);
  });

  it("reports a completed rescan when no new sessions are found", async () => {
    renderPage("all");

    const rescan = await screen.findByRole("button", { name: "重新扫描" });
    toastSuccessMock.mockReset();
    fireEvent.click(rescan);

    await waitFor(() =>
      expect(toastSuccessMock).toHaveBeenCalledWith(
        "扫描完成：没有发现新会话，共 4 条",
      ),
    );
  });

  it("shows real provider icons and session counts in the Agent filter", async () => {
    renderPage("all");

    const filter = await screen.findByRole("combobox", {
      name: /Agent 筛选/i,
    });
    await userEvent.click(filter);

    const codexOption = await screen.findByRole("option", { name: /Codex/ });
    expect(within(codexOption).getByTitle("Codex")).toBeInTheDocument();
    expect(codexOption).toHaveTextContent("3");

    const claudeOption = screen.getByRole("option", { name: /Claude Code/ });
    expect(within(claudeOption).getByTitle("Claude Code")).toBeInTheDocument();
    expect(claudeOption).toHaveTextContent("1");

    const zcodeOption = screen.getByRole("option", { name: /ZCode/ });
    expect(within(zcodeOption).getByTitle("ZCode")).toBeInTheDocument();
    expect(zcodeOption).toHaveTextContent("0");

    const deepseekOption = screen.getByRole("option", {
      name: /DeepSeek Harness/,
    });
    expect(
      within(deepseekOption).getByTitle("DeepSeek Harness"),
    ).toBeInTheDocument();
    expect(deepseekOption).toHaveTextContent("0");
  });

  it("coalesces rapid rescan clicks into one disk scan and one notification", async () => {
    const listSpy = vi.spyOn(sessionsApi, "list");
    renderPage("all");

    const rescan = await screen.findByRole("button", { name: "重新扫描" });
    await waitFor(() => expect(rescan).not.toBeDisabled());
    listSpy.mockClear();
    toastSuccessMock.mockReset();

    fireEvent.click(rescan);
    fireEvent.click(rescan);

    await waitFor(() => expect(toastSuccessMock).toHaveBeenCalledTimes(1));
    expect(listSpy).toHaveBeenCalledTimes(1);
    listSpy.mockRestore();
  });

  it("deletes the selected session and selects the next visible session", async () => {
    renderPage();

    await waitFor(() =>
      expect(
        screen.getByRole("heading", { name: "Alpha Session" }),
      ).toBeInTheDocument(),
    );

    fireEvent.click(screen.getByRole("button", { name: /仅删除此会话/i }));

    const dialog = screen.getByTestId("confirm-dialog");
    expect(dialog).toBeInTheDocument();
    expect(within(dialog).getByText(/Alpha Session/)).toBeInTheDocument();

    fireEvent.click(within(dialog).getByRole("button", { name: /删除会话/i }));

    await waitFor(() =>
      expect(
        screen.getByRole("heading", { name: "Beta Session" }),
      ).toBeInTheDocument(),
    );

    expect(screen.queryByText("Alpha Session")).not.toBeInTheDocument();
    expect(toastErrorMock).not.toHaveBeenCalled();
    expect(toastSuccessMock).toHaveBeenCalled();
  });

  it("keeps the exit batch mode button visible when the provider filter empties the list", async () => {
    renderPage();

    await waitFor(() =>
      expect(
        screen.getByRole("heading", { name: "Alpha Session" }),
      ).toBeInTheDocument(),
    );

    fireEvent.click(screen.getByRole("button", { name: /批量管理/i }));
    await switchProviderFilter(/ZCode/i);

    await waitFor(() => expect(screen.queryByText("Alpha Session")).toBeNull());

    expect(screen.getByRole("button", { name: /退出批量管理/i })).toBeVisible();
  });

  it("drops hidden selections when the provider filter narrows the result set", async () => {
    renderPage();

    await waitFor(() =>
      expect(
        screen.getByRole("heading", { name: "Alpha Session" }),
      ).toBeInTheDocument(),
    );

    fireEvent.click(screen.getByRole("button", { name: /批量管理/i }));
    fireEvent.click(screen.getByRole("button", { name: /全选筛选结果/i }));

    expect(screen.getByText("已选 3 项")).toBeInTheDocument();

    await switchProviderFilter(/Claude Code/i);

    await waitFor(() =>
      expect(screen.queryByText("Beta Session")).not.toBeInTheDocument(),
    );

    // 切走的供应商下原选中项全部不可见，应被清空而不是隐藏保留
    await waitFor(() =>
      expect(screen.getByText("已选 0 项")).toBeInTheDocument(),
    );
  });

  /**
   * 修复回归：详情面板（右侧）一直有一个红色删除按钮，它原本**只删当前打开的那一条**。
   * 用户在批量管理里勾选多条后点这个更显眼的按钮，就会得到「多选了却只删掉一个」。
   *
   * 现在它是批量模式下**唯一**的删除入口（工具条上那个已按需求移除），文案带计数。
   */
  it("is the only batch delete entry and deletes the whole selection", async () => {
    const deleteManySpy = vi.spyOn(sessionsApi, "deleteMany");
    const deleteSpy = vi.spyOn(sessionsApi, "delete");
    renderPage();

    await waitFor(() =>
      expect(
        screen.getByRole("heading", { name: "Alpha Session" }),
      ).toBeInTheDocument(),
    );

    await switchToGroupedView();
    fireEvent.click(screen.getByRole("button", { name: /批量管理/i }));
    fireEvent.click(screen.getByRole("button", { name: /全选筛选结果/i }));
    expect(screen.getByText("已选 3 项")).toBeInTheDocument();

    // 批量工具条上不该再有删除按钮（旧文案「批量删除」已不存在）
    expect(
      screen.queryByRole("button", { name: /批量删除/ }),
    ).not.toBeInTheDocument();
    // 删除入口只有右边这一个，且文案带计数
    expect(
      screen.getAllByRole("button", { name: /删除已勾选的 \d+ 条/ }),
    ).toHaveLength(1);

    openBatchDeleteDialog(3);

    const dialog = screen.getByTestId("confirm-dialog");
    expect(
      within(dialog).getByText(/已选中的 3 个本地会话记录/),
    ).toBeInTheDocument();
    fireEvent.click(
      within(dialog).getByRole("button", { name: /删除所选会话/i }),
    );

    await waitFor(() => expect(deleteManySpy).toHaveBeenCalled());
    expect(deleteManySpy.mock.calls[0][0]).toHaveLength(3);
    // 不能退化成本地单条删除
    expect(deleteSpy).not.toHaveBeenCalled();

    deleteManySpy.mockRestore();
    deleteSpy.mockRestore();
  });

  it("keeps the detail-pane delete button single-target outside batch mode", async () => {
    const deleteSpy = vi
      .spyOn(sessionsApi, "delete")
      .mockResolvedValue({ status: "deleted" });
    renderPage();
    await screen.findByRole("heading", { name: "Alpha Session" });

    // 未进入批量管理：按钮明确写成「仅删除此会话」
    fireEvent.click(screen.getByRole("button", { name: /仅删除此会话/ }));
    const dialog = screen.getByTestId("confirm-dialog");
    fireEvent.click(within(dialog).getByRole("button", { name: /删除会话/i }));
    await waitFor(() =>
      expect(deleteSpy).toHaveBeenCalledWith(
        expect.objectContaining({ includeProject: false }),
      ),
    );
    deleteSpy.mockRestore();
  });

  it("never offers project-directory deletion in the real confirmation dialog", async () => {
    const deleteSpy = vi
      .spyOn(sessionsApi, "delete")
      .mockResolvedValueOnce({ status: "deleted" });
    renderPage();
    await screen.findByRole("heading", { name: "Alpha Session" });
    fireEvent.click(screen.getByRole("button", { name: /仅删除此会话/i }));
    const dialog = screen.getByRole("dialog");
    expect(within(dialog).queryByRole("checkbox")).not.toBeInTheDocument();
    fireEvent.click(within(dialog).getByRole("button", { name: /删除会话/i }));
    await waitFor(() =>
      expect(deleteSpy).toHaveBeenCalledWith(
        expect.objectContaining({ includeProject: false }),
      ),
    );
    deleteSpy.mockRestore();
  });

  it("surfaces backend cleanup warnings as warnings instead of delete failures", async () => {
    const deleteSpy = vi.spyOn(sessionsApi, "delete").mockResolvedValueOnce({
      status: "deleted",
      warnings: ["会话已删除，但 Codex 索引清理失败：database is locked"],
    });

    renderPage();

    await waitFor(() =>
      expect(
        screen.getByRole("heading", { name: "Alpha Session" }),
      ).toBeInTheDocument(),
    );

    fireEvent.click(screen.getByRole("button", { name: /仅删除此会话/i }));
    const dialog = screen.getByTestId("confirm-dialog");
    fireEvent.click(within(dialog).getByRole("button", { name: /删除会话/i }));

    await waitFor(() =>
      expect(toastWarningMock).toHaveBeenCalledWith(
        "会话已删除，但 Codex 索引清理失败：database is locked",
      ),
    );
    expect(toastSuccessMock).toHaveBeenCalled();
    expect(toastErrorMock).not.toHaveBeenCalled();
    deleteSpy.mockRestore();
  });

  it("restores batch delete controls when deleteMany rejects", async () => {
    const deleteManySpy = vi
      .spyOn(sessionsApi, "deleteMany")
      .mockRejectedValueOnce(new Error("network error"));

    renderPage();

    await waitFor(() =>
      expect(
        screen.getByRole("heading", { name: "Alpha Session" }),
      ).toBeInTheDocument(),
    );

    fireEvent.click(screen.getByRole("button", { name: /批量管理/i }));
    fireEvent.click(screen.getByRole("button", { name: /全选筛选结果/i }));
    openBatchDeleteDialog();

    const dialog = screen.getByTestId("confirm-dialog");
    fireEvent.click(
      within(dialog).getByRole("button", { name: /删除所选会话/i }),
    );

    await waitFor(() =>
      expect(toastErrorMock).toHaveBeenCalledWith("network error"),
    );

    await waitFor(() => expect(batchDeleteButton()).not.toBeDisabled());

    deleteManySpy.mockRestore();
  });

  it("removes successfully deleted sessions from the UI before refetch completes", async () => {
    const view = renderPage();
    let resolveInvalidate!: () => void;
    const invalidateSpy = vi
      .spyOn(view.client, "invalidateQueries")
      .mockImplementation(
        () =>
          new Promise((resolve) => {
            resolveInvalidate = () => resolve(undefined);
          }),
      );

    await waitFor(() =>
      expect(
        screen.getByRole("heading", { name: "Alpha Session" }),
      ).toBeInTheDocument(),
    );

    fireEvent.click(screen.getByRole("button", { name: /批量管理/i }));
    fireEvent.click(screen.getByRole("button", { name: /全选筛选结果/i }));
    openBatchDeleteDialog();

    const dialog = screen.getByTestId("confirm-dialog");
    fireEvent.click(
      within(dialog).getByRole("button", { name: /删除所选会话/i }),
    );

    await waitFor(() => {
      expect(screen.queryByText("Alpha Session")).not.toBeInTheDocument();
      expect(screen.queryByText("Beta Session")).not.toBeInTheDocument();
    });

    await act(async () => {
      resolveInvalidate();
    });
    invalidateSpy.mockRestore();
  });

  it("shows the native-style project tree expanded by default", async () => {
    renderPage("all");

    await waitFor(() =>
      expect(
        screen.getByRole("heading", { name: "Claude Session" }),
      ).toBeInTheDocument(),
    );

    await switchToGroupedView();

    expect(
      screen.getByRole("button", {
        name: /展开或折叠 Codex 供应商分组/,
      }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", {
        name: /展开或折叠 Claude Code 供应商分组/,
      }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /展开或折叠 codex 目录分组/ }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /Alpha Session/ }),
    ).toBeInTheDocument();
  });

  it("persists manual directory expansion state", async () => {
    renderPage();

    await waitFor(() =>
      expect(
        screen.getByRole("heading", { name: "Alpha Session" }),
      ).toBeInTheDocument(),
    );

    await switchToGroupedView();
    const directoryToggle = screen.getByRole("button", {
      name: /展开或折叠 codex 目录分组/,
    });
    fireEvent.click(directoryToggle);
    fireEvent.click(directoryToggle);

    expect(
      screen.getByRole("button", { name: /展开或折叠 codex 目录分组/ }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /Alpha Session/ }),
    ).toBeInTheDocument();
    await waitFor(() =>
      expect(
        JSON.parse(window.localStorage.getItem(GROUP_EXPANSION_STORAGE_KEY)!)
          .expandedDirectoryKeys,
      ).toContain("codex:\\mock\\codex"),
    );

    fireEvent.click(directoryToggle);

    await waitFor(() =>
      expect(
        screen.queryByRole("button", { name: /Alpha Session/ }),
      ).not.toBeInTheDocument(),
    );
    await waitFor(() =>
      expect(
        JSON.parse(window.localStorage.getItem(GROUP_EXPANSION_STORAGE_KEY)!)
          .expandedDirectoryKeys,
      ).not.toContain("codex:\\mock\\codex"),
    );
  });

  it("keeps the matching native project tree visible when filtering providers", async () => {
    renderPage("all");

    await waitFor(() =>
      expect(
        screen.getByRole("combobox", { name: /Agent 筛选/i }),
      ).toBeInTheDocument(),
    );

    await switchToGroupedView();
    await switchProviderFilter(/Claude Code/i);

    await waitFor(() =>
      expect(screen.queryByText("Alpha Session")).not.toBeInTheDocument(),
    );

    expect(
      screen.getByRole("heading", { name: "Claude Session" }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", {
        name: /展开或折叠 Claude Code 供应商分组/,
      }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /展开或折叠 claude 目录分组/ }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /Claude Session/ }),
    ).toBeInTheDocument();
    expect(screen.queryByText("Gamma Session")).not.toBeInTheDocument();
  });

  it("supports batch deletion from grouped view", async () => {
    renderPage();

    await waitFor(() =>
      expect(
        screen.getByRole("heading", { name: "Alpha Session" }),
      ).toBeInTheDocument(),
    );

    await switchToGroupedView();
    fireEvent.click(screen.getByRole("button", { name: /批量管理/i }));
    fireEvent.click(screen.getByRole("button", { name: /全选筛选结果/i }));
    openBatchDeleteDialog();

    const dialog = screen.getByTestId("confirm-dialog");
    fireEvent.click(
      within(dialog).getByRole("button", { name: /删除所选会话/i }),
    );

    await waitFor(() => {
      expect(screen.queryByText("Alpha Session")).not.toBeInTheDocument();
      expect(screen.queryByText("Beta Session")).not.toBeInTheDocument();
      expect(screen.queryByText("Gamma Session")).not.toBeInTheDocument();
    });

    expect(toastErrorMock).not.toHaveBeenCalled();
    expect(toastSuccessMock).toHaveBeenCalled();
  });

  it("selects visible deletable sessions by provider group in grouped batch mode", async () => {
    renderPage("all");

    await waitFor(() =>
      expect(
        screen.getByRole("heading", { name: "Claude Session" }),
      ).toBeInTheDocument(),
    );

    await enterGroupedBatchMode();

    const codexProviderCheckbox = screen.getByRole("checkbox", {
      name: /选择 Codex 供应商分组内会话/,
    });
    const claudeProviderCheckbox = screen.getByRole("checkbox", {
      name: /选择 Claude Code 供应商分组内会话/,
    });

    fireEvent.click(codexProviderCheckbox);

    expect(codexProviderCheckbox).toBeChecked();
    expect(claudeProviderCheckbox).not.toBeChecked();
    expect(screen.getByText("已选 3 项")).toBeInTheDocument();

    fireEvent.click(codexProviderCheckbox);

    expect(codexProviderCheckbox).not.toBeChecked();
    expect(screen.getByText("已选 0 项")).toBeInTheDocument();
  });

  it("selects visible deletable sessions by directory group and marks the provider as mixed", async () => {
    renderPage();

    await waitFor(() =>
      expect(
        screen.getByRole("heading", { name: "Alpha Session" }),
      ).toBeInTheDocument(),
    );

    await enterGroupedBatchMode();
    expandDirectoryGroup("codex", "codex");

    const providerCheckbox = screen.getByRole("checkbox", {
      name: /选择 Codex 供应商分组内会话/,
    });
    const codexDirectoryCheckbox = screen.getByRole("checkbox", {
      name: /选择 codex 目录分组内会话/,
    });

    fireEvent.click(codexDirectoryCheckbox);

    expect(codexDirectoryCheckbox).toBeChecked();
    expect(providerCheckbox).toHaveAttribute("aria-checked", "mixed");
    expect(screen.getByText("已选 2 项")).toBeInTheDocument();
  });

  it("marks grouped batch checkboxes as mixed when only one session is selected", async () => {
    renderPage();

    await waitFor(() =>
      expect(
        screen.getByRole("heading", { name: "Alpha Session" }),
      ).toBeInTheDocument(),
    );

    await enterGroupedBatchMode();
    expandDirectoryGroup("codex", "codex");

    fireEvent.click(screen.getAllByRole("checkbox", { name: "选择会话" })[0]);

    expect(
      screen.getByRole("checkbox", {
        name: /选择 Codex 供应商分组内会话/,
      }),
    ).toHaveAttribute("aria-checked", "mixed");
    expect(
      screen.getByRole("checkbox", { name: /选择 codex 目录分组内会话/ }),
    ).toHaveAttribute("aria-checked", "mixed");
    expect(screen.getByText("已选 1 项")).toBeInTheDocument();
  });

  it("batch deletes only sessions selected from a grouped directory", async () => {
    renderPage();

    await waitFor(() =>
      expect(
        screen.getByRole("heading", { name: "Alpha Session" }),
      ).toBeInTheDocument(),
    );

    await enterGroupedBatchMode();
    expandDirectoryGroup("codex", "codex");
    fireEvent.click(
      screen.getByRole("checkbox", {
        name: /选择 codex 目录分组内会话/,
      }),
    );
    openBatchDeleteDialog();

    const dialog = screen.getByTestId("confirm-dialog");
    fireEvent.click(
      within(dialog).getByRole("button", { name: /删除所选会话/i }),
    );

    await waitFor(() => {
      expect(screen.queryByText("Alpha Session")).not.toBeInTheDocument();
      expect(screen.queryByText("Beta Session")).not.toBeInTheDocument();
    });

    expect(
      screen.getByRole("button", { name: /展开或折叠 未知目录 目录分组/ }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("checkbox", { name: "选择会话" }),
    ).toBeInTheDocument();
    expect(toastErrorMock).not.toHaveBeenCalled();
    expect(toastSuccessMock).toHaveBeenCalled();
  });

  it("shows message read failures rather than an empty conversation", async () => {
    const spy = vi
      .spyOn(sessionsApi, "streamMessages")
      .mockRejectedValue(new Error("database is locked"));
    renderPage();
    expect(
      await screen.findByText("会话读取失败，不能据此判断会话为空。"),
    ).toBeInTheDocument();
    expect(screen.getByText("database is locked")).toBeInTheDocument();
    expect(
      screen.queryByText("sessionManager.emptySession"),
    ).not.toBeInTheDocument();
    spy.mockRestore();
  });

  it("filters sessions by multiple keywords and restores results after clearing", async () => {
    renderPage();
    await screen.findByRole("heading", { name: "Alpha Session" });
    fireEvent.change(screen.getByRole("searchbox", { name: "搜索会话" }), {
      target: { value: "beta /mock/codex" },
    });
    expect(
      await screen.findByRole("heading", { name: "Beta Session" }),
    ).toBeInTheDocument();
    expect(screen.getByText("找到 1 条会话")).toBeInTheDocument();
    expect(
      screen.queryByRole("heading", { name: "Alpha Session" }),
    ).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "清空搜索" }));
    expect(screen.queryByText("找到 1 条会话")).not.toBeInTheDocument();
    expect(screen.getByRole("searchbox", { name: "搜索会话" })).toHaveValue("");
  });

  it("shows a recoverable search empty state", async () => {
    renderPage();
    await screen.findByRole("heading", { name: "Alpha Session" });
    fireEvent.change(screen.getByRole("searchbox", { name: "搜索会话" }), {
      target: { value: "no-such-session-xyz" },
    });
    expect(await screen.findByText("没有匹配的会话")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "清空搜索条件" }));
    expect(
      await screen.findByRole("heading", { name: "Alpha Session" }),
    ).toBeInTheDocument();
  });

  it("shows scan failures rather than no sessions", async () => {
    const spy = vi
      .spyOn(sessionsApi, "list")
      .mockRejectedValue(new Error("access denied"));
    renderPage();
    expect(await screen.findByRole("alert")).toHaveTextContent("access denied");
    expect(
      screen.queryByText("sessionManager.noSessions"),
    ).not.toBeInTheDocument();
    spy.mockRestore();
  });

  it("surfaces partial scan diagnostics alongside readable sessions", async () => {
    const original = sessionsApi.list;
    const spy = vi
      .spyOn(sessionsApi, "list")
      .mockImplementation(async (onWarnings) => {
        const sessions = await original();
        onWarnings?.(["zcode: database is locked"]);
        return sessions;
      });
    renderPage();
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "zcode: database is locked",
    );
    expect(
      await screen.findByRole("heading", { name: "Alpha Session" }),
    ).toBeInTheDocument();
    spy.mockRestore();
  });

  it("keeps a pending deletion visible and offers a retry", async () => {
    const target: SessionMeta = {
      providerId: "codex",
      sessionId: "pending-1",
      title: "Pending Session",
      sourcePath: "/mock/pending.jsonl",
      cleanupPending: true,
    };
    setSessionFixtures([target], {});
    const spy = vi.spyOn(sessionsApi, "delete").mockResolvedValue({
      status: "cleanup_pending",
      warnings: ["index locked"],
    });
    renderPage();
    fireEvent.click(await screen.findByRole("button", { name: "继续清理" }));
    fireEvent.click(
      within(screen.getByRole("dialog")).getByRole("button", {
        name: /删除会话/,
      }),
    );
    await waitFor(() =>
      expect(toastWarningMock).toHaveBeenCalledWith(
        "清理尚未完成，可在列表中继续清理",
        expect.objectContaining({ description: "index locked" }),
      ),
    );
    expect(
      await screen.findByRole("button", { name: "继续清理" }),
    ).toBeInTheDocument();
    expect(toastSuccessMock).not.toHaveBeenCalled();
    spy.mockRestore();
  });

  it("invalidates selected messages when rescanning unchanged session paths", async () => {
    const spy = vi
      .spyOn(sessionsApi, "streamMessages")
      .mockResolvedValue([{ role: "user", content: "before rescan" }]);
    renderPage();
    await waitFor(() => expect(spy).toHaveBeenCalled());
    const before = spy.mock.calls.length;
    spy.mockResolvedValue([{ role: "user", content: "after rescan" }]);
    fireEvent.click(screen.getByRole("button", { name: /重新扫描/ }));
    await waitFor(() => expect(spy.mock.calls.length).toBeGreaterThan(before));
    spy.mockRestore();
  });

  it("refreshes the list after a deletion request fails", async () => {
    const listSpy = vi.spyOn(sessionsApi, "list");
    const deleteSpy = vi
      .spyOn(sessionsApi, "delete")
      .mockRejectedValue(new Error("partial cleanup"));
    renderPage();
    await screen.findByRole("heading", { name: "Alpha Session" });
    const before = listSpy.mock.calls.length;
    fireEvent.click(screen.getByRole("button", { name: /仅删除此会话/ }));
    fireEvent.click(
      within(screen.getByRole("dialog")).getByRole("button", {
        name: /删除会话/,
      }),
    );
    await waitFor(() =>
      expect(listSpy.mock.calls.length).toBeGreaterThan(before),
    );
    deleteSpy.mockRestore();
    listSpy.mockRestore();
  });

  it("keeps the confirm dialog and selection when a single deletion fails", async () => {
    const deleteSpy = vi
      .spyOn(sessionsApi, "delete")
      .mockRejectedValue(new Error("Agent 仍在运行"));
    renderPage();
    await screen.findByRole("heading", { name: "Alpha Session" });

    fireEvent.click(screen.getByRole("button", { name: /仅删除此会话/ }));
    fireEvent.click(
      within(screen.getByRole("dialog")).getByRole("button", {
        name: /删除会话/,
      }),
    );

    // 失败后对话框必须留着：用户要能看出失败的是哪一条并直接重试
    await waitFor(() => expect(toastErrorMock).toHaveBeenCalled());
    expect(screen.getByTestId("confirm-dialog")).toBeInTheDocument();
    expect(
      within(screen.getByTestId("confirm-dialog")).getByText(/Alpha Session/),
    ).toBeInTheDocument();

    deleteSpy.mockRestore();
  });

  it("lists every batch deletion target in the confirm dialog", async () => {
    renderPage();

    await waitFor(() =>
      expect(
        screen.getByRole("heading", { name: "Alpha Session" }),
      ).toBeInTheDocument(),
    );

    await switchToGroupedView();
    fireEvent.click(screen.getByRole("button", { name: /批量管理/i }));
    fireEvent.click(screen.getByRole("button", { name: /全选筛选结果/i }));
    openBatchDeleteDialog();

    const details = within(
      screen.getByTestId("confirm-dialog-details"),
    ).getByRole("list");
    for (const title of ["Alpha Session", "Beta Session", "Gamma Session"]) {
      expect(within(details).getByText(title)).toBeInTheDocument();
    }
  });

  /**
   * 修复回归：曾经用「一次最多 100 条」硬拦批量删除，而列表每页正好 100 条，结果是
   * 选择范围一旦超过一页就完全无法删除——用户看到的正是「只能删掉显示的那些」。
   * 现在改为：删除照做，但清单列不全时必须勾选「已核对」才能确认。
   */
  it("deletes every selected session beyond one page once acknowledged", async () => {
    const manySessions: SessionMeta[] = Array.from(
      { length: 120 },
      (_, index) => ({
        providerId: "codex",
        sessionId: `bulk-${index}`,
        title: `Bulk Session ${index}`,
        sourcePath: `/mock/bulk/session-${index}.jsonl`,
        lastActiveAt: 1000 - index,
      }),
    );
    setSessionFixtures(manySessions, {});
    const deleteManySpy = vi.spyOn(sessionsApi, "deleteMany");
    renderPage();

    await waitFor(() =>
      expect(
        screen.getByRole("heading", { name: "Bulk Session 0" }),
      ).toBeInTheDocument(),
    );

    await switchToGroupedView();
    fireEvent.click(screen.getByRole("button", { name: /批量管理/i }));
    fireEvent.click(screen.getByRole("button", { name: /全选筛选结果/i }));
    expect(screen.getByText("已选 120 项")).toBeInTheDocument();
    openBatchDeleteDialog();

    const dialog = screen.getByTestId("confirm-dialog");
    // 清单列不全：必须先勾选「已核对」才能确认
    const confirmButton = within(dialog).getByRole("button", {
      name: /删除所选会话/i,
    });
    expect(confirmButton).toBeDisabled();

    fireEvent.click(within(dialog).getByRole("checkbox"));
    expect(confirmButton).toBeEnabled();
    fireEvent.click(confirmButton);

    await waitFor(() => expect(deleteManySpy).toHaveBeenCalled());
    // 关键：请求必须覆盖全部 120 条，而不是只覆盖当前渲染的一页
    expect(deleteManySpy.mock.calls[0][0]).toHaveLength(120);

    deleteManySpy.mockRestore();
  });

  it("needs no extra acknowledgement when every target is listed", async () => {
    renderPage();

    await waitFor(() =>
      expect(
        screen.getByRole("heading", { name: "Alpha Session" }),
      ).toBeInTheDocument(),
    );

    await switchToGroupedView();
    fireEvent.click(screen.getByRole("button", { name: /批量管理/i }));
    fireEvent.click(screen.getByRole("button", { name: /全选筛选结果/i }));
    openBatchDeleteDialog();

    const dialog = screen.getByTestId("confirm-dialog");
    // 能逐条列出（≤50 条）时不需要额外勾选，直接可确认
    expect(within(dialog).queryByRole("checkbox")).not.toBeInTheDocument();
    expect(
      within(dialog).getByRole("button", { name: /删除所选会话/i }),
    ).toBeEnabled();
  });

  it("does not expose a resume command action", async () => {
    setSessionFixtures(
      [
        {
          providerId: "codex",
          sessionId: "resume-1",
          title: "Resume Session",
          sourcePath: "/mock/resume.jsonl",
          resumeCommand: "codex resume resume-1",
        },
      ],
      {},
    );
    renderPage();
    await screen.findByRole("heading", { name: "Resume Session" });
    expect(
      screen.queryByRole("button", { name: "复制恢复命令" }),
    ).not.toBeInTheDocument();
  });

  /**
   * 缩略导航「压根不出来」的回归。
   *
   * 显示条件原先依赖 `scrollHeight / clientHeight` 的布局测量，同一个会话会随窗口
   * 高度、分栏宽度与测量时机时而出现、时而消失；后来换成正文字数门槛，很短的单轮
   * 会话又永远不出现。现在只要求「有用户提问」——这两条用例把两个方向都钉住：单轮
   * 长正文要出现，单轮短正文同样要出现。
   */
  it("shows the turn navigator for a long single-question session", async () => {
    setSessionFixtures(
      [
        {
          providerId: "codex",
          sessionId: "long-1",
          title: "Long Answer",
          sourcePath: "/mock/long.jsonl",
          lastActiveAt: 10,
        },
      ],
      {
        "codex:/mock/long.jsonl": [
          { role: "user", content: "只有一个问题", ts: 10 },
          { role: "assistant", content: "长回答".repeat(1200), ts: 20 },
        ],
      },
    );
    renderPage();

    await screen.findByRole("heading", { name: "Long Answer" });
    // 导航条依赖正文，等它真正渲染出来再断言
    expect(
      await screen.findByRole("button", { name: /跳转到第 1 个用户问题/ }),
    ).toBeInTheDocument();
  });

  it("shows the turn navigator for a short single-question session too", async () => {
    renderPage();

    await screen.findByRole("heading", { name: "Alpha Session" });
    // 一屏能看完的会话同样要有导航条：用户不知道背后有没有长度门槛，
    // 看不到就只会认为这个功能坏了
    expect(
      await screen.findByRole("button", { name: /跳转到第 1 个用户问题/ }),
    ).toBeInTheDocument();
  });

  it("labels archived sessions", async () => {
    setSessionFixtures(
      [
        {
          providerId: "codex",
          sessionId: "archive-1",
          title: "Archived Session",
          sourcePath: "/mock/archive.jsonl",
          archived: true,
          resumeCommand: "codex resume archive-1",
        },
      ],
      {},
    );
    renderPage();
    await screen.findByRole("heading", { name: "Archived Session" });
    expect(screen.getAllByText("已归档").length).toBeGreaterThan(0);
  });

  /**
   * DeepSeek Harness 会把子 Agent 会话生成成独立会话。它们在列表里只显示为「残留」，
   * 用户既认不出归属、也没法先逐个删干净——父会话因此永远删不掉。
   *
   * 这里覆盖两件事：列表里说清归属；删除父会话时一次性带上整条子会话链。
   */
  describe("subagent lineage", () => {
    const lineage = (): SessionMeta[] => [
      {
        providerId: "deepseek",
        sessionId: "ds-parent",
        title: "Parent Chat",
        projectDir: "/mock/deepseek",
        lastActiveAt: 50,
        sourcePath: "/mock/deepseek/parent/session.v3.jsonl",
      },
      {
        providerId: "deepseek",
        sessionId: "ds-child-a",
        title: "Sub Task A",
        residual: true,
        parentSessionId: "ds-parent",
        projectDir: "/mock/deepseek",
        lastActiveAt: 40,
        sourcePath: "/mock/deepseek/child-a/session.v3.jsonl",
      },
      {
        providerId: "deepseek",
        sessionId: "ds-child-b",
        title: "Sub Task B",
        residual: true,
        parentSessionId: "ds-parent",
        projectDir: "/mock/deepseek",
        lastActiveAt: 30,
        sourcePath: "/mock/deepseek/child-b/session.v3.jsonl",
      },
      {
        providerId: "deepseek",
        sessionId: "ds-grandchild",
        title: "Nested Helper",
        residual: true,
        parentSessionId: "ds-child-a",
        projectDir: "/mock/deepseek",
        lastActiveAt: 20,
        sourcePath: "/mock/deepseek/grandchild/session.v3.jsonl",
      },
    ];

    beforeEach(() => {
      setSessionFixtures(lineage(), {
        "deepseek:/mock/deepseek/parent/session.v3.jsonl": [
          { role: "user", content: "parent", ts: 50 },
        ],
      });
    });

    const renderLineage = async () => {
      renderPage("deepseek");
      await screen.findByRole("heading", { name: "Parent Chat" });
    };

    it("names the parent of a subagent session instead of only marking it residual", async () => {
      await renderLineage();

      // 两条子会话属于父会话；孙会话属于子会话，各自显示各自的上层。
      expect(screen.getAllByText("「Parent Chat」的子会话")).toHaveLength(2);
      expect(screen.getByText("「Sub Task A」的子会话")).toBeInTheDocument();
      // 归属比「残留」有信息量，两者不同时堆在行里
      expect(screen.queryByText("残留")).not.toBeInTheDocument();
    });

    it("explains the lineage and the one-click cleanup on hover", async () => {
      await renderLineage();

      await userEvent.hover(screen.getAllByText("「Parent Chat」的子会话")[0]);

      const tooltip = await screen.findByRole("tooltip");
      expect(tooltip).toHaveTextContent("「Parent Chat」的子 Agent 会话");
      expect(tooltip).toHaveTextContent("同时删除子会话");
    });

    it("deletes the parent together with every descendant in one request", async () => {
      const deleteManySpy = vi.spyOn(sessionsApi, "deleteMany");
      const deleteSpy = vi.spyOn(sessionsApi, "delete");
      await renderLineage();

      // 按钮自己就把实际范围说清楚，不再只是「仅删除此会话」
      fireEvent.click(
        screen.getByRole("button", { name: /删除此会话及其 3 个子会话/i }),
      );

      const dialog = screen.getByTestId("confirm-dialog");
      expect(within(dialog).getByText(/及其 3 个子会话/)).toBeInTheDocument();
      // 逐条列出实际删除范围，并标出每条子会话的归属（含隔代的孙会话）
      expect(
        within(dialog).getAllByText(/子会话 · 属于「Parent Chat」/),
      ).toHaveLength(2);
      expect(
        within(dialog).getAllByText(/子会话 · 属于「Sub Task A」/),
      ).toHaveLength(1);
      expect(within(dialog).getByText("Nested Helper")).toBeInTheDocument();

      const cascade = within(dialog).getByRole("checkbox", {
        name: /同时删除「Parent Chat」的 3 个子会话/,
      });
      expect(cascade).toBeChecked();

      // 单条父会话带子会话时，确认按钮自己就说明范围
      fireEvent.click(
        within(dialog).getByRole("button", { name: /删除会话及其子会话/i }),
      );

      await waitFor(() => expect(deleteManySpy).toHaveBeenCalled());
      expect(
        deleteManySpy.mock.calls[0][0].map((item) => item.sessionId).sort(),
      ).toEqual(["ds-child-a", "ds-child-b", "ds-grandchild", "ds-parent"]);
      // 不能再退化成「只删父会话、留下孤儿子会话」
      expect(deleteSpy).not.toHaveBeenCalled();

      deleteManySpy.mockRestore();
      deleteSpy.mockRestore();
    });

    it("can still delete only the parent when the cascade box is cleared", async () => {
      const deleteManySpy = vi.spyOn(sessionsApi, "deleteMany");
      const deleteSpy = vi.spyOn(sessionsApi, "delete");
      await renderLineage();

      fireEvent.click(
        screen.getByRole("button", { name: /删除此会话及其 3 个子会话/i }),
      );
      const dialog = screen.getByTestId("confirm-dialog");
      fireEvent.click(
        within(dialog).getByRole("checkbox", {
          name: /同时删除「Parent Chat」的 3 个子会话/,
        }),
      );

      expect(
        within(dialog).getByRole("checkbox", {
          name: /同时删除「Parent Chat」的 3 个子会话/,
        }),
      ).not.toBeChecked();
      // 范围缩回一条后，确认框自己也退回单条语义
      expect(
        within(dialog).getByRole("button", { name: /^删除会话$/ }),
      ).toBeInTheDocument();

      fireEvent.click(
        within(dialog).getByRole("button", { name: /^删除会话$/ }),
      );

      await waitFor(() => expect(deleteSpy).toHaveBeenCalled());
      expect(deleteSpy.mock.calls[0][0]).toMatchObject({
        sessionId: "ds-parent",
        sourcePath: "/mock/deepseek/parent/session.v3.jsonl",
      });
      expect(deleteManySpy).not.toHaveBeenCalled();

      deleteManySpy.mockRestore();
      deleteSpy.mockRestore();
    });

    it("offers no cascade when the selected session has no children", async () => {
      await renderLineage();

      fireEvent.click(screen.getByText("Nested Helper"));

      const [deleteButton] = screen.getAllByRole("button", {
        name: /仅删除此会话/i,
      });
      fireEvent.click(deleteButton);

      const dialog = screen.getByTestId("confirm-dialog");
      expect(within(dialog).queryByRole("checkbox")).not.toBeInTheDocument();
      expect(within(dialog).getByText(/Nested Helper/)).toBeInTheDocument();
    });

    it("adds the children of every selected root when several parents are picked", async () => {
      const deleteManySpy = vi.spyOn(sessionsApi, "deleteMany");
      await renderLineage();

      await switchToGroupedView();
      fireEvent.click(screen.getByRole("button", { name: /批量管理/i }));

      // 显式勾选父会话与孙会话两条：子会话 A/B 只作为「父会话的后代」被带上
      const checkRow = (title: string) => {
        // 详情面板的标题与列表行可能同名，只取列表行里的那一份
        const rows = screen
          .getAllByText(title)
          .map((node) => node.closest(".asm-session-item"))
          .filter((node): node is HTMLElement => node !== null);
        expect(rows).toHaveLength(1);
        fireEvent.click(
          within(rows[0]).getByRole("checkbox", { name: "选择会话" }),
        );
      };
      checkRow("Parent Chat");
      checkRow("Nested Helper");

      openBatchDeleteDialog(2);
      const dialog = screen.getByTestId("confirm-dialog");
      expect(
        within(dialog).getByText(/其中 2 条是随父会话一并删除的子会话/),
      ).toBeInTheDocument();
      const cascade = within(dialog).getByRole("checkbox", {
        name: /同时删除这 2 条会话下的 2 个子会话/,
      });
      expect(cascade).toBeChecked();

      fireEvent.click(
        within(dialog).getByRole("button", { name: /删除所选会话/i }),
      );

      await waitFor(() => expect(deleteManySpy).toHaveBeenCalled());
      expect(
        deleteManySpy.mock.calls[0][0].map((item) => item.sessionId).sort(),
      ).toEqual(["ds-child-a", "ds-child-b", "ds-grandchild", "ds-parent"]);

      deleteManySpy.mockRestore();
    });

    it("skips the cascade prompt when the whole tree is already selected", async () => {
      const deleteManySpy = vi.spyOn(sessionsApi, "deleteMany");
      await renderLineage();

      await switchToGroupedView();
      fireEvent.click(screen.getByRole("button", { name: /批量管理/i }));
      fireEvent.click(screen.getByRole("button", { name: /全选筛选结果/i }));

      openBatchDeleteDialog(4);
      const dialog = screen.getByTestId("confirm-dialog");
      // 父子都在选择集里：没有额外的后代可补，勾选项不该出现
      expect(within(dialog).queryByRole("checkbox")).not.toBeInTheDocument();
      expect(
        within(dialog).getByText(/已选中的 4 个本地会话记录/),
      ).toBeInTheDocument();

      fireEvent.click(
        within(dialog).getByRole("button", { name: /删除所选会话/i }),
      );

      await waitFor(() => expect(deleteManySpy).toHaveBeenCalled());
      expect(deleteManySpy.mock.calls[0][0]).toHaveLength(4);

      deleteManySpy.mockRestore();
    });
  });
});
