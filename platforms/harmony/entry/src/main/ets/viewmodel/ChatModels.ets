// Chat models — @Observed for live UI updates via @ObjectLink in bubbles.

@Observed
export class ToolStep {
  callId: string;
  name: string;
  args: string;
  result: string | null = null;
  ok: boolean | null = null;

  constructor(callId: string, name: string, args: string) {
    this.callId = callId; this.name = name; this.args = args;
  }
}

@Observed
export class AssistantItem {
  thinking: string = '';
  thinkingActive: boolean = false;
  thinkingDone: boolean = false;
  thinkingExpanded: boolean = false;
  text: string = '';
  streaming: boolean = false;
  done: boolean = false;
  error: string | null = null;
  steps: ToolStep[] = [];
}

@Observed
export class UserItem {
  text: string;
  constructor(text: string) { this.text = text; }
}

// A chat entry: user or assistant.
export class ChatEntry {
  kind: 'user' | 'assistant';
  user?: UserItem;
  assistant?: AssistantItem;
  private constructor(kind: 'user' | 'assistant') { this.kind = kind; }
  static user(text: string): ChatEntry { const e = new ChatEntry('user'); e.user = new UserItem(text); return e; }
  static assistant(): ChatEntry { const e = new ChatEntry('assistant'); e.assistant = new AssistantItem(); return e; }
}
