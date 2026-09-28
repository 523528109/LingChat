<template>
  <!--
    气泡窗：只画「通知 + 气泡」，不承载交互（整窗点击穿透）。
    通知由 DialogueBox 自己钉在气泡顶边上方 —— 气泡高度随文本变化，通知因此
    始终紧贴气泡顶而不是窗口顶。显示内容全部来自宠物窗的镜像。
  -->
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

/**
 * 是否整段显示、不播打字机。
 *
 * 只在**挂载后的首个非空台词**上为真：气泡窗可能比台词晚建（切进桌宠时），
 * 或因为改尺寸被重建，那些场景只该复现气泡现状，不该重播动画。
 * 之后任何一句都算新台词 —— 该播打字机。
 * 因此这里盯的是"台词是否变过"，而不是"挂载是否完成"。
 */
const instant = ref(true);
let firstLineSeen = false;

/**
 * 缩放**只认镜像值**：气泡窗的窗口尺寸是 Rust 按宠物窗的 scale 创建的，
 * 这里若读本窗口自己的 store，就可能在滑杆调整后与窗口实际尺寸错位。
 */
const scale = ref(settingsStore.pet?.scale || 1);
const px = (base: number) => `${Math.round(base * scale.value)}px`;

const bubbleVisible = computed(
  () => gameStore.currentStatus === "responding" && uiStore.showCharacterLine.trim() !== "",
);

/**
 * 气泡正文可用高度 = 气泡带减去通知预算。
 *
 * 通知钉在气泡的背宠一侧（上置时在气泡上方、下置时在下方），所以气泡带是
 * 「正文 + 通知」共用的：正文取满整条带，通知就顶到窗口之外被裁掉。DIALOG_MAX_BASE
 * 正是按这个减法定的（278 - 72 - 16），超出这个高度的正文改为盒内滚动。
 */
const bubbleMaxHeight = computed(() => Math.round(DIALOG_MAX_BASE * scale.value));

/**
 * 气泡在宠物的哪一侧，由宠物窗判定后镜像过来（手动设置 + 自动选边）。
 * 决定整块内容的贴边方向、长尾朝向、通知位置与进出场动效方向。
 */
const side = ref<BubbleSide>("above");

/** 左右置时气泡贴窗口上边还是下边（宠物窗按宠物在屏幕上的高低判定后镜像过来） */
const align = ref<BubbleAlign>("top");

/**
 * 换位相位。
 *
 * 气泡窗是独立窗口，位置由 Rust 改，**跳位是瞬时的**：直接把内容画到新位置会看到
 * 气泡"闪"到另一边。所以宠物窗换位前会先把 swapping 置真，本窗口先淡掉内容，
 * 等窗口跳位、侧别镜像过来之后再淡回来 —— 跳位正好落在看不见的这段时间里。
 */
const swapping = ref(false);
/** 淡入只在 swapping 由真转假的那一次播 */
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

/** 镜像投影：写进本窗口的 store，DialogueBox / PetNotification 即可直接渲染 */
const applyMirror = (m: BubbleMirror) => {
  if (m.petScale > 0) scale.value = m.petScale;
  if (m.bubbleSide) side.value = m.bubbleSide;
  if (m.bubbleAlign === "top" || m.bubbleAlign === "bottom") align.value = m.bubbleAlign;
  swapping.value = Boolean(m.swapping);
  // 首个非空台词整段显示（复现现状）；此后任何一句都是新台词 → 播打字机
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
  // 语音字段必须镜像：气泡里的打字机读它决定是否播打字音效（见 TypeWriter.playRandomSound），
  // 而本窗口不跑事件处理器，不镜像就恒为 "None" → 有语音时也播。
  uiStore.currentAvatarAudio = m.avatarAudio ?? "None";
  settingsStore.setTextSpeed(m.textSpeed);
  uiStore.notification = m.notification as typeof uiStore.notification;
};

/**
 * 打字状态转发给宠物窗：自动推进调度器在那边（事件队列/语音/AUTO 开关都在宠物窗），
 * 气泡窗只负责画，自己跑不了状态机。
 */
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
  // 宠物窗推进时若气泡还在打字，先补全文本（跨窗口不能直调组件方法）
  unlistenFinish = await appWindow.listen(PET_FINISH_TYPING_EVENT, () =>
    dialogRef.value?.finishTyping(),
  );

  // 握手重试：气泡窗创建早于宠物窗注册监听时，第一次请求会石沉大海，
  // 之后要等到下一句台词才会推送 —— 表现为「切进桌宠时当前台词丢失」。
  // 镜像本身幂等，重发几次的代价可以忽略。
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

/* 换位：整块内容淡出 → 窗口跳位 → 淡入。跳位藏在这段看不见的窗口期间 */
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
