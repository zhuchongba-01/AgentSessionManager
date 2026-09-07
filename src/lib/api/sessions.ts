import { invoke } from "@tauri-apps/api/core";
import type { SessionMessage, SessionMeta } from "@/types";

export interface DeleteSessionOptions {
  providerId: string;
  sessionId: string;
  sourcePath: string;
  includeProject?: boolean;
}

/** 后端 delete_session 的结构化结果（tag + snake_case 变体名）。 */
export type DeleteSessionReply =
  | { status: "deleted"; warnings?: string[] }
  | { status: "not_found" }
  | { status: "cleanup_pending"; warnings: string[] };

export interface DeleteSessionResult extends DeleteSessionOptions {
  success: boolean;
  error?: string;
  warnings?: string[];
}

export const sessionsApi = {
  async list(
    onWarnings?: (warnings: string[]) => void,
  ): Promise<SessionMeta[]> {
    const report = await invoke<
      { sessions: SessionMeta[]; warnings: string[] } | SessionMeta[]
    >("list_sessions");
    if (Array.isArray(report)) {
      onWarnings?.([]);
      return report;
    }
    onWarnings?.(report.warnings);
    return report.sessions;
  },

  async getMessages(
    providerId: string,
    sourcePath: string,
  ): Promise<SessionMessage[]> {
    return await invoke("get_session_messages", { providerId, sourcePath });
  },

  async delete(options: DeleteSessionOptions): Promise<DeleteSessionReply> {
    const {
      providerId,
      sessionId,
      sourcePath,
      includeProject = false,
    } = options;
    return await invoke("delete_session", {
      providerId,
      sessionId,
      sourcePath,
      includeProject,
    });
  },

  async deleteMany(
    items: DeleteSessionOptions[],
  ): Promise<DeleteSessionResult[]> {
    return await invoke("delete_sessions", { items });
  },

  async resume(
    providerId: string,
    sourcePath: string,
    launch: boolean,
  ): Promise<string> {
    return await invoke("session_resume", { providerId, sourcePath, launch });
  },
};
