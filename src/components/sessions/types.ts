export type ProviderFilter =
  | "all"
  | "codex"
  | "claude"
  | "gemini"
  | "copilot"
  | "deepseek"
  | "grokbuild"
  | "workbuddy"
  | "codebuddy"
  | "kimi"
  | "openclaw"
  | "opencode"
  | "zcode"
  | "pi";

export type SessionStatusFilter =
  | "all"
  | "residual"
  | "cleanupPending"
  | "archived"
  | "child";

export type SessionListViewMode = "flat" | "grouped" | "time";

export type SessionReaderViewMode = "all" | "dialogue" | "changes";

export interface GroupSelectionState {
  checked: boolean | "indeterminate";
  isSelected: boolean;
  selectedCount: number;
  selectableCount: number;
}

export interface SessionGroupExpansionState {
  expandedProviderIds: Set<string>;
  expandedDirectoryKeys: Set<string>;
  restoredFromStorage: boolean;
}

export const PROVIDER_FILTER_ORDER = [
  "codex",
  "claude",
  "gemini",
  "copilot",
  "deepseek",
  "grokbuild",
  "workbuddy",
  "codebuddy",
  "kimi",
  "openclaw",
  "opencode",
  "zcode",
  "pi",
] as const;
