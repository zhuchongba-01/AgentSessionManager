export type ToolActionKind =
  | "shell"
  | "read"
  | "search"
  | "edit"
  | "write"
  | "web"
  | "mcp"
  | "agent"
  | "ask"
  | "todo"
  | "other";

export type ToolRunStatus =
  | "success"
  | "error"
  | "interrupted"
  | "pending"
  | "unknown";

export interface AgentReaderStyle {
  id: string;
  /** 提问气泡浅底色、目录当前轮指示条与步骤强调色 */
  accentColor: string;
  assistantGlyph: string;
  thinkingGlyph: string;
  /** 各 Agent 原生思维链标签（思考中… / 推理 / 想法 / Thought / Thinking… / 思考） */
  thinkingLabel: string;
  stepGlyph: {
    success: string;
    error: string;
    pending: string;
    interrupted: string;
  };
  /** 结果详情框形态：tree 用符号引出（⎿ / └ / │），box 用带左状态条边框盒（Gemini），none 用左侧状态竖条（Pi） */
  resultFrame: "tree" | "box" | "none";
  resultConnector: string;
  titleFormat: "call" | "verb" | "icon";
  monoTitles: boolean;
  merge: {
    readSearchRuns: boolean;
    mcpSameServerRuns: boolean;
  };
  verbs: Partial<Record<ToolActionKind, string>>;
}

const CLAUDE_STYLE: AgentReaderStyle = {
  id: "claude",
  accentColor: "#d97757",
  assistantGlyph: "⏺",
  thinkingGlyph: "✻",
  thinkingLabel: "思考中…",
  stepGlyph: {
    success: "⏺",
    error: "⏺",
    pending: "⏺",
    interrupted: "⏺",
  },
  resultFrame: "tree",
  resultConnector: "⎿",
  titleFormat: "call",
  monoTitles: true,
  merge: { readSearchRuns: false, mcpSameServerRuns: true },
  verbs: {
    edit: "Update",
  },
};

const CODEX_STYLE: AgentReaderStyle = {
  id: "codex",
  accentColor: "#10a37f",
  assistantGlyph: "•",
  thinkingGlyph: "•",
  thinkingLabel: "推理",
  stepGlyph: {
    success: "•",
    error: "•",
    pending: "•",
    interrupted: "•",
  },
  resultFrame: "tree",
  resultConnector: "└",
  titleFormat: "verb",
  monoTitles: false,
  merge: { readSearchRuns: true, mcpSameServerRuns: false },
  verbs: {
    shell: "Ran",
    read: "Read",
    search: "Search",
    edit: "Edited",
    write: "Added",
    web: "Searched",
    mcp: "Called",
    agent: "Spawned agent",
    ask: "Asked",
    todo: "Updated plan",
  },
};

const GEMINI_STYLE: AgentReaderStyle = {
  id: "gemini",
  accentColor: "#3186ff",
  assistantGlyph: "✦",
  thinkingGlyph: "✦",
  thinkingLabel: "想法",
  stepGlyph: {
    success: "✓",
    error: "✗",
    pending: "○",
    interrupted: "−",
  },
  resultFrame: "box",
  resultConnector: "",
  titleFormat: "verb",
  monoTitles: true,
  merge: { readSearchRuns: false, mcpSameServerRuns: false },
  verbs: {
    shell: "Shell",
    read: "ReadFile",
    search: "Search",
    edit: "Edit",
    write: "WriteFile",
    web: "WebFetch",
    mcp: "MCP",
    agent: "Agent",
    ask: "Ask",
    todo: "Todos",
  },
};

const OPENCODE_STYLE: AgentReaderStyle = {
  id: "opencode",
  accentColor: "#6366f1",
  assistantGlyph: "",
  thinkingGlyph: "",
  thinkingLabel: "Thought",
  stepGlyph: {
    success: "✓",
    error: "✗",
    pending: "~",
    interrupted: "✗",
  },
  resultFrame: "tree",
  resultConnector: "│",
  titleFormat: "icon",
  monoTitles: true,
  merge: { readSearchRuns: false, mcpSameServerRuns: false },
  verbs: {
    shell: "$",
    read: "→ Read",
    search: "✱ Grep",
    edit: "← Edit",
    write: "← Wrote",
    web: "% WebFetch",
    mcp: "⚙ MCP",
    agent: "│ Task",
    ask: "→ Asked",
    todo: "⚙ Todos",
  },
};

const PI_STYLE: AgentReaderStyle = {
  id: "pi",
  accentColor: "#14b8a6",
  assistantGlyph: "",
  thinkingGlyph: "",
  thinkingLabel: "Thinking…",
  stepGlyph: {
    success: "●",
    error: "●",
    pending: "●",
    interrupted: "●",
  },
  resultFrame: "none",
  resultConnector: "",
  titleFormat: "verb",
  monoTitles: true,
  merge: { readSearchRuns: false, mcpSameServerRuns: false },
  verbs: {
    shell: "bash",
    read: "read",
    search: "grep",
    edit: "edit",
    write: "write",
    web: "fetch",
  },
};

const GENERIC_STYLE: AgentReaderStyle = {
  id: "generic",
  accentColor: "#4f6ef7",
  assistantGlyph: "•",
  thinkingGlyph: "•",
  thinkingLabel: "思考",
  stepGlyph: {
    success: "•",
    error: "✗",
    pending: "•",
    interrupted: "−",
  },
  resultFrame: "tree",
  resultConnector: "└",
  titleFormat: "verb",
  monoTitles: true,
  merge: { readSearchRuns: false, mcpSameServerRuns: false },
  verbs: {
    shell: "运行",
    read: "读取",
    search: "搜索",
    edit: "修改",
    write: "写入",
    web: "网页",
    mcp: "MCP",
    agent: "子代理",
    ask: "提问",
    todo: "待办",
  },
};

const PROVIDER_OVERRIDES: Record<string, Partial<AgentReaderStyle>> = {
  copilot: {
    id: "copilot",
    accentColor: "#8b5cf6",
    assistantGlyph: "●",
    thinkingGlyph: "•",
    thinkingLabel: "思考",
    resultConnector: "└",
    titleFormat: "call",
    verbs: {},
  },
  deepseek: {
    id: "deepseek",
    accentColor: "#4d6bfe",
    assistantGlyph: "⏺",
    thinkingGlyph: "✻",
    thinkingLabel: "思考",
    stepGlyph: CLAUDE_STYLE.stepGlyph,
    resultConnector: "⎿",
    titleFormat: "call",
    verbs: { edit: "Update" },
  },
  workbuddy: {
    id: "workbuddy",
    accentColor: "#0ac49a",
    assistantGlyph: "⏺",
    thinkingGlyph: "✻",
    thinkingLabel: "思考中…",
    stepGlyph: CLAUDE_STYLE.stepGlyph,
    resultConnector: "⎿",
    titleFormat: "call",
    verbs: { edit: "Update" },
  },
  codebuddy: {
    id: "codebuddy",
    accentColor: "#6c4dff",
    assistantGlyph: "⏺",
    thinkingGlyph: "✻",
    thinkingLabel: "思考中…",
    stepGlyph: CLAUDE_STYLE.stepGlyph,
    resultConnector: "⎿",
    titleFormat: "call",
    verbs: { edit: "Update" },
  },
  kimi: {
    id: "kimi",
    accentColor: "#2b8fff",
    assistantGlyph: "•",
    thinkingGlyph: "•",
    thinkingLabel: "思考",
    resultConnector: "└",
    titleFormat: "call",
    verbs: {},
  },
  openclaw: {
    ...PI_STYLE,
    id: "openclaw",
    accentColor: "#ef4444",
  },
  grokbuild: {
    id: "grokbuild",
    accentColor: "#f59e0b",
    assistantGlyph: "•",
    thinkingGlyph: "•",
    thinkingLabel: "思考",
    resultConnector: "└",
    titleFormat: "verb",
  },
  zcode: {
    ...OPENCODE_STYLE,
    id: "zcode",
    accentColor: "#3b82f6",
  },
};

export function getAgentReaderStyle(
  providerId?: string | null,
): AgentReaderStyle {
  const normalized = providerId?.toLowerCase() ?? "";
  if (normalized === "claude") return CLAUDE_STYLE;
  if (normalized === "codex") return CODEX_STYLE;
  if (normalized === "gemini") return GEMINI_STYLE;
  if (normalized === "opencode") return OPENCODE_STYLE;
  if (normalized === "pi") return PI_STYLE;
  const custom = PROVIDER_OVERRIDES[normalized];
  if (custom) {
    return {
      ...GENERIC_STYLE,
      ...custom,
      stepGlyph: custom.stepGlyph ?? GENERIC_STYLE.stepGlyph,
      merge: custom.merge ?? GENERIC_STYLE.merge,
      verbs: custom.verbs ?? GENERIC_STYLE.verbs,
    };
  }
  return GENERIC_STYLE;
}

export function formatDurationCompact(ms: number): string {
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const seconds = ms / 1000;
  if (seconds < 10) return `${Number(seconds.toFixed(1))}s`;
  const totalSeconds = Math.round(seconds);
  if (totalSeconds < 60) return `${totalSeconds}s`;
  if (totalSeconds < 3600) {
    const m = Math.floor(totalSeconds / 60);
    const s = totalSeconds % 60;
    return s ? `${m}m${s}s` : `${m}m`;
  }
  const h = Math.floor(totalSeconds / 3600);
  const m = Math.floor((totalSeconds % 3600) / 60);
  return m ? `${h}h${m}m` : `${h}h`;
}

export function formatTokensCompact(tokens: number): string {
  if (tokens < 1000) return String(tokens);
  if (tokens < 1_000_000) return `${Number((tokens / 1000).toFixed(1))}k`;
  return `${Number((tokens / 1_000_000).toFixed(1))}M`;
}

export function classifyToolKind(rawName: string): ToolActionKind {
  const lower = rawName.toLowerCase().replace(/^mcp__[^_]+__/, "");
  if (
    rawName.toLowerCase().startsWith("mcp__") ||
    lower === "mcptoolcall" ||
    lower.includes("mcp")
  ) {
    return "mcp";
  }
  if (
    [
      "bash",
      "shell",
      "exec",
      "exec_command",
      "local_shell",
      "commandexecution",
      "run_shell_command",
      "run_command",
      "run_terminal_cmd",
      "execute_command",
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
      "read_many_files",
      "readfile",
      "view",
      "cat",
      "open_file",
    ].includes(lower)
  ) {
    return "read";
  }
  if (
    [
      "grep",
      "grep_search",
      "search_file_content",
      "search",
      "ripgrep",
      "glob",
      "find",
      "ls",
      "list",
      "list_dir",
      "list_directory",
      "toolsearch",
      "ast_grep_search",
      "codesearch",
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
      "filechange",
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
      "web_search_call",
      "websearch",
      "webfetch",
      "web_fetch",
      "google_web_search",
      "open_page",
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
      "send_message",
      "invoke_subagent",
      "dispatch_agent",
    ].includes(lower)
  ) {
    return "agent";
  }
  if (
    [
      "askuserquestion",
      "ask_user",
      "question",
      "request_user_input_async",
    ].includes(lower)
  ) {
    return "ask";
  }
  if (
    [
      "todowrite",
      "todo_write",
      "todoread",
      "write_todos",
      "update_plan",
      "todo",
    ].includes(lower)
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
