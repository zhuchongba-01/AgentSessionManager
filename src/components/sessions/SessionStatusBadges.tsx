import { Badge } from "@/components/ui/badge";
import { useTranslation } from "react-i18next";
import type { SessionMeta } from "@/types";

const baseClass =
  "h-[18px] shrink-0 px-1.5 py-0 text-[10px] font-medium leading-none";

export function SessionStatusBadges({ session }: { session: SessionMeta }) {
  const { t } = useTranslation();
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
      {session.residual && !session.archived && (
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
      )}
    </>
  );
}
