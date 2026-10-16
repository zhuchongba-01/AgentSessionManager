import type { SessionMessage, SessionMeta } from "@/types";

const CODEX_IDE_CONTEXT_PREFIX = "# Context from my IDE setup:";
const CODEX_REQUEST_MARKER = "my request for codex";
export const UNKNOWN_PROJECT_DIR_KEY = "__unknown_project_dir__";

export interface SessionDirectoryGroup {
  key: string;
  projectDir: string | null;
  projectName: string | null;
  label: string;
  sessions: SessionMeta[];
}

export interface SessionProviderGroup {
  providerId: string;
  sessions: SessionMeta[];
  directories: SessionDirectoryGroup[];
}

const getCodexRequestHeadingPayload = (lineText: string) => {
  if (!lineText.startsWith("#")) return null;

  const heading = lineText.replace(/^#+\s*/, "");
  const suffix = heading.toLowerCase().startsWith(CODEX_REQUEST_MARKER)
    ? heading.slice(CODEX_REQUEST_MARKER.length).trimStart()
    : null;

  if (suffix === null) return null;
  if (!suffix) return "";
  if (!/^[:：\-—]/.test(suffix)) return null;

  return suffix.replace(/^[:：\-—\s]+/, "").trim();
};

const extractCodexPromptFromIdeContext = (content: string) => {
  const trimmed = content.trim();
  if (!trimmed.startsWith(CODEX_IDE_CONTEXT_PREFIX)) {
    return null;
  }

  // VS Code injects the real prompt as the LAST "## My request for Codex:"
  // section, so keep the final matching heading. Earlier matches can be
  // headings that live inside the active selection / open file content.
  // Trade-off: if the request body itself repeats the heading, the preview
  // truncates to its trailing part (rare; see sessionUtils.test.ts).
  const lines = trimmed.replace(/\r\n/g, "\n").split("\n");
  let prompt: string | null = null;
  for (const [index, line] of lines.entries()) {
    const inlinePrompt = getCodexRequestHeadingPayload(line.trim());
    if (inlinePrompt === null) continue;

    if (inlinePrompt) {
      prompt = inlinePrompt;
      continue;
    }

    const followingPrompt = lines
      .slice(index + 1)
      .join("\n")
      .trim();
    prompt = followingPrompt || null;
  }

  return prompt;
};

export const getSessionKey = (session: SessionMeta) =>
  `${session.providerId}:${session.sessionId}:${session.sourcePath ?? ""}`;

export const getSessionDirectoryGroupKey = (
  providerId: string,
  projectDir?: string | null,
  projectName?: string | null,
  sidebarSection?: SessionMeta["sidebarSection"],
) => {
  if (sidebarSection) return `${providerId}:section:${sidebarSection.id}`;
  const trimmed = projectDir?.trim();
  const name = projectName?.trim();
  const identity = trimmed
    ? trimmed
        .replace(/[/\\]+/g, "\\")
        .replace(/\\+$/, "")
        .toLocaleLowerCase()
    : name?.toLocaleLowerCase();
  return `${providerId}:${identity || UNKNOWN_PROJECT_DIR_KEY}`;
};

export const getBaseName = (value?: string | null) => {
  if (!value) return "";
  const trimmed = value.trim();
  if (!trimmed) return "";
  const normalized = trimmed.replace(/[\\/]+$/, "");
  const parts = normalized.split(/[\\/]/).filter(Boolean);
  return parts[parts.length - 1] || trimmed;
};

export const formatTimestamp = (value?: number) => {
  if (!value) return "";
  return new Date(value).toLocaleString();
};

// 消息时间戳：当天的只显示时刻（如 "14:02"），更早的才带日期，
// 避免对话流里每条消息都重复一长串日期。
export const formatMessageTimestamp = (value?: number) => {
  if (!value) return "";
  const date = new Date(value);
  const now = new Date();
  const sameDay =
    date.getFullYear() === now.getFullYear() &&
    date.getMonth() === now.getMonth() &&
    date.getDate() === now.getDate();
  if (sameDay) {
    return date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  }
  return date.toLocaleString();
};

export const formatRelativeTime = (
  value: number | undefined,
  t: (key: string, options?: Record<string, unknown>) => string,
) => {
  if (!value) return "";
  const now = Date.now();
  const diff = now - value;
  const minutes = Math.floor(diff / 60000);
  const hours = Math.floor(diff / 3600000);
  const days = Math.floor(diff / 86400000);

  if (minutes < 1) return t("sessionManager.justNow");
  if (minutes < 60) return t("sessionManager.minutesAgo", { count: minutes });
  if (hours < 24) return t("sessionManager.hoursAgo", { count: hours });
  if (days < 7) return t("sessionManager.daysAgo", { count: days });
  return new Date(value).toLocaleDateString();
};

export const getProviderLabel = (
  providerId: string,
  t: (key: string) => string,
) => {
  const normalized = providerId.toLocaleLowerCase();
  const labels: Record<string, string> = {
    codex: "Codex",
    claude: "Claude Code",
    opencode: "OpenCode",
    zcode: "ZCode",
    grokbuild: "Grok Build",
    pi: "Pi",
    deepseek: "DeepSeek Harness",
  };
  const key = `apps.${normalized}`;
  const translated = t(key);
  return translated === key ? (labels[normalized] ?? providerId) : translated;
};

// 根据 providerId 获取对应的图标名称
export const getProviderIconName = (providerId: string) => {
  const normalized = providerId.toLocaleLowerCase();
  if (normalized === "codex") return "openai";
  if (normalized === "grokbuild") return "grok";
  return normalized;
};

export const getRoleTone = (role: string) => {
  const normalized = role.toLowerCase();
  if (normalized === "user") return "text-[hsl(112_30%_34%)] dark:text-primary";
  if (normalized === "assistant") return "text-foreground";
  return "text-muted-foreground";
};

export const getRoleLabel = (role: string, t: (key: string) => string) => {
  const normalized = role.toLowerCase();
  if (normalized === "assistant") return "AI";
  if (normalized === "user") return t("sessionManager.roleUser");
  if (normalized === "system") return t("sessionManager.roleSystem");
  if (normalized === "tool") return t("sessionManager.roleTool");
  return role;
};

export const formatSessionTitle = (session: SessionMeta) => {
  return (
    session.title ||
    session.projectName ||
    getBaseName(session.projectDir) ||
    session.sessionId.slice(0, 8)
  );
};

/**
 * `providerId:sessionId` → 会话显示名。子会话的归属展示需要父会话的名字，而父会话
 * 行可能被搜索过滤掉、或压根不在当前列表页里，所以按原生 ID 建一张表来查。
 */
export const buildSessionTitleIndex = (
  sessions: SessionMeta[],
): Map<string, string> => {
  const index = new Map<string, string>();
  sessions.forEach((session) => {
    index.set(
      `${session.providerId}:${session.sessionId}`,
      formatSessionTitle(session),
    );
  });
  return index;
};

export const getSessionNativeKey = (providerId: string, sessionId: string) =>
  `${providerId}:${sessionId}`;

/**
 * 父会话的显示名。父会话不在扫描结果里时退回原生 ID 前 8 位——宁可显示 ID，
 * 也不能退回「残留」这种看不出归属的标签。
 */
export const resolveParentLabel = (
  session: SessionMeta,
  titles: Map<string, string>,
): string | undefined => {
  const parentId = session.parentSessionId;
  if (!parentId) return undefined;
  return (
    titles.get(getSessionNativeKey(session.providerId, parentId)) ??
    parentId.slice(0, 8)
  );
};

/**
 * 收集这些会话的全部后代（子 Agent 会话、子会话的子会话……）。
 *
 * 删除父会话时必须先删子会话，逐个手动删既繁琐又容易漏；这个集合就是「一并删除」
 * 的范围。传进来的 `roots` 本身不出现在结果里（它们已经是删除目标），但仍然参与
 * 遍历——批量删除同时勾选了父子两行时，子会话自己的子会话也必须带上。
 *
 * `sessions` 必须是**完整扫描结果**，不是过滤后的列表：搜索/筛选把某条子会话藏起来
 * 并不代表它不存在，漏掉它只会让父会话的预检失败。
 */
export const collectSessionDescendants = (
  sessions: SessionMeta[],
  roots: SessionMeta[],
): SessionMeta[] => {
  if (roots.length === 0) return [];

  const childrenByParent = new Map<string, SessionMeta[]>();
  sessions.forEach((session) => {
    const parentId = session.parentSessionId;
    if (!parentId) return;
    const key = getSessionNativeKey(session.providerId, parentId);
    const siblings = childrenByParent.get(key);
    if (siblings) {
      siblings.push(session);
    } else {
      childrenByParent.set(key, [session]);
    }
  });

  const rootKeys = new Set(roots.map(getSessionKey));
  const visited = new Set<string>();
  const queue: string[] = [];
  roots.forEach((session) => {
    const key = getSessionNativeKey(session.providerId, session.sessionId);
    // 同一个原生会话可能有多条同 ID 历史副本，visited 只记原生 ID，
    // 避免副本互相把对方当成对方的祖先而无限展开。
    if (visited.has(key)) return;
    visited.add(key);
    queue.push(key);
  });

  const descendants: SessionMeta[] = [];
  for (let index = 0; index < queue.length; index += 1) {
    const children = childrenByParent.get(queue[index]);
    if (!children) continue;
    children.forEach((child) => {
      const childNativeKey = getSessionNativeKey(
        child.providerId,
        child.sessionId,
      );
      if (visited.has(childNativeKey)) return;
      visited.add(childNativeKey);
      queue.push(childNativeKey);
      // 显式勾选的会话已经在删除目标里，不再重复列出。
      if (!rootKeys.has(getSessionKey(child))) descendants.push(child);
    });
  }
  return descendants;
};

export const groupSessionsByProviderAndDirectory = (
  sessions: SessionMeta[],
  unknownDirectoryLabel: string,
  ungroupedDirectoryLabel = unknownDirectoryLabel,
): SessionProviderGroup[] => {
  const providerGroups: SessionProviderGroup[] = [];
  const providerGroupMap = new Map<string, SessionProviderGroup>();
  const directoryGroupMaps = new Map<
    string,
    Map<string, SessionDirectoryGroup>
  >();

  sessions.forEach((session) => {
    let providerGroup = providerGroupMap.get(session.providerId);
    if (!providerGroup) {
      providerGroup = {
        providerId: session.providerId,
        sessions: [],
        directories: [],
      };
      providerGroupMap.set(session.providerId, providerGroup);
      providerGroups.push(providerGroup);
      directoryGroupMaps.set(session.providerId, new Map());
    }

    providerGroup.sessions.push(session);

    const trimmedProjectDir = session.projectDir?.trim() || null;
    const trimmedProjectName = session.projectName?.trim() || null;
    const directoryKey = getSessionDirectoryGroupKey(
      session.providerId,
      trimmedProjectDir,
      trimmedProjectName,
      session.sidebarSection,
    );
    const directoryGroups = directoryGroupMaps.get(session.providerId)!;

    let directoryGroup = directoryGroups.get(directoryKey);
    if (!directoryGroup) {
      directoryGroup = {
        key: directoryKey,
        projectDir: session.sidebarSection ? null : trimmedProjectDir,
        projectName: trimmedProjectName,
        label:
          session.sidebarSection?.name ||
          trimmedProjectName ||
          (trimmedProjectDir
            ? getBaseName(trimmedProjectDir) || trimmedProjectDir
            : session.providerId === "deepseek"
              ? ungroupedDirectoryLabel
              : unknownDirectoryLabel),
        sessions: [],
      };
      directoryGroups.set(directoryKey, directoryGroup);
      providerGroup.directories.push(directoryGroup);
    }

    directoryGroup.sessions.push(session);
  });

  return providerGroups;
};

export const shouldHideCodexMessageFromToc = (content: string) => {
  const trimmed = content.trim();
  return (
    trimmed.startsWith("# AGENTS.md instructions for ") ||
    trimmed.startsWith("<environment_context>") ||
    (trimmed.startsWith(CODEX_IDE_CONTEXT_PREFIX) &&
      !extractCodexPromptFromIdeContext(trimmed))
  );
};

export const getVisibleSessionMessages = (
  messages: SessionMessage[],
  isCodexSession: boolean,
) => {
  if (!isCodexSession) return messages;

  return messages.filter((message) => {
    const role = message.role.toLowerCase();
    if (role === "developer" || role === "system") return false;
    return !(role === "user" && shouldHideCodexMessageFromToc(message.content));
  });
};

export const extractCodexPromptPreview = (content: string) => {
  return extractCodexPromptFromIdeContext(content) ?? content;
};

export interface UserMessageTocEntry {
  index: number;
  preview: string;
  ts?: number;
}

/**
 * 消息目录 / 缩略导航的条目。
 *
 * **`index` 是「可见消息」数组里的下标，不是正文里的绝对下标。**
 * 传给虚拟列表、`scrollToIndex` 与 `SessionMinimap` 的都必须是同一个空间：
 * 虚拟列表的 count 就是 `getVisibleSessionMessages` 的结果长度。这一点容易被
 * 误解成 wire 里的 `SessionMessage.index`（那是取全文用的绝对下标，只在
 * `get_message_content` 中使用）；两者混用会让点击缩略导航跳到错误的消息上。
 */
export const buildUserMessageToc = (
  visibleMessages: SessionMessage[],
  isCodexSession: boolean,
): UserMessageTocEntry[] => {
  const entries: UserMessageTocEntry[] = [];
  visibleMessages.forEach((message, index) => {
    if (message.role.toLowerCase() !== "user") return;
    const previewContent = isCodexSession
      ? extractCodexPromptPreview(message.content)
      : message.content;
    entries.push({
      index,
      preview: formatSessionMessagePreview(previewContent),
      ts: message.ts,
    });
  });
  return entries;
};

export const formatSessionMessagePreview = (
  content: string,
  maxLength = 50,
) => {
  return (
    content.slice(0, maxLength) + (content.length > maxLength ? "..." : "")
  );
};

/**
 * 缩略导航的显示条件：**只要有用户提问就显示**。
 *
 * 这条规则被改过两次，都是因为「什么才算值得导航」一旦交给别的判据，用户看到的就是
 * 「有时候有、有时候没有」：
 *
 * 1. 最早按「正文是否超过两屏」的像素高度判断。那个数字来自一次布局测量，窗口高度、
 *    分栏宽度、滚动条、测量时机都会影响它，同一个会话时而出现、时而消失。
 * 2. 改成按正文字符数（单轮 ≥ 3000 字）判断，确定性有了，但很短的单轮会话仍然永远
 *    不出现——用户看到的就是「压根不出来」，而他自己并不知道背后有个长度门槛。
 *
 * 现在只剩「有没有提问」这一个判据：没有提问就没有刻度，有提问就一定给导航。少一条
 * 判据，就少一种「该出现却没出现」。
 */
export const shouldShowSessionMinimap = (userTurnCount: number) =>
  userTurnCount > 0;
