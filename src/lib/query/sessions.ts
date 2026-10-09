import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useRef, useState } from "react";
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

interface SessionTranscript {
  messages: SessionMessage[];
  revision: number;
}

let nextMessageRevision = 0;

export const useSessionMessagesQuery = (
  providerId?: string,
  sourcePath?: string,
) => {
  // 部分结果只用于当前请求的渐进显示，不写入查询缓存。
  // 独立批次让内容完全相同的刷新也能使按需全文缓存失效。
  const activeRequest = useRef(0);
  const [streamed, setStreamed] = useState<
    (SessionTranscript & { providerId: string; sourcePath: string }) | null
  >(null);

  const query = useQuery<SessionTranscript>({
    queryKey: ["sessionMessages", providerId, sourcePath],
    queryFn: async ({ signal }) => {
      const revision = ++nextMessageRevision;
      activeRequest.current = revision;
      setStreamed({
        providerId: providerId!,
        sourcePath: sourcePath!,
        revision,
        messages: [],
      });
      const messages = await sessionsApi.streamMessages(
        providerId!,
        sourcePath!,
        (partial) => {
          // 切换会话、取消或重新读取后，旧 Channel 可能还在发送消息。
          if (signal.aborted || activeRequest.current !== revision) return;
          setStreamed({
            providerId: providerId!,
            sourcePath: sourcePath!,
            revision,
            messages: partial,
          });
        },
      );
      return { messages, revision };
    },
    enabled: Boolean(providerId && sourcePath),
    staleTime: 30_000,
  });

  const snapshot =
    query.isFetching &&
    streamed &&
    streamed.providerId === providerId &&
    streamed.sourcePath === sourcePath
      ? streamed
      : query.data;

  return {
    ...query,
    data: snapshot?.messages,
    messageRevision: snapshot?.revision ?? 0,
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
