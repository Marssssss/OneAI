// ChatPage — ArkUI chat UI. Mirrors the Android Compose / macOS SwiftUI layout:
// top bar (drawer + title + settings) + message list (ForEach) + input bar,
// with a session-drawer overlay and a settings overlay. State mutation drives
// re-render via `streamTick` (bumped per event), sidestepping @Observed.

import { ChatViewModel } from '../viewmodel/ChatViewModel';
import { ChatEntry, AssistantItem, ToolStep } from '../viewmodel/ChatModels';
import type { ChatEvent, SessionInfo, ChatMessage } from '../utils/OneAiNative';
import common from '@ohos.app.ability.common';

@Entry
@Component
struct ChatPage {
  @State vm: ChatViewModel = new ChatViewModel(this.getUIContext().getHostContext() as common.UIAbilityContext);
  @State items: ChatEntry[] = [];
  @State sessions: SessionInfo[] = [];
  @State input: string = '';
  @State running: boolean = false;
  @State error: string | null = null;
  @State streamTick: number = 0;
  @State showDrawer: boolean = false;
  @State showSettings: boolean = false;
  private scroller: List = new List();

  async aboutToAppear() {
    await this.vm.loadConfig();
    await this.vm.ensureApp();
    this.sessions = this.vm.refreshSessions();
    if (this.sessions.length > 0) this.loadSession(this.sessions[0].id);
    else this.vm.newConversation();
  }

  private loadSession(id: string) {
    const msgs: ChatMessage[] = this.vm.loadSession(id);
    this.items = [];
    for (const m of msgs) {
      if (m.role === 'user' && m.text.trim().length > 0) this.items.push(ChatEntry.user(m.text));
      else if (m.role === 'assistant' && m.text.trim().length > 0) {
        const e = ChatEntry.assistant(); e.assistant!.text = m.text; e.assistant!.done = true;
        this.items.push(e);
      }
    }
    this.error = null;
    this.streamTick++;
  }

  private async send() {
    const task = this.input.trim();
    if (task.length === 0 || this.running) return;
    this.input = '';
    this.items.push(ChatEntry.user(task));
    const turn = ChatEntry.assistant();
    this.items.push(turn);
    this.running = true; this.error = null;
    // Mid-turn save so the new chat shows in the drawer instantly.
    this.vm.save();
    this.sessions = this.vm.refreshSessions();
    try {
      await this.vm.runTask(task, (ev: ChatEvent) => {
        this.vm.applyEvent(ev, turn.assistant!);
        this.streamTick++;
      });
    } catch (e) {
      turn.assistant!.error = String(e);
    }
    turn.assistant!.streaming = false; turn.assistant!.done = true; this.running = false;
    this.streamTick++;
    this.sessions = this.vm.refreshSessions();
  }

  private async retry() {
    const task = this.vm.lastUserTask;
    if (!task || this.running) return;
    const last = this.items[this.items.length - 1];
    if (last?.assistant?.error) { this.items.pop(); }  // drop failed turn, keep user bubble
    const turn = ChatEntry.assistant();
    this.items.push(turn);
    this.running = true; this.error = null;
    this.vm.save();
    try {
      await this.vm.runTask(task, (ev: ChatEvent) => {
        this.vm.applyEvent(ev, turn.assistant!);
        this.streamTick++;
      });
    } catch (e) {
      turn.assistant!.error = String(e);
    }
    turn.assistant!.streaming = false; turn.assistant!.done = true; this.running = false;
    this.streamTick++;
    this.sessions = this.vm.refreshSessions();
  }

  private relativeTime(ms: number): string {
    const diff = Date.now() - ms; const mins = Math.floor(diff / 60000);
    if (mins < 1) return '刚刚';
    if (mins < 60) return `${mins} 分钟前`;
    if (mins < 1440) return `${Math.floor(mins / 60)} 小时前`;
    if (mins < 10080) return `${Math.floor(mins / 1440)} 天前`;
    const d = new Date(ms);
    return `${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')} ${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`;
  }

  build() {
    Stack() {
      Column() {
        // Top bar
        Row() {
          Button('☰').onClick(() => this.showDrawer = true).backgroundColor(Color.Transparent);
          Text('OneAI').fontSize(18).fontWeight(FontWeight.Bold).margin({ left: 8 });
          Blank();
          Button('⚙').onClick(() => this.showSettings = true).backgroundColor(Color.Transparent);
        }.width('100%').height(48).padding({ left: 12, right: 12 });

        if (this.vm.needsKeyConfig) {
          Text('未配置 API Key,点击设置 → 填入 kind / model / key 后保存')
            .fontSize(12).padding(10).backgroundColor('#E7F0FF').borderRadius(8)
            .margin({ left: 12, right: 12, bottom: 6 }).onClick(() => this.showSettings = true);
        }

        // Message list
        List({ space: 14, scroller: this.scroller }) {
          ForEach(this.items, (entry: ChatEntry) => {
            if (entry.kind === 'user') {
              Row() { Blank(); Text(entry.user!.text).fontSize(14).padding(10)
                  .backgroundColor('#E7F0FF').borderRadius(14).constraintSize({ maxWidth: 320 }); }
            } else {
              this.AssistantBubble(entry.assistant!)
            }
          }, (entry: ChatEntry, i: number) => `${i}`)
        }.layoutWeight(1).width('100%').padding(12).scrollBar(BarState.Off)

        if (this.error) {
          Text(`✗ ${this.error}`).fontSize(12).fontColor('#E5484D').padding({ left: 12, right: 12, bottom: 4 });
        }

        // Input bar
        Row() {
          TextArea({ text: this.input, placeholder: '问点什么…' })
            .onChange((v: string) => this.input = v)
            .layoutWeight(1).maxLines(4).borderRadius(18).backgroundColor('#EFEFF1').margin({ right: 8 });
          if (this.running) {
            Button('■').backgroundColor('#E5484D').fontColor(Color.White)
              .borderRadius(20).width(40).height(40).onClick(() => this.vm.interrupt());
          } else {
            Button('➤').backgroundColor(this.input.trim().length > 0 ? '#4D6BFE' : '#EFEFF1')
              .fontColor(Color.White).borderRadius(20).width(40).height(40).onClick(() => this.send());
          }
        }.width('100%').padding(8).backgroundColor(Color.White);
      }.width('100%').height('100%')

      // Drawer overlay
      if (this.showDrawer) {
        this.Drawer()
      }
      // Settings overlay
      if (this.showSettings) {
        this.SettingsPanel()
      }
    }.width('100%').height('100%').backgroundColor('#F7F7F8')
  }

  @Builder AssistantBubble(item: AssistantItem) {
    Column({ space: 6 }) {
      // Thinking card
      if (item.thinking.length > 0) {
        Row() {
          Text('🧠').fontSize(12);
          Text(item.thinkingActive ? '思考中…' : '已深度思考').fontSize(12);
          if (!item.thinkingActive) {
            Blank();
            Text(item.thinkingExpanded ? '▾' : '▸').fontSize(12)
              .onClick(() => { item.thinkingExpanded = !item.thinkingExpanded; this.streamTick++; });
          }
        }.width('100%').padding(10).backgroundColor('#F2F4FB').borderRadius(10);
        if (item.thinkingActive || item.thinkingExpanded) {
          Text(item.thinking).fontSize(12).fontColor('#8A8A8A').maxLines(20).padding({ left: 10, right: 10, bottom: 8 });
        }
      }
      // Steps
      ForEach(item.steps, (s: ToolStep) => {
        Column() {
          Text(`${s.ok === true ? '✓' : s.ok === false ? '✗' : '⚙'} ${s.name}(${s.args})`)
            .fontSize(11).fontFamily('monospace').fontColor(s.ok === true ? '#3B8C5A' : s.ok === false ? '#E5484D' : '#8A8A8A');
          if (s.result) Text(`    └ ${s.result.slice(0, 200)}`).fontSize(11).fontFamily('monospace').fontColor('#8A8A8A');
        }.alignItems(HorizontalAlign.Start).width('100%');
      }, (s: ToolStep, i: number) => `${i}`)

      if (item.text.length > 0) {
        Text(item.text).fontSize(14).fontColor('#1A1A1A').width('100%');
      }
      if (item.streaming && item.text.length > 0) { Text('▍').fontSize(14).fontColor('#4D6BFE'); }
      if (item.error) {
        Row() {
          Text(`✗ ${item.error}`).fontSize(12).fontColor('#E5484D').layoutWeight(1);
          Button('重试').fontSize(12).backgroundColor(Color.Transparent).onClick(() => this.retry());
        }.width('100%');
      }
    }.alignItems(HorizontalAlign.Start).width('100%')
  }

  @Builder Drawer() {
    Stack({ alignContent: Alignment.End }) {
      Column().width('100%').height('100%').backgroundColor(Color.Black).opacity(0.3)
        .onClick(() => this.showDrawer = false);
      Column() {
        Row() {
          Text('会话').fontSize(16).fontWeight(FontWeight.Medium);
          Blank();
          Button('+ 新对话').fontSize(12).backgroundColor(Color.Transparent)
            .onClick(() => { this.vm.newConversation(); this.showDrawer = false; });
        }.width('100%').padding(12);
        Divider();
        if (this.sessions.length === 0) {
          Text('还没有会话\n发一条消息开始吧').fontSize(12).fontColor('#8A8A8A').padding(20);
        } else {
          List() {
            ForEach(this.sessions, (s: SessionInfo) => {
              Row() {
                Column() {
                  Text(s.title ? s.title : '新对话').fontSize(13).maxLines(1).fontWeight(this.vm.currentSessionId === s.id ? FontWeight.Medium : FontWeight.Normal);
                  Text(`${s.message_count} 条 · ${this.relativeTime(s.updated_at_ms)}`).fontSize(11).fontColor('#8A8A8A').maxLines(1);
                }.layoutWeight(1).alignItems(HorizontalAlign.Start);
                Text('🗑').fontSize(12).onClick(() => { this.vm.deleteConversation(s.id); this.sessions = this.vm.refreshSessions(); });
              }.width('100%').padding({ left: 16, right: 16, top: 10, bottom: 10 })
              .backgroundColor(this.vm.currentSessionId === s.id ? '#E7F0FF' : Color.Transparent)
              .onClick(() => { this.loadSession(s.id); this.showDrawer = false; });
            }, (s: SessionInfo) => s.id);
          }.layoutWeight(1);
        }
      }.width('70%').height('100%').backgroundColor(Color.White);
    }.width('100%').height('100%')
  }

  @Builder SettingsPanel() {
    Stack({ alignContent: Alignment.Center }) {
      Column().width('100%').height('100%').backgroundColor(Color.Black).opacity(0.4)
        .onClick(() => this.showSettings = false);
      Column({ space: 10 }) {
        Text('Provider 设置').fontSize(16).fontWeight(FontWeight.Medium);
        // kind picker
        Row() {
          ForEach(['openai', 'anthropic', 'ollama'], (k: string) => {
            Button(k).fontSize(12)
              .backgroundColor(this.vm.provider.kind === k ? '#4D6BFE' : '#EFEFF1')
              .fontColor(this.vm.provider.kind === k ? Color.White : '#1A1A1A')
              .margin({ right: 8 })
              .onClick(() => { this.vm.applyPreset(k); this.streamTick++; });
          }, (k: string) => k);
        }
        TextInput({ text: this.vm.provider.model, placeholder: 'model' })
          .onChange((v: string) => this.vm.provider.model = v).fontSize(13);
        TextInput({ text: this.vm.provider.api_key, placeholder: 'api key' })
          .onChange((v: string) => this.vm.provider.api_key = v).fontSize(13).type(InputType.Password);
        TextInput({ text: this.vm.provider.base_url, placeholder: 'base url (ollama → host:port)' })
          .onChange((v: string) => this.vm.provider.base_url = v).fontSize(13);
        Text('ollama 示例:kind=ollama, model=llama3, base url=127.0.0.1:11434。保存后重建 App(历史保留)。')
          .fontSize(11).fontColor('#8A8A8A');
        Row() {
          Blank();
          Button('保存').fontSize(15).backgroundColor('#4D6BFE').fontColor(Color.White)
            .onClick(async () => {
              await this.vm.saveConfig();
              await this.vm.rebuildApp();
              this.sessions = this.vm.refreshSessions();
              if (this.sessions.length > 0) this.loadSession(this.sessions[0].id);
              else this.vm.newConversation();
              this.showSettings = false;
            });
        }.width('100%');
      }.width('85%').padding(16).backgroundColor(Color.White).borderRadius(12);
    }.width('100%').height('100%');
  }
}
