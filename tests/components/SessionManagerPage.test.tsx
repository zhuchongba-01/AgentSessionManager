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

const openViewModeMenu = async () => {
  await userEvent.click(screen.getByRole("combobox", { name: /查看方式/i }));
};

const switchToGroupedView = async () => {
  await openViewModeMenu();
  const groupedOption = await screen.findByRole("option", { name: /分类/i });
  await userEvent.click(groupedOption);
  await waitFor(() =>
    expect(
      screen.queryByRole("option", { name: /分类/i }),
    ).not.toBeInTheDocument(),
  );
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
    server.use(
      http.get(GITHUB_LATEST_RELEASE_API, () =>
        HttpResponse.json({
          tag_name: "v1.3.10",
          draft: false,
          prerelease: false,
        }),
      ),
    );
    renderPage();

    const updateButton = await screen.findByRole("button", {
      name: "发现新版本 v1.3.10",
    });
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

    fireEvent.click(screen.getByRole("button", { name: /删除会话/i }));

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

  it("never offers project-directory deletion in the real confirmation dialog", async () => {
    const deleteSpy = vi
      .spyOn(sessionsApi, "delete")
      .mockResolvedValueOnce({ status: "deleted" });
    renderPage();
    await screen.findByRole("heading", { name: "Alpha Session" });
    fireEvent.click(screen.getByRole("button", { name: /删除会话/i }));
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

    fireEvent.click(screen.getByRole("button", { name: /删除会话/i }));
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
    fireEvent.click(screen.getByRole("button", { name: /批量删除/i }));

    const dialog = screen.getByTestId("confirm-dialog");
    fireEvent.click(
      within(dialog).getByRole("button", { name: /删除所选会话/i }),
    );

    await waitFor(() =>
      expect(toastErrorMock).toHaveBeenCalledWith("network error"),
    );

    await waitFor(() =>
      expect(
        screen.getByRole("button", { name: /批量删除/i }),
      ).not.toBeDisabled(),
    );

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
    fireEvent.click(screen.getByRole("button", { name: /批量删除/i }));

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
    fireEvent.click(screen.getByRole("button", { name: /批量删除/i }));

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
    fireEvent.click(screen.getByRole("button", { name: /批量删除/i }));

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
      .spyOn(sessionsApi, "getMessages")
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
      .spyOn(sessionsApi, "getMessages")
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
    fireEvent.click(screen.getByRole("button", { name: /删除会话/ }));
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

  it("copies a backend-validated resume command for a current session", async () => {
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
    const spy = vi
      .spyOn(sessionsApi, "resume")
      .mockResolvedValue("codex resume resume-1");
    const copy = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { writeText: copy },
    });
    renderPage();
    fireEvent.click(
      await screen.findByRole("button", { name: "复制恢复命令" }),
    );
    await waitFor(() =>
      expect(spy).toHaveBeenCalledWith("codex", "/mock/resume.jsonl", false),
    );
    expect(copy).toHaveBeenCalledWith("codex resume resume-1");
    spy.mockRestore();
  });

  it("labels archived sessions and disables direct resume", async () => {
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
    expect(screen.getByRole("button", { name: "复制恢复命令" })).toBeDisabled();
  });
});
