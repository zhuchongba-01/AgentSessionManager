import { Badge } from "@/components/ui/badge";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { useTranslation } from "react-i18next";
import type { SessionMeta } from "@/types";

const baseClass =
  "h-[18px] shrink-0 px-1.5 py-0 text-[10px] font-medium leading-none";

export function SessionStatusBadges({
  session,
  parentLabel,
}: {
  session: SessionMeta;
  /**
   * 父会话的显示名，由列表层解析（父行可能被搜索过滤掉，行组件自己看不到整张表）。
   * 缺省时退回原生 ID 前 8 位。
   */
  parentLabel?: string;
}) {
  const { t } = useTranslation();
  // 子会话的身份是「归属」，不是「残留」：只显示残留时用户根本不知道该删哪一条。
  // 残留的事实仍然重要，放进悬停提示里。
  const parent = session.parentSessionId
    ? (parentLabel ?? session.parentSessionId.slice(0, 8))
    : undefined;
  return (
    <>
      {session.cleanupPending && (
        <Badge
          variant="outline"
          className={`${baseClass} border-blue-500/45 text-blue-700 dark:border-blue-400/40 dark:bg-blue-500/10 dark:text-blue-300`}
        >
          {t("sessionManager.cleanupPending", { defaultValue: "待清理" })}
        </Badge>
      )}
      {session.archived && (
        <Badge
          variant="outline"
          className={`${baseClass} text-muted-foreground`}
        >
          {t("sessionManager.archived", { defaultValue: "已归档" })}
        </Badge>
      )}
      {parent ? (
        <Tooltip>
          <TooltipTrigger asChild>
            {/* Badge 不转发 ref，Tooltip 的 asChild 需要一层原生元素。
                归属是这一行的重点：给徽标 shrink-0，让长标题自己省略，
                而不是把「属于谁」挤到看不见。 */}
            <span className="inline-flex shrink-0">
              <Badge
                variant="outline"
                data-testid="session-parent-badge"
                className={`${baseClass} max-w-[10rem] overflow-hidden border-sky-500/50 text-sky-700 dark:border-sky-500/40 dark:bg-sky-500/10 dark:text-sky-300`}
              >
                <span className="truncate">
                  {t("sessionManager.childOf", {
                    defaultValue: "「{{parent}}」的子会话",
                    parent,
                  })}
                </span>
              </Badge>
            </span>
          </TooltipTrigger>
          <TooltipContent className="max-w-xs whitespace-pre-line">
            {t("sessionManager.childOfTooltip", {
              defaultValue:
                "这是「{{parent}}」的子 Agent 会话（{{sessionId}}）。\n本地文件仍在，但它不出现在 Agent 自己的会话列表里。\n删除父会话时可以勾选「同时删除子会话」，一次清理整条子会话链。",
              parent,
              sessionId: session.sessionId,
            })}
          </TooltipContent>
        </Tooltip>
      ) : session.residual && !session.archived ? (
        <Badge
          variant="outline"
          title={t("sessionManager.residualDescription", {
            defaultValue:
              "本地文件仍存在，但该记录未显示在 Agent 的当前会话列表中",
          })}
          className={`${baseClass} border-amber-500/50 text-amber-700 dark:border-amber-500/40 dark:bg-amber-500/10 dark:text-amber-300`}
        >
          {t("sessionManager.residual", { defaultValue: "残留" })}
        </Badge>
      ) : null}
    </>
  );
}
