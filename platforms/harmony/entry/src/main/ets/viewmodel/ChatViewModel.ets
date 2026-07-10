// ChatViewModel — holds the native app/session handles + provider config, and
// drives the NAPI calls. UI state lives in the page (@State); this VM returns
// data the page applies. The streaming onEvent fires on the ArkTS thread, so
// the page can mutate @State directly inside it.

import oneai from 'liboneai_napi.so';
import { preferences } from '@ohos.data.preferences';
import common from '@ohos.app.ability.common';
import { configJson, parseSessions, parseMessages, parseEvent } from '../utils/OneAiNative';
import type { AppHandle, SessionHandle, ProviderConfig, SessionInfo, ChatMessage, ChatEvent } from '../utils/OneAiNative';
import { ChatEntry, AssistantItem, ToolStep } from './ChatModels';

const PRESETS: Record<string, { model: string; baseUrl: string }> = {
  openai: { model: 'gpt-4o-mini', baseUrl: '' },
  anthropic: { model: 'claude-sonnet-4-6', baseUrl: '' },
  ollama: { model: 'llama3', baseUrl: '127.0.0.1:11434' },
};

export class ChatViewModel {
  private context: common.UIAbilityContext;
  private prefsName = 'oneai_provider';

  app: AppHandle = 0n;
  session: SessionHandle = 0n;
  currentSessionId: string | null = null;
  lastUserTask: string | null = null;

  provider: ProviderConfig = {
    kind: 'openai', api_key: '', base_url: '', model: 'gpt-4o-mini',
    host: undefined, port: undefined, db_path: '', default_tools: true,
  };

  constructor(context: common.UIAbilityContext) { this.context = context; }

  get needsKeyConfig(): boolean {
    return (this.provider.kind === 'openai' || this.provider.kind === 'anthropic') &&
           (this.provider.api_key ?? '').length === 0;
  }

  async loadConfig(): Promise<void> {
    try {
      const p = await preferences.getPreferences(this.context, { name: this.prefsName });
      this.provider.kind = (await p.get('kind', 'openai') as string) ?? 'openai';
      this.provider.model = (await p.get('model', 'gpt-4o-mini') as string) ?? 'gpt-4o-mini';
      this.provider.api_key = (await p.get('apiKey', '') as string) ?? '';
      this.provider.base_url = (await p.get('baseUrl', '') as string) ?? '';
    } catch { /* defaults */ }
  }

  async saveConfig(): Promise<void> {
    const p = await preferences.getPreferences(this.context, { name: this.prefsName });
    await p.put('kind', this.provider.kind);
    await p.put('model', this.provider.model);
    await p.put('apiKey', this.provider.api_key ?? '');
    await p.put('baseUrl', this.provider.base_url ?? '');
    await p.flush();
  }

  applyPreset(newKind: string): void {
    if (newKind === this.provider.kind) return;
    this.provider.kind = newKind;
    const preset = PRESETS[newKind];
    if (preset) { this.provider.model = preset.model; this.provider.base_url = preset.baseUrl; }
  }

  private dbPath(): string {
    // filesDir = app private files dir (the HarmonyOS counterpart of Android filesDir).
    return `${this.context.filesDir}/oneai.db`;
  }

  private toConfig(): ProviderConfig {
    const c = { ...this.provider };
    c.db_path = this.dbPath();
    c.default_tools = true;
    return c;
  }

  async ensureApp(): Promise<boolean> {
    if (this.app !== 0n) return true;
    try {
      this.app = oneai.createApp(configJson(this.toConfig()));
      return this.app !== 0n;
    } catch { return false; }
  }

  async rebuildApp(): Promise<void> {
    if (this.session) oneai.freeSession(this.session);
    if (this.app) oneai.freeApp(this.app);
    this.app = 0n; this.session = 0n; this.currentSessionId = null;
    await this.ensureApp();
  }

  refreshSessions(): SessionInfo[] {
    if (!this.app) return [];
    return parseSessions(oneai.listConversations(this.app)).sort((a, b) => b.updated_at_ms - a.updated_at_ms);
  }

  newConversation(): void {
    if (!this.app) return;
    this.session = oneai.createSession(this.app, null);
    this.currentSessionId = oneai.sessionId(this.session);
    this.lastUserTask = null;
  }

  loadSession(id: string): ChatMessage[] {
    if (!this.app) return [];
    this.session = oneai.createSession(this.app, id);
    this.currentSessionId = oneai.sessionId(this.session);
    this.lastUserTask = null;
    return parseMessages(oneai.sessionMessages(this.session));
  }

  deleteConversation(id: string): void { if (this.app) oneai.deleteConversation(this.app, id); }
  save(): void { if (this.session) oneai.sessionSave(this.session); }
  interrupt(): void { if (this.session) oneai.sessionInterrupt(this.session); }

  /** Run a task. onEvent fires on the ArkTS thread for each streamed event;
   *  resolves on completion (no value) or rejects (error string).
   *  addUserItem=false is used by retry to avoid duplicating the user bubble. */
  runTask(task: string, onEvent: (ev: ChatEvent) => void): Promise<void> {
    return oneai.sessionRunTask(this.session, task, (json: string) => {
      const ev = parseEvent(json);
      if (ev) onEvent(ev);
    });
  }

  /** Apply a ChatEvent to an AssistantItem, mirroring the Android/macOS handler. */
  applyEvent(ev: ChatEvent, turn: AssistantItem): void {
    switch (ev.type) {
      case 'Thinking': turn.thinkingActive = true; turn.thinking += ev.text ?? ''; break;
      case 'StreamChunk':
        if (turn.thinkingActive) { turn.thinkingActive = false; turn.thinkingDone = true; }
        turn.streaming = true; turn.text += ev.text ?? ''; break;
      case 'ToolCall': turn.steps.push(new ToolStep(ev.id ?? '', ev.name ?? '', ev.args_json ?? '')); break;
      case 'ToolResult': {
        let step: ToolStep | undefined = turn.steps.find(s => s.callId === ev.call_id);
        if (!step) { for (let i = turn.steps.length - 1; i >= 0; i--) if (turn.steps[i].result === null) { step = turn.steps[i]; break; } }
        if (step) { step.result = ev.content ?? null; step.ok = ev.success ?? false; }
        break;
      }
      case 'DirectAnswer':
        if (ev.text && ev.text.length > 0) turn.text = ev.text;
        if (turn.thinkingActive) { turn.thinkingActive = false; turn.thinkingDone = true; }
        break;
      case 'Complete':
        if (ev.final_text && ev.final_text.length > 0) turn.text = ev.final_text;
        if (turn.thinkingActive) { turn.thinkingActive = false; turn.thinkingDone = true; }
        turn.streaming = false; turn.done = true; break;
      case 'Error': turn.error = ev.message ?? ''; turn.streaming = false; turn.done = true; break;
    }
  }
}
