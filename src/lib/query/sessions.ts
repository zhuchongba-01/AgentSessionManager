import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import {
  sessionsApi,
  type DeleteSessionOptions,
  type DeleteSessionReply,
} from "@/lib/api/sessions";
import type { SessionMessage, SessionMeta } from "@/types";
import { extractErrorMessage } from "@/utils/errorUtils";

export const useSessionsQuery = () =>
  useQuery<SessionMeta[]>({
    queryKey: ["sessions"],
    queryFn: () => sessionsApi.list(),
    staleTime: 30_000,
    // 全量扫描成本高（读所有会话文件 + 打开各 SQLite 库），切回窗口就
    // 重扫磁盘不可接受；数据刷新交给手动"重新扫描"和删除后的主动失效。
    refetchOnWindowFocus: false,
  });

export const useSessionMessagesQuery = (
  providerId?: string,
  sourcePath?: string,
) =>
  useQuery<SessionMessage[]>({
    queryKey: ["sessionMessages", providerId, sourcePath],
    queryFn: () => sessionsApi.getMessages(providerId!, sourcePath!),
    enabled: Boolean(providerId && sourcePath),
    staleTime: 30_000,
  });

export const useDeleteSessionMutation = () => {
  const queryClient = useQueryClient();
  const { t } = useTranslation();

  return useMutation({
    mutationFn: async (
      input: DeleteSessionOptions,
    ): Promise<DeleteSessionReply> => {
      const reply = await sessionsApi.delete(input);
      if (reply.status === "deleted") {
        queryClient.setQueryData<SessionMeta[]>(["sessions"], (current) =>
          (current ?? []).filter(
            (session) =>
              !(
                session.providerId === input.providerId &&
                session.sessionId === input.sessionId &&
                session.sourcePath === input.sourcePath
              ),
          ),
        );
        queryClient.removeQueries({
          queryKey: ["sessionMessages", input.providerId, input.sourcePath],
        });
        await queryClient.invalidateQueries({ queryKey: ["sessions"] });
        toast.success(
          t("sessionManager.sessionDeleted", { defaultValue: "会话已删除" }),
        );
        // 删除本身成功、只有后续索引清理失败：降级为警告而不是报失败，
        // 否则用户会重试一个已经不存在的会话。
        reply.warnings?.forEach((warning) => toast.warning(warning));
      } else if (reply.status === "not_found") {
        // 后端明确说记录已不存在：不能再报"删除成功"，但要让缓存回到真实状态。
        await queryClient.invalidateQueries({ queryKey: ["sessions"] });
        toast.info(
          t("sessionManager.sessionNotFound", {
            defaultValue: "会话记录已不存在，无需删除",
          }),
        );
      }
      return reply;
    },
    onError: (error: Error) => {
      toast.error(
        t("sessionManager.deleteFailed", {
          defaultValue: "删除会话失败: {{error}}",
          error: extractErrorMessage(error) || t("common.unknown"),
        }),
      );
    },
  });
};
