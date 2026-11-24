export type ToolActionKind =
  | "shell"
  | "read"
  | "search"
  | "edit"
  | "write"
  | "web"
  | "mcp"
  | "agent"
  | "todo"
  | "other";

export interface AgentReaderStyle {
  id: string;
  /** HSL 或 CSS 颜色，用于提问气泡浅底色、目录当前轮指示条与步骤强调 */
  accentColor: string;
  assistantGlyph: string;
  thinkingGlyph: string;
  stepGlyph: {
    success: string;
    error: string;
  };
  resultConnector: string;
  titleFormat: "call" | "verb" | "icon";
  monoTitles: boolean;
  verbs: Partial<Record<ToolActionKind, string>>;
}

const CLAUDE_STYLE: AgentReaderStyle = {
  id: "claude",
  accentColor: "#d97757",
  assistantGlyph: "⏺",
  thinkingGlyph: "✻",
  stepGlyph: { success: "⏺", error: "⏺" },
  resultConnector: "⎿",
  titleFormat: "call",
  monoTitles: true,
  verbs: {
    edit: "Update",
  },
};

const CODEX_STYLE: AgentReaderStyle = {
  id: "codex",
  accentColor: "#10a37f",
  assistantGlyph: "•",
  thinkingGlyph: "•",
  stepGlyph: { success: "•", error: "•" },
  resultConnector: "└",
  titleFormat: "verb",
  monoTitles: false,
  verbs: {
    shell: "Ran",
    read: "Read",
    search: "Search",
    edit: "Edited",
    write: "Added",
    web: "Searched",
    mcp: "Called",
    agent: "Spawned agent",
    todo: "Updated plan",
  },
};

const GEMINI_STYLE: AgentReaderStyle = {
  id: "gemini",
  accentColor: "#3186ff",
  assistantGlyph: "✦",
  thinkingGlyph: "✦",
  stepGlyph: { success: "✓", error: "✗" },
  resultConnector: "│",
  titleFormat: "verb",
  monoTitles: true,
  verbs: {
    shell: "Shell",
    read: "ReadFile",
    search: "Search",
    edit: "Edit",
    write: "WriteFile",
    web: "WebFetch",
    mcp: "MCP",
    agent: "Agent",
    todo: "Todos",
  },
};

const OPENCODE_STYLE: AgentReaderStyle = {
  id: "opencode",
  accentColor: "#6366f1",
  assistantGlyph: "",
  thinkingGlyph: "◇",
  stepGlyph: { success: "✓", error: "✗" },
  resultConnector: "│",
  titleFormat: "icon",
  monoTitles: true,
  verbs: {
    shell: "$",
    read: "→ Read",
    search: "✱ Grep",
    edit: "← Edit",
    write: "← Wrote",
    web: "% WebFetch",
    mcp: "⚙ MCP",
    agent: "│ Task",
    todo: "⚙ Todos",
  },
};

const PROVIDER_ACCENTS: Record<string, Partial<AgentReaderStyle>> = {
  copilot: {
    id: "copilot",
    accentColor: "#8b5cf6",
    assistantGlyph: "●",
    thinkingGlyph: "◈",
    resultConnector: "└",
    titleFormat: "call",
  },
  deepseek: {
    id: "deepseek",
    accentColor: "#4d6bfe",
    assistantGlyph: "⏺",
    thinkingGlyph: "✻",
    resultConnector: "⎿",
    titleFormat: "call",
  },
  workbuddy: {
    id: "workbuddy",
    accentColor: "#0ac49a",
    assistantGlyph: "⏺",
    thinkingGlyph: "✻",
    resultConnector: "⎿",
    titleFormat: "call",
  },
  codebuddy: {
    id: "codebuddy",
    accentColor: "#6c4dff",
    assistantGlyph: "⏺",
    thinkingGlyph: "✻",
    resultConnector: "⎿",
    titleFormat: "call",
  },
  kimi: {
    id: "kimi",
    accentColor: "#2b8fff",
    assistantGlyph: "•",
    thinkingGlyph: "✦",
    resultConnector: "└",
    titleFormat: "call",
  },
  openclaw: {
    id: "openclaw",
    accentColor: "#ef4444",
    assistantGlyph: "●",
    thinkingGlyph: "•",
    resultConnector: "└",
    titleFormat: "verb",
  },
  grokbuild: {
    id: "grokbuild",
    accentColor: "#f59e0b",
    assistantGlyph: "•",
    thinkingGlyph: "✦",
    resultConnector: "└",
    titleFormat: "call",
  },
  zcode: {
    id: "zcode",
    accentColor: "#3b82f6",
    assistantGlyph: "•",
    thinkingGlyph: "✻",
    resultConnector: "└",
    titleFormat: "call",
  },
  pi: {
    id: "pi",
    accentColor: "#14b8a6",
    assistantGlyph: "●",
    thinkingGlyph: "•",
    stepGlyph: { success: "●", error: "●" },
    resultConnector: "│",
    titleFormat: "verb",
    verbs: {
      shell: "bash",
      read: "read",
      search: "grep",
      edit: "edit",
      write: "write",
      web: "fetch",
    },
  },
};

const GENERIC_STYLE: AgentReaderStyle = {
  id: "generic",
  accentColor: "#4f6ef7",
  assistantGlyph: "•",
  thinkingGlyph: "✻",
  stepGlyph: { success: "•", error: "✗" },
  resultConnector: "└",
  titleFormat: "call",
  monoTitles: true,
  verbs: {},
};

export function getAgentReaderStyle(
  providerId?: string | null,
): AgentReaderStyle {
  const normalized = providerId?.toLowerCase() ?? "";
  if (normalized === "claude") return CLAUDE_STYLE;
  if (normalized === "codex") return CODEX_STYLE;
  if (normalized === "gemini") return GEMINI_STYLE;
  if (normalized === "opencode") return OPENCODE_STYLE;
  const custom = PROVIDER_ACCENTS[normalized];
  if (custom) {
    return {
      ...GENERIC_STYLE,
      ...custom,
      stepGlyph: custom.stepGlyph ?? GENERIC_STYLE.stepGlyph,
      verbs: custom.verbs ?? GENERIC_STYLE.verbs,
    };
  }
  return GENERIC_STYLE;
}

export function classifyToolKind(rawName: string): ToolActionKind {
  const lower = rawName.toLowerCase().replace(/^mcp__[^_]+__/, "");
  if (rawName.toLowerCase().startsWith("mcp__") || lower.includes("mcp")) {
    return "mcp";
  }
  if (
    [
      "bash",
      "shell",
      "exec_command",
      "run_command",
      "run_terminal_cmd",
      "powershell",
      "cmd",
      "terminal",
      "bashexecution",
    ].includes(lower)
  ) {
    return "shell";
  }
  if (
    [
      "read",
      "read_file",
      "readfile",
      "view",
      "cat",
      "open_file",
      "list_dir",
      "ls",
      "list",
    ].includes(lower)
  ) {
    return "read";
  }
  if (
    [
      "grep",
      "search",
      "ripgrep",
      "glob",
      "find",
      "codebase_search",
      "file_search",
    ].includes(lower)
  ) {
    return "search";
  }
  if (
    [
      "edit",
      "multiedit",
      "edit_file",
      "replace",
      "apply_patch",
      "patch",
      "str_replace_editor",
      "str_replace_based_edit_tool",
      "notebookedit",
    ].includes(lower)
  ) {
    return "edit";
  }
  if (["write", "write_file", "writefile", "create_file"].includes(lower)) {
    return "write";
  }
  if (
    [
      "web_search",
      "websearch",
      "webfetch",
      "web_fetch",
      "google_web_search",
      "fetch",
      "curl",
    ].includes(lower)
  ) {
    return "web";
  }
  if (
    [
      "task",
      "agent",
      "subagent",
      "spawn_agent",
      "invoke_subagent",
      "dispatch_agent",
    ].includes(lower)
  ) {
    return "agent";
  }
  if (
    ["todowrite", "todo_write", "todoread", "update_plan", "todo"].includes(
      lower,
    )
  ) {
    return "todo";
  }
  return "other";
}

export function shortenProjectPath(
  rawText: string,
  projectDir?: string | null,
): string {
  if (!rawText) return "";
  let text = rawText.trim();
  const root = projectDir?.trim().replace(/[\\/]+$/, "");
  if (root) {
    const normRoot = root.replace(/\\/g, "/").toLowerCase();
    const normText = text.replace(/\\/g, "/");
    if (normText.toLowerCase() === normRoot) {
      return ".";
    }
    if (normText.toLowerCase().startsWith(`${normRoot}/`)) {
      text = normText.slice(normRoot.length + 1);
      return text;
    }
  }
  return text
    .replace(/^[A-Za-z]:[\\/](?:Users|home)[\\/][^\\/]+[\\/]/, "~/")
    .replace(/^\/(?:Users|home)\/[^/]+\//, "~/");
}
