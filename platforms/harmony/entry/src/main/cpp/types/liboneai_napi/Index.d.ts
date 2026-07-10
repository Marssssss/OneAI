// Type declarations for the OneAI NAPI module (liboneai_napi.so).
// Imported from ArkTS as:  import oneai from 'liboneai_napi.so'
//
// Handles are BigInt (opaque pointers to the C facade's OneAiApp/OneAiSession).
// sessionRunTask returns a Promise that resolves on completion; the onEvent
// callback fires on the ArkTS thread for each streamed ChatEvent (JSON string).

export type AppHandle = bigint;
export type SessionHandle = bigint;

export interface ProviderConfig {
  kind: string;        // "openai" | "anthropic" | "ollama"
  api_key?: string;
  base_url?: string;
  model: string;
  host?: string;
  port?: number;
  db_path: string;
  default_tools: boolean;
}

export interface SessionInfo {
  id: string;
  title: string | null;
  message_count: number;
  updated_at_ms: number;
}

export interface ChatMessage {
  role: string;        // "user" | "assistant" | "system" | "tool"
  text: string;
}

export interface ChatEvent {
  type: 'StreamChunk' | 'Thinking' | 'ToolCall' | 'ToolResult' | 'DirectAnswer' | 'Complete' | 'Error';
  text?: string;
  final_text?: string;
  message?: string;
  id?: string;
  name?: string;
  args_json?: string;
  call_id?: string;
  tool_name?: string;
  content?: string;
  success?: boolean;
}

export const createApp: (config: ProviderConfig) => AppHandle;
export const freeApp: (app: AppHandle) => void;
export const hasProvider: (app: AppHandle) => boolean;
export const createSession: (app: AppHandle, id: string | null) => SessionHandle;
export const freeSession: (s: SessionHandle) => void;
export const sessionId: (s: SessionHandle) => string;
export const listConversations: (app: AppHandle) => SessionInfo[];
export const deleteConversation: (app: AppHandle, id: string) => void;
export const sessionMessages: (s: SessionHandle) => ChatMessage[];
export const sessionSave: (s: SessionHandle) => boolean;
export const sessionInterrupt: (s: SessionHandle) => void;
// onEvent fires (on the ArkTS thread) with a JSON ChatEvent string during the run.
// Resolves on completion (null) or rejects (error string).
export const sessionRunTask: (s: SessionHandle, task: string, onEvent: (eventJson: string) => void) => Promise<void>;

export default {
  createApp, freeApp, hasProvider, createSession, freeSession, sessionId,
  listConversations, deleteConversation, sessionMessages, sessionSave,
  sessionInterrupt, sessionRunTask,
};
