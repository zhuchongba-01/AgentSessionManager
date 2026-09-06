import { useTranslation } from "react-i18next";
import { Checkbox } from "@/components/ui/checkbox";
import { Badge } from "@/components/ui/badge";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import type { SessionMeta } from "@/types";
import { AgentIcon } from "./AgentIcon";
import {
  formatRelativeTime,
  formatSessionTitle,
  getProviderIconName,
  getProviderLabel,
  getSessionKey,
} from "./utils";

interface SessionItemProps {
  session: SessionMeta;
  isSelected: boolean;
  selectionMode: boolean;
  isChecked: boolean;
  isCheckDisabled?: boolean;
  onSelect: (key: string) => void;
  onToggleChecked: (checked: boolean) => void;
}

export function SessionItem({
  session,
  isSelected,
  selectionMode,
  isChecked,
  isCheckDisabled = false,
  onSelect,
  onToggleChecked,
}: SessionItemProps) {
  const { t } = useTranslation();
  const title = formatSessionTitle(session);
  const lastActive = session.lastActiveAt || session.createdAt || undefined;
  const sessionKey = getSessionKey(session);

  return (
    <div className={cn("asm-session-item group", isSelected && "is-selected")}>
      {selectionMode && (
        <Checkbox
          checked={isChecked}
          disabled={isCheckDisabled}
          aria-label={t("sessionManager.selectForBatch", {
            defaultValue: "选择会话",
          })}
          onCheckedChange={(checked) => onToggleChecked(Boolean(checked))}
          className="shrink-0"
        />
      )}
      <button
        type="button"
        onClick={() => onSelect(sessionKey)}
        className="flex min-w-0 flex-1 cursor-pointer items-center gap-2 text-left"
      >
        <Tooltip>
          <TooltipTrigger asChild>
            <span className="asm-provider-icon">
              <AgentIcon
                icon={getProviderIconName(session.providerId)}
                name={session.providerId}
                size={16}
              />
            </span>
          </TooltipTrigger>
          <TooltipContent>
            {getProviderLabel(session.providerId, t)}
          </TooltipContent>
        </Tooltip>
        <span className="asm-session-title">{title}</span>
        {session.residual && (
          <Badge
            variant="outline"
            title={t("sessionManager.residualDescription", {
              defaultValue:
                "本地文件仍存在，但该记录未显示在 Agent 的当前会话列表中",
            })}
            className="shrink-0 border-amber-500/50 px-1.5 py-0 text-[10px] font-medium text-amber-700 dark:border-amber-500/40 dark:bg-amber-500/10 dark:text-amber-300"
          >
            {t("sessionManager.residual", { defaultValue: "残留" })}
          </Badge>
        )}
        <span className="asm-session-time">
          {lastActive ? formatRelativeTime(lastActive, t) : t("common.unknown")}
        </span>
      </button>
    </div>
  );
}
