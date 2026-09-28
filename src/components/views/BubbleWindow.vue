<template>
  <div
    id="bubble-app"
    :style="appStyleVars"
    class="relative overflow-hidden bg-transparent transition-none select-none"
    :class="swapClass"
  >
    <DialogueBox
      ref="dialogRef"
      :visible="bubbleVisible"
      :line="uiStore.showCharacterLine"
      :emotion="uiStore.showCharacterEmotion"
      :speed="uiStore.typeWriterSpeed"
      :instant="instant"
      :max-height="bubbleMaxHeight"
      :side="side"
      :align="align"
      :align-inset="alignInset"
      @typing-change="onTypingChange"
    />
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useSettingsStore } from "@/stores/modules/settings";
import { useUIStore } from "@/stores/modules/ui/ui";
import { useGameStore } from "@/stores/modules/game";
import DialogueBox from "../pet/DialogueBox.vue";
import {
  PET_BUBBLE_EVENT,
  PET_BUBBLE_REQUEST,
  PET_BUBBLE_TYPING_EVENT,
  PET_FINISH_TYPING_EVENT,
  type BubbleAlign,
  type BubbleMirror,
  type BubbleSide,
} from "../pet/bubbleMirror";
import {
  BAND_BASE,
  DIALOG_MAX_BASE,
  NOTIFICATION_MAX_BASE,
  TAIL_OVERHANG_BASE,
} from "../pet/constants";

const settingsStore = useSettingsStore();
const uiStore = useUIStore();
const gameStore = useGameStore();

const dialogRef = ref<InstanceType<typeof DialogueBox> | null>(null);

const instant = ref(true);
let firstLineSeen = false;

const scale = ref(settingsStore.pet?.scale || 1);
const px = (base: number) => `${Math.round(base * scale.value)}px`;

const bubbleVisible = computed(
  () => gameStore.currentStatus === "responding" && uiStore.showCharacterLine.trim() !== "",
);

const bubbleMaxHeight = computed(() => Math.round(DIALOG_MAX_BASE * scale.value));

const side = ref<BubbleSide>("above");

const align = ref<BubbleAlign>("top");

const alignInset = ref(0);

const swapping = ref(false);
const fadingIn = ref(false);
let fadeTimer: number | undefined;

watch(swapping, (now, before) => {
  if (now) {
    fadingIn.value = false;
    return;
  }
  if (!before) return;
  fadingIn.value = true;
  if (fadeTimer !== undefined) window.clearTimeout(fadeTimer);
  fadeTimer = window.setTimeout(() => {
    fadingIn.value = false;
    fadeTimer = undefined;
  }, 220);
});

const swapClass = computed(() => (swapping.value ? "swap-out" : fadingIn.value ? "swap-in" : ""));

const appStyleVars = computed(() => ({
  "--pet-ui-scale": scale.value.toString(),
  "--band-h": px(BAND_BASE),
  "--tail": px(TAIL_OVERHANG_BASE),
  "--dialog-h": px(DIALOG_MAX_BASE),
  "--notify-h": px(NOTIFICATION_MAX_BASE),
}));

const applyMirror = (m: BubbleMirror) => {
  if (m.petScale > 0) scale.value = m.petScale;
  if (m.bubbleSide) side.value = m.bubbleSide;
  if (m.bubbleAlign === "top" || m.bubbleAlign === "bottom") align.value = m.bubbleAlign;
  alignInset.value = Math.max(0, m.alignInset ?? 0);
  swapping.value = Boolean(m.swapping);
  if (!firstLineSeen && m.line.trim()) {
    firstLineSeen = true;
    instant.value = true;
  } else if (firstLineSeen) {
    instant.value = false;
  }
  gameStore.currentStatus = m.status as typeof gameStore.currentStatus;
  uiStore.showCharacterLine = m.line;
  uiStore.showCharacterTitle = m.title;
  uiStore.showCharacterSubtitle = m.subtitle;
  uiStore.showCharacterEmotion = m.emotion;
  uiStore.showCharacterMotionText = m.motionText;
  uiStore.currentAvatarAudio = m.avatarAudio ?? "None";
  settingsStore.setTextSpeed(m.textSpeed);
  uiStore.notification = m.notification as typeof uiStore.notification;
};

const onTypingChange = (typing: boolean) => {
  void getCurrentWindow().emitTo("main", PET_BUBBLE_TYPING_EVENT, { typing });
};

let unlisten: (() => void) | null = null;
let unlistenFinish: (() => void) | null = null;
const pingTimers: number[] = [];

onMounted(async () => {
  const appWindow = getCurrentWindow();
  document.body.style.backgroundColor = "transparent";
  document.documentElement.style.backgroundColor = "transparent";

  unlisten = await appWindow.listen<BubbleMirror>(PET_BUBBLE_EVENT, (event) =>
    applyMirror(event.payload),
  );
  unlistenFinish = await appWindow.listen(PET_FINISH_TYPING_EVENT, () =>
    dialogRef.value?.finishTyping(),
  );

  const ping = () => void appWindow.emit(PET_BUBBLE_REQUEST);
  ping();
  for (const ms of [120, 400, 1000]) {
    pingTimers.push(window.setTimeout(ping, ms));
  }
});

onUnmounted(() => {
  document.body.style.backgroundColor = "";
  document.documentElement.style.backgroundColor = "";
  unlisten?.();
  unlisten = null;
  unlistenFinish?.();
  unlistenFinish = null;
  pingTimers.forEach((t) => window.clearTimeout(t));
});
</script>

<style scoped>
#bubble-app {
  width: 100vw;
  height: 100dvh;
}

.swap-out {
  animation: bubble-swap-out 150ms ease-in forwards;
}

.swap-in {
  animation: bubble-swap-in 200ms ease-out;
}

@keyframes bubble-swap-out {
  from {
    opacity: 1;
  }
  to {
    opacity: 0;
  }
}

@keyframes bubble-swap-in {
  from {
    opacity: 0;
  }
  to {
    opacity: 1;
  }
}
</style>
