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
) => {
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
  const key = `apps.${providerId}`;
  const translated = t(key);
  return translated === key ? providerId : translated;
};

// 根据 providerId 获取对应的图标名称
export const getProviderIconName = (providerId: string) => {
  if (providerId === "codex") return "openai";
  if (providerId === "grokbuild") return "grok";
  return providerId;
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

export const groupSessionsByProviderAndDirectory = (
  sessions: SessionMeta[],
  unknownDirectoryLabel: string,
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
    );
    const directoryGroups = directoryGroupMaps.get(session.providerId)!;

    let directoryGroup = directoryGroups.get(directoryKey);
    if (!directoryGroup) {
      directoryGroup = {
        key: directoryKey,
        projectDir: trimmedProjectDir,
        projectName: trimmedProjectName,
        label:
          trimmedProjectName ||
          (trimmedProjectDir
            ? getBaseName(trimmedProjectDir) || trimmedProjectDir
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

export const formatSessionMessagePreview = (
  content: string,
  maxLength = 50,
) => {
  return (
    content.slice(0, maxLength) + (content.length > maxLength ? "..." : "")
  );
};
