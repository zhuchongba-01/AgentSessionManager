export type ProviderFilter =
  | "all"
  | "codex"
  | "grokbuild"
  | "claude"
  | "opencode"
  | "pi"
  | "deepseek"
  | "zcode"
  | "workbuddy";

export type SessionStatusFilter =
  | "all"
  | "residual"
  | "cleanupPending"
  | "archived"
  | "child";

export type SessionListViewMode = "flat" | "grouped";

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
  "deepseek",
  "grokbuild",
  "claude",
  "workbuddy",
  "opencode",
  "zcode",
  "pi",
] as const;
