import { invoke } from "@tauri-apps/api/core";
import type { SessionMessage, SessionMeta } from "@/types";

export interface DeleteSessionOptions {
  providerId: string;
  sessionId: string;
  sourcePath: string;
  includeProject?: boolean;
  sharedConfirmed?: boolean;
}

/** 后端 delete_session 的结构化结果（tag + snake_case 变体名）。 */
export type DeleteSessionReply =
  | { status: "deleted"; warnings?: string[] }
  | { status: "not_found" }
  | {
      status: "needs_shared_confirmation";
      sharedCount: number;
      providers: string[];
    };

export interface DeleteSessionResult extends DeleteSessionOptions {
  success: boolean;
  error?: string;
  warnings?: string[];
}

export const sessionsApi = {
  async list(): Promise<SessionMeta[]> {
    return await invoke("list_sessions");
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
      sharedConfirmed = false,
    } = options;
    return await invoke("delete_session", {
      providerId,
      sessionId,
      sourcePath,
      includeProject,
      sharedConfirmed,
    });
  },

  async deleteMany(
    items: DeleteSessionOptions[],
  ): Promise<DeleteSessionResult[]> {
    return await invoke("delete_sessions", { items });
  },

  async launchTerminal(options: {
    command: string;
    cwd?: string | null;
    customConfig?: string | null;
  }): Promise<boolean> {
    const { command, cwd, customConfig } = options;
    return await invoke("launch_session_terminal", {
      command,
      cwd,
      customConfig,
    });
  },
};
