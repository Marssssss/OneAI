// ArkTS wrapper over the liboneai_napi native module. Parses the JSON the C
// facade exchanges (events / sessions / messages) into typed objects.

import oneai from 'liboneai_napi.so';
import type { AppHandle, SessionHandle, ProviderConfig, SessionInfo, ChatMessage, ChatEvent } from 'liboneai_napi.so';

export type { AppHandle, SessionHandle, ProviderConfig, SessionInfo, ChatMessage, ChatEvent };
export const { createApp, freeApp, hasProvider, createSession, freeSession, sessionId,
  listConversations, deleteConversation, sessionMessages, sessionSave,
  sessionInterrupt, sessionRunTask } = oneai;

export function parseSessions(json: string): SessionInfo[] {
  try { return JSON.parse(json) as SessionInfo[]; } catch { return []; }
}
export function parseMessages(json: string): ChatMessage[] {
  try { return JSON.parse(json) as ChatMessage[]; } catch { return []; }
}
export function parseEvent(json: string): ChatEvent | null {
  try { return JSON.parse(json) as ChatEvent; } catch { return null; }
}
export function configJson(c: ProviderConfig): string { return JSON.stringify(c); }

// Re-export the module handle type aliases for the VM.
export const native = oneai;
