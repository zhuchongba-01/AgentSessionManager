import { AgentIcon } from "@/components/sessions/AgentIcon";
import { cn } from "@/lib/utils";

export const APP_DISPLAY_NAME: Record<string, string> = {
  claude: "Claude Code",
  codex: "Codex",
  gemini: "Gemini CLI",
  copilot: "GitHub Copilot",
  opencode: "OpenCode",
  zcode: "ZCode",
  pi: "Pi",
  openclaw: "OpenClaw",
  deepseek: "DeepSeek Harness",
  workbuddy: "WorkBuddy",
  codebuddy: "CodeBuddy",
  kimi: "Kimi CLI",
  grokbuild: "Grok Build",
};

export function getAppDisplayName(providerId: string): string {
  return APP_DISPLAY_NAME[providerId] ?? providerId;
}

export function AppGlyph({
  app,
  size = 16,
  badgeClassName,
  className,
}: {
  app: string;
  size?: number;
  badgeClassName?: string;
  className?: string;
}) {
  return (
    <AgentIcon
      icon={app}
      name={getAppDisplayName(app)}
      size={size}
      className={cn(badgeClassName, className)}
    />
  );
}
