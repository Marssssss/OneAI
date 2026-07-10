import UIAbility from '@ohos.app.ability.UIAbility';
import type { AbilityConstant, Want } from '@ohos.app.ability.AbilityConstant';
import window from '@ohos.window';

export default class EntryAbility extends UIAbility {
  onCreate(want: Want, launchParam: AbilityConstant.LaunchParam) { /* one-time init */ }

  onWindowStageCreate(windowStage: window.WindowStage) {
    windowStage.loadContent('pages/ChatPage', (err) => {
      if (err.code) { /* log: failed to load content */ }
    });
  }

  onDestroy() { /* noop */ }
}
