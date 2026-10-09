import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import {
  sessionsApi,
  type DeleteSessionOptions,
  type DeleteSessionReply,
} from "@/lib/api/sessions";
import type { SessionMessage, SessionMeta } from "@/types";
import { extractErrorMessage } from "@/utils/errorUtils";

export const useSessionsQuery = () => {
  const client = useQueryClient();
  const { data: warnings = [] } = useQuery<string[]>({
    queryKey: ["sessionScanWarnings"],
    queryFn: async () => [],
    initialData: [],
    enabled: false,
  });
  const query = useQuery<SessionMeta[]>({
    queryKey: ["sessions"],
    queryFn: () =>
      sessionsApi.list((issues) =>
        client.setQueryData(["sessionScanWarnings"], issues),
      ),
    staleTime: 30_000,
    // 全量扫描成本高（读所有会话文件 + 打开各 SQLite 库），切回窗口就
    // 重扫磁盘不可接受；数据刷新交给手动"重新扫描"和删除后的主动失效。
    refetchOnWindowFocus: false,
  });
  return { ...query, warnings };
};

export const useSessionMessagesQuery = (
  providerId?: string,
  sourcePath?: string,
) => {
  // 流式到达的部分结果只用于渐进渲染，不写进 react-query 缓存：半截数据一旦被当成
  // 新鲜的缓存，切回这个会话就会看到被截断的正文。
  const [streamed, setStreamed] = useState<{
    providerId: string;
    sourcePath: string;
    messages: SessionMessage[];
  } | null>(null);

  const query = useQuery<SessionMessage[]>({
    queryKey: ["sessionMessages", providerId, sourcePath],
    queryFn: () =>
      sessionsApi.streamMessages(providerId!, sourcePath!, (messages) =>
        setStreamed({
          providerId: providerId!,
          sourcePath: sourcePath!,
          messages,
        }),
      ),
    enabled: Boolean(providerId && sourcePath),
    staleTime: 30_000,
  });

  // 只在同一次取数过程中用渐进结果；流结束后交回 react-query 的完整数据
  const progressive =
    streamed &&
    streamed.providerId === providerId &&
    streamed.sourcePath === sourcePath
      ? streamed.messages
      : undefined;

  return {
    ...query,
    data: query.isFetching && progressive ? progressive : query.data,
  };
};

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
        toast.success(
          t("sessionManager.sessionDeleted", { defaultValue: "会话已删除" }),
        );
        // 兼容旧后端的警告；新后端会将未完成的清理返回 cleanup_pending。
        reply.warnings?.forEach((warning) => toast.warning(warning));
      } else if (reply.status === "cleanup_pending") {
        toast.warning("清理尚未完成，可在列表中继续清理", {
          description: reply.warnings.join("；"),
          duration: 12_000,
        });
      } else if (reply.status === "not_found") {
        // 后端明确说记录已不存在：不能再报"删除成功"，但要让缓存回到真实状态。
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
    onSettled: async (_reply, _error, input) => {
      await queryClient.cancelQueries({
        queryKey: ["sessionMessages", input.providerId, input.sourcePath],
      });
      queryClient.removeQueries({
        queryKey: ["sessionMessages", input.providerId, input.sourcePath],
      });
      await queryClient.invalidateQueries({ queryKey: ["sessions"] });
    },
  });
};
