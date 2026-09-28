<template>
  <!--
    宠物窗 = 锚点窗口：只有头像带与输入带，顶边就是头像顶边，因此贴得住屏幕最顶。
    气泡/通知不在本窗口内，见 BubbleWindow.vue。
  -->
  <div
    id="pet-app"
    :style="appStyleVars"
    class="relative flex h-(--app-height) w-(--app-width) flex-col items-center overflow-hidden bg-transparent transition-none select-none"
  >
    <DragArea :isDragging="isDragging">
      <div
        ref="avatarContainer"
        class="flex shrink-0 items-center justify-center bg-transparent"
        :style="{ width: 'var(--avatar-size)', height: 'var(--avatar-size)' }"
      >
        <GameRolesStage
          @avatar-click="handleAvatarClick"
          @open-settings="handleOpenSettings"
          @switch-auto-mode="handleSwitchAutoMode"
          @exit-pet-mode="handleExitPetMode"
          @audio-ended="handleAudioFinished"
          @audio-started="handleAudioStarted"
        />
      </div>
    </DragArea>

    <!-- 输入带：输入行**靠下**对齐，底部只留发送键光晕的余量 —— 下置气泡贴着
         本窗底边，带内底部留空多少，气泡看起来就离输入框多远 -->
    <div
      ref="chatContainer"
      class="flex w-full shrink-0 items-end justify-center bg-transparent transition-none"
      :style="{ height: 'var(--chat-h)', paddingBottom: px(CHAT_BASE_PB) }"
    >
      <ChatInput ref="ChatInputRef" :visible="showChatInput" />
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";
import { invoke } from "@tauri-apps/api/core";
import { WebviewWindow } from "@tauri-apps/api/webviewWindow";
import { getCurrentWindow, currentMonitor } from "@tauri-apps/api/window";

import { useGameStore } from "@/stores/modules/game";
import { useSettingsStore } from "@/stores/modules/settings";
import { useUIStore } from "@/stores/modules/ui/ui";
import { useAutoAdvance } from "@/composables/chat/useAutoAdvance";
import { useDialogAdvance } from "@/composables/chat/useDialogAdvance";

import ChatInput from "../pet/ChatInput.vue";
import DragArea from "../pet/DragArea.vue";
import GameRolesStage from "../pet/GameRolesStage.vue";
import { useFileDrop } from "../pet/useFileDrop";
import {
  AVATAR_BAND_BASE,
  BUBBLE_GAP_BASE,
  BUBBLE_WINDOW_H_BASE,
  CHAT_BASE_H,
  CHAT_BASE_PB,
  PET_WINDOW_H_BASE,
  WINDOW_WIDTH_BASE,
} from "../pet/constants";
import {
  PET_BUBBLE_EVENT,
  PET_BUBBLE_REQUEST,
  PET_BUBBLE_TYPING_EVENT,
  PET_FINISH_TYPING_EVENT,
  type BubbleAlign,
  type BubbleMirror,
  type BubbleSide,
} from "../pet/bubbleMirror";

const { t } = useI18n();
const router = useRouter();
const gameStore = useGameStore();
const settingsStore = useSettingsStore();
const uiStore = useUIStore();

const showChatInput = ref(false);
const { isDragging } = useFileDrop();

/**
 * 气泡内打字机的打字状态（由气泡窗广播回来）。
 *
 * 桌宠重构后打字机随 DialogueBox 搬进了独立的 BubbleWindow，而自动推进调度器
 * （事件队列 / 语音 / AUTO 开关）仍在宠物窗，跨窗口拿不到组件 ref，只能靠
 * `pet:bubble-typing` 事件同步——否则调度器以为永远不在打字，自动对话直接失效。
 */
const bubbleTyping = ref(false);

const avatarContainer = ref<HTMLElement | null>(null);
const chatContainer = ref<HTMLElement | null>(null);
const ChatInputRef = ref<InstanceType<typeof ChatInput> | null>(null);

const scale = computed(() => settingsStore.pet?.scale || 1);
const px = (base: number) => `${Math.round(base * scale.value)}px`;

const appStyleVars = computed(() => ({
  "--pet-ui-scale": scale.value.toString(),
  "--app-width": px(WINDOW_WIDTH_BASE),
  "--app-height": px(PET_WINDOW_H_BASE),
  "--avatar-size": px(AVATAR_BAND_BASE),
  "--chat-h": px(CHAT_BASE_H),
}));

// ─────────────────────────── 镜像显示状态给气泡窗 ───────────────────────────

/** 设置窗广播的气泡位置变更事件 */
const PET_BUBBLE_SIDE_EVENT = "pet-bubble-side-changed";

const appWindow = getCurrentWindow();

const emitMirror = () => {
  const payload: BubbleMirror = {
    status: gameStore.currentStatus,
    line: uiStore.showCharacterLine,
    title: uiStore.showCharacterTitle,
    subtitle: uiStore.showCharacterSubtitle,
    emotion: uiStore.showCharacterEmotion,
    motionText: uiStore.showCharacterMotionText,
    textSpeed: uiStore.typeWriterSpeed,
    // 语音字段必须镜像：气泡里的打字机据此决定是否播打字音效（见 TypeWriter.playRandomSound），
    // 气泡窗不跑事件处理器，不镜像就恒为 "None" → 有角色语音时也会播打字机音效。
    avatarAudio: uiStore.currentAvatarAudio,
    petScale: scale.value,
    bubbleSide: mirroredSide.value,
    bubbleAlign: mirroredAlign.value,
    // 侧置时内容的纵向内缩：宠物贴边出屏时让气泡窗把内容拉回工作区内（贴着屏幕边缘）
    alignInset: bubbleAlignInset.value,
    swapping: swapping.value,
    notification: {
      isVisible: uiStore.notification.isVisible,
      title: uiStore.notification.title,
      message: uiStore.notification.message,
      type: uiStore.notification.type,
    },
  };
  void appWindow.emitTo("pet_bubble", PET_BUBBLE_EVENT, payload);
};

// ─────────────────── 气泡位置：手动设置 + 自动选边 ───────────────────
//
// 方位完全由本窗口判定（Rust 只负责据此摆窗口）：above / below / left / right 手动指定，
// auto 见 bubbleSide 的注释。判定要用宠物的屏幕位置与显示器工作区，所以这里跟踪窗口位置
// 与尺寸，变化时重判一次（applyBubblePlacement 负责把结果落到气泡窗）。
const petLeftCss = ref(0);
const petTopCss = ref(0);
const petWidthCss = ref(WINDOW_WIDTH_BASE);
const petHeightCss = ref(PET_WINDOW_H_BASE);
const workLeftCss = ref(0);
const workTopCss = ref(0);
const workRightCss = ref(0);
const workBottomCss = ref(0);
const layoutReady = ref(false);
/**
 * 最近一次同步到的显示器缩放。`onMoved` 里要当场把物理像素换算成 CSS 像素（见那里），
 * 来不及再查一次显示器，所以缓存一份；换屏由防抖后的 syncBubblePlacement 刷新。
 */
let petDpr = 1;
/** 换位期间为真：气泡窗据此把内容淡掉，遮住独立窗口的瞬时跳位 */
const swapping = ref(false);
/**
 * 已镜像给气泡窗的方位与贴边。**滞后于判定值**：窗口还没挪过去之前不能翻转版式，
 * 否则气泡窗会在旧位置用新朝向画一帧（长尾、通知都会跳）。
 */
const mirroredSide = ref<BubbleSide>("above");
const mirroredAlign = ref<BubbleAlign>("top");

const bubbleSideSetting = computed(() => settingsStore.pet?.bubbleSide ?? "above");

/** 四边各自还能放下整块气泡窗的余量（CSS px，<0 = 放不下） */
const sideRooms = computed(() => {
  const gap = BUBBLE_GAP_BASE * scale.value;
  const bubbleW = WINDOW_WIDTH_BASE * scale.value;
  const bubbleH = BUBBLE_WINDOW_H_BASE * scale.value;
  return {
    above: petTopCss.value - workTopCss.value - bubbleH - gap,
    below: workBottomCss.value - (petTopCss.value + petHeightCss.value) - bubbleH - gap,
    left: petLeftCss.value - workLeftCss.value - bubbleW - gap,
    right: workRightCss.value - (petLeftCss.value + petWidthCss.value) - bubbleW - gap,
  };
});

/**
 * 自动选边用的判定常量（基准值，实际都乘 `pet.scale` —— 宠物窗及其内容整体缩放）。
 */

/**
 * 上方至少要有这么多空间才继续上置（CSS px，基准值）。
 *
 * 按**单行台词**算，而不是整块气泡窗（296）：气泡窗是透明窗口，内容贴着它靠近宠物的
 * 那条边排，短台词只用到边上的一小截，其余部分即使被顶到屏幕外也看不见。按窗高算的话，
 * 宠物一进上半屏就会被判成「上方放不下」而掉到下置。
 * 单行 ≈ 台词行 21 + 情绪行 20 + 盒内边距 12 + 边框 2 + 行下留白 6 ≈ 61，
 * 再加气泡盒相对宠物窗顶边的 长尾 10 + 间隙 8 = 18 → 79；取 88 留一点余量
 * （"明显放不下"才切下置）。
 */
const ABOVE_MIN_ROOM_BASE = 88;

/**
 * 「贴住某条屏幕边」的判定距离（CSS px，基准值）。
 *
 * 看的是**头像可见区**（≈ 头像尺寸的 15%），不是宠物窗：窗下半截是透明的输入带，
 * 贴角时它早就出屏了，用它判会松掉一大截。要同时贴住一条横边与一条纵边才算缩在角上。
 */
const CORNER_BAND_BASE = 32;

/**
 * 迟滞带（CSS px，基准值）。反方向判定要多让出这么多距离才切回来，
 * 宠物停在判定线上时不会来回横跳。
 */
const HYSTERESIS_BASE = 48;

/** 头像可见区在宠物窗内的水平内缩（与 components/pet/constants.ts 同步：头像居中、贴窗顶） */
const AVATAR_INSET_X_BASE = (WINDOW_WIDTH_BASE - AVATAR_BAND_BASE) / 2;

/**
 * 气泡放在宠物的哪一侧（auto）。
 *
 * **默认始终上置**，只有两种情况例外：
 *   1. **极其边角**（头像同时贴住一条横边与一条纵边，例如几乎贴紧屏幕左下角）→ 侧置，
 *      方向自动取余量大的那侧；
 *   2. 上方空间小到**单行台词都明显放不下**（贴着屏幕上沿）→ 下置。上沿的边角已在第 1 条
 *      被拦下，所以「左右上角」不会走到这里。
 *
 * 优先级 **上方 > 下方 > 左侧 = 右侧**（都是气泡窗整体相对宠物的方位）。判定都带迟滞，
 * 且看的是**当前已生效**的方位（`mirroredSide` 滞后于判定值），停在边界附近不会反复换位。
 */
const bubbleSide = computed<BubbleSide>(() => {
  const mode = bubbleSideSetting.value;
  if (mode !== "auto") return mode;
  if (!layoutReady.value) return "above";

  const s = scale.value;
  const current = mirroredSide.value;
  const sidePlaced = current === "left" || current === "right";
  const hysteresis = HYSTERESIS_BASE * s;

  const rooms = sideRooms.value;
  const sideFits = rooms.right >= 0 || rooms.left >= 0;
  const roomierSide: BubbleSide = rooms.right >= rooms.left ? "right" : "left";

  // 头像可见区相对工作区的四条边距
  const avatarLeft = petLeftCss.value + AVATAR_INSET_X_BASE * s;
  const avatarRight = petLeftCss.value + petWidthCss.value - AVATAR_INSET_X_BASE * s;
  const avatarTop = petTopCss.value;
  const avatarBottom = petTopCss.value + AVATAR_BAND_BASE * s;
  /** 已侧置时判定带放宽一个迟滞带：要走出明显一段距离才算离开边角 */
  const band = (CORNER_BAND_BASE + (sidePlaced ? HYSTERESIS_BASE : 0)) * s;
  const atLeftEdge = avatarLeft - workLeftCss.value <= band;
  const atRightEdge = workRightCss.value - avatarRight <= band;
  const atTopEdge = avatarTop - workTopCss.value <= band;
  const atBottomEdge = workBottomCss.value - avatarBottom <= band;

  // 1) 极其边角 → 侧置（方向自动）
  if ((atLeftEdge || atRightEdge) && (atTopEdge || atBottomEdge) && sideFits) return roomierSide;

  // 2) 上方空间不足（单行都放不下）→ 下置；下方也放不下就退回侧置，再不行按余量挑。
  //    已在上置时门槛寸步不让（否则单行会被顶出屏幕），从别的方位切回上方则多要一个
  //    迟滞带 —— 这一对门槛就是上方 ↔ 下方/侧置之间的迟滞。
  const aboveRoom = petTopCss.value - workTopCss.value;
  const aboveMinRoom = ABOVE_MIN_ROOM_BASE * s + (current === "above" ? 0 : hysteresis);
  if (aboveRoom < aboveMinRoom) {
    if (rooms.below >= 0) return "below";
    if (sideFits) return roomierSide;
    const all: BubbleSide[] = ["above", "below", "left", "right"];
    return all.reduce(
      (best, side) => (rooms[side] > rooms[best] ? side : best),
      "above" as BubbleSide,
    );
  }

  // 3) 默认上置
  return "above";
});

/**
 * 左右置时气泡贴气泡窗的上边还是下边。
 *
 * 跟着宠物在屏幕上的高低走：宠物偏下 → 贴下边（气泡窗底边与宠物窗底边齐平，气泡
 * 落在宠物脚边），宠物偏上 → 贴上边（两窗顶边齐平，气泡落在宠物头顶）。这样气泡
 * 始终待在宠物所在的那一段屏幕区域里，不会跟宠物分家。Rust 侧按它还决定气泡窗的
 * 纵向位置，所以贴哪边都不会把气泡顶到屏幕外。
 * 屏幕中线附近留 40px 迟滞，宠物停在中间时不会来回翻边。
 */
const ALIGN_HYSTERESIS_CSS = 40;
const bubbleAlign = ref<BubbleAlign>("top");
const refreshBubbleAlign = () => {
  if (!layoutReady.value) return;
  const delta =
    petTopCss.value + petHeightCss.value / 2 - (workTopCss.value + workBottomCss.value) / 2;
  if (bubbleAlign.value === "top") {
    // 宠物落到屏幕中线以下 → 改成靠着下边
    if (delta > ALIGN_HYSTERESIS_CSS) bubbleAlign.value = "bottom";
  } else if (delta < -ALIGN_HYSTERESIS_CSS) {
    bubbleAlign.value = "top";
  }
};

/**
 * 左右置时气泡内容相对所贴那条边的额外内缩（CSS px），镜像给气泡窗。
 *
 * 气泡窗按 `bubbleAlign` 把「内容所贴的那条边」与宠物窗的同一条边对齐，但宠物窗下半截
 * 是透明的输入带：宠物贴到屏幕上/下沿时那条边已经跑到工作区外（头像贴任务栏时窗底出屏
 * 64px），内容会被一起推出去。这里把溢出量算出来，让气泡窗把内容拉回工作区内 ——
 * 边角处气泡因此紧贴屏幕边缘，而不是飘在宠物头顶。
 */
const bubbleAlignInset = computed(() => {
  if (bubbleAlign.value === "bottom") {
    return Math.max(0, petTopCss.value + petHeightCss.value - workBottomCss.value);
  }
  return Math.max(0, workTopCss.value - petTopCss.value);
});

/** 读取宠物窗位置/尺寸与显示器工作区（移动、换屏、改缩放后调用） */
const syncBubblePlacement = async () => {
  try {
    const [pos, size, monitor] = await Promise.all([
      appWindow.outerPosition(),
      appWindow.outerSize(),
      currentMonitor(),
    ]);
    if (!monitor) return;
    const dpr = monitor.scaleFactor || window.devicePixelRatio || 1;
    petDpr = dpr;
    petLeftCss.value = pos.x / dpr;
    petTopCss.value = pos.y / dpr;
    if (size.width > 0) petWidthCss.value = size.width / dpr;
    if (size.height > 0) petHeightCss.value = size.height / dpr;
    workLeftCss.value = monitor.workArea.position.x / dpr;
    workTopCss.value = monitor.workArea.position.y / dpr;
    workRightCss.value = (monitor.workArea.position.x + monitor.workArea.size.width) / dpr;
    workBottomCss.value = (monitor.workArea.position.y + monitor.workArea.size.height) / dpr;
    layoutReady.value = true;
    refreshBubbleAlign();
  } catch {
    // 拿不到窗口/显示器信息时保持上一次判定
  }
};

let placementTimer: number | undefined;
/** 移动事件很密集：防抖后再判定，避免拖动过程中反复换位 */
const schedulePlacementSync = () => {
  if (placementTimer !== undefined) window.clearTimeout(placementTimer);
  placementTimer = window.setTimeout(() => {
    void syncBubblePlacement().then(() => applyBubblePlacement(true));
  }, 120);
};

const pushBubbleSide = async (side: BubbleSide, align: BubbleAlign) => {
  try {
    await invoke("set_bubble_side", { side, align });
  } catch (error) {
    console.error("设置气泡位置失败:", error);
  }
};

const delay = (ms: number) => new Promise((resolve) => window.setTimeout(resolve, ms));

/**
 * 换位编排：先让气泡窗淡掉内容，再挪窗口/翻版式，最后淡回来。
 * 气泡窗是独立窗口，位置变化是瞬时的 —— 不淡掉就会看到气泡"闪"到另一边。
 * 串行执行：连点设置里的选项时逐次播放，不会两轮编排互相打架。
 */
let swapQueue: Promise<void> = Promise.resolve();
const runSwap = (commit: () => Promise<void> | void) => {
  swapQueue = swapQueue.then(async () => {
    swapping.value = true;
    emitMirror();
    await delay(160);
    await commit();
    swapping.value = false;
    emitMirror();
  });
};

/**
 * 把当前判定结果落到气泡窗：方位与贴边都由 Rust 参与定位（贴边只影响左右置的纵向
 * 位置），所以两者任一变化都要重排一次窗口，再同步版式给气泡窗。
 *
 * @param animate 是否播换位淡入淡出。首次判定（进桌宠模式）不播 —— 此时气泡窗刚建、
 *                内容本来就是空的，淡出没有意义，反而会白等 160ms。
 */
const applyBubblePlacement = async (animate: boolean) => {
  const side = bubbleSide.value;
  const align = bubbleAlign.value;
  if (side === mirroredSide.value && align === mirroredAlign.value) return;
  const commit = async () => {
    await pushBubbleSide(side, align);
    mirroredSide.value = side;
    mirroredAlign.value = align;
  };
  if (!animate) {
    await commit();
    emitMirror();
    return;
  }
  runSwap(commit);
};

// ─────────────────────────── 窗口与命中区 ───────────────────────────

const applyWindowLayout = async () => {
  try {
    await invoke("set_pet_mode", { enable: true, scale: scale.value });
  } catch (error) {
    console.error("调整窗口布局失败:", error);
  }
};

let lastRectsKey = "";
const reportSolidRegions = () => {
  const rects: { x: number; y: number; width: number; height: number }[] = [];

  if (avatarContainer.value) {
    const r = avatarContainer.value.getBoundingClientRect();
    rects.push({ x: r.x, y: r.y, width: r.width, height: r.height });
  }
  // 输入框显示时略微外扩，保证极小尺寸下的判定连贯
  if (chatContainer.value && showChatInput.value) {
    const r = chatContainer.value.getBoundingClientRect();
    rects.push({ x: r.x - 20, y: r.y - 20, width: r.width + 40, height: r.height + 40 });
  }

  const rectsKey = JSON.stringify(rects);
  if (rectsKey === lastRectsKey) return;
  lastRectsKey = rectsKey;
  invoke("update_solid_regions", { rects }).catch(() => {
    lastRectsKey = "";
  });
};

const unlisteners: (() => void)[] = [];
let hitTestInterval: number | undefined;

onMounted(async () => {
  document.body.style.backgroundColor = "transparent";
  document.documentElement.style.backgroundColor = "transparent";

  // 气泡窗启动晚于本窗口，挂载后会主动请求一次当前状态
  unlisteners.push(
    await appWindow.listen(PET_BUBBLE_REQUEST, emitMirror),
    // 桌宠设置窗口的四个热更通道：设置窗口只改自己的 store + 广播，本窗口负责生效
    await appWindow.listen<{ scale: number }>("pet-scale-changed", (event) => {
      const next = Number(event.payload?.scale);
      if (!Number.isNaN(next)) settingsStore.pet.scale = next;
    }),
    await appWindow.listen<{ fps: number }>("pet-live2d-fps-changed", (event) => {
      const next = Number(event.payload?.fps);
      if (!Number.isNaN(next)) settingsStore.setPetLive2dFps(next);
    }),
    await appWindow.listen<{ volume: number }>("pet-volume-changed", (event) => {
      const next = Number(event.payload?.volume);
      if (!Number.isNaN(next)) settingsStore.updateAudio({ characterVolume: next });
    }),
    await appWindow.listen<{ effect: string }>("background-effect-changed", (event) => {
      if (event.payload?.effect) uiStore.setBackgroundEffect(event.payload.effect);
    }),
    // 气泡位置设置：设置窗只改自己的 store + 广播，本窗口负责判定与生效
    await appWindow.listen<{ side: string }>(PET_BUBBLE_SIDE_EVENT, (event) => {
      const side = event.payload?.side;
      if (
        side === "above" ||
        side === "below" ||
        side === "left" ||
        side === "right" ||
        side === "auto"
      ) {
        settingsStore.pet.bubbleSide = side;
        void syncBubblePlacement().then(() => applyBubblePlacement(true));
      }
    }),
    // 窗口移动 / 换屏：重算侧别（自动判定依赖宠物窗在屏幕上的位置）。
    // 位置**当场**按事件负载更新：侧别判定可以防抖（见 schedulePlacementSync），但气泡的
    // 纵向内缩（bubbleAlignInset）得逐帧跟着走 —— 只等防抖那 120ms 的话，拖动期间内缩一直
    // 停在旧值、松手才被拽回去，看起来一顿一顿的。
    await appWindow.onMoved(({ payload }) => {
      if (typeof payload?.x === "number") petLeftCss.value = payload.x / petDpr;
      if (typeof payload?.y === "number") petTopCss.value = payload.y / petDpr;
      schedulePlacementSync();
    }),
    // 输入框显隐兜底：光标离开 solid 区域后窗口会开启点击穿透，webview 从此收不到
    // 鼠标事件，mouseleave 可能永远不来（见 api/pet.rs::spawn_hit_test_poll）。
    await appWindow.listen<{ x: number; y: number }>("pet:cursor", (event) => {
      const { x, y } = event.payload;
      setShowChatInput(x >= 0 && y >= 0 && x <= window.innerWidth && y <= window.innerHeight);
    }),
    // 气泡窗的打字状态（自动推进调度器要用它判断打字机是否已结束）
    await appWindow.listen<{ typing: boolean }>(PET_BUBBLE_TYPING_EVENT, (event) => {
      onBubbleTyping(Boolean(event.payload?.typing));
    }),
  );

  await applyWindowLayout();

  // 先量出宠物窗在屏幕上的位置与四周余量，再决定气泡放哪边，并把方位推给 Rust
  await syncBubblePlacement();
  await applyBubblePlacement(false);
  emitMirror(); // 同步方位给气泡窗（气泡窗的首次请求可能早于这次判定）

  hitTestInterval = window.setInterval(reportSolidRegions, 100);
});

onUnmounted(() => {
  document.body.style.backgroundColor = "";
  document.documentElement.style.backgroundColor = "";
  unlisteners.forEach((unlisten) => unlisten());
  if (hitTestInterval !== undefined) window.clearInterval(hitTestInterval);
  if (placementTimer !== undefined) window.clearTimeout(placementTimer);
});

watch(scale, () => {
  void applyWindowLayout();
  schedulePlacementSync();
});

// 显示状态变化即推给气泡窗（气泡窗不参与状态机，只复现这些字段）。
// 内缩量跟着窗口位置走，拖动时属于独立变化源，必须单独作为依赖，否则可能漏推。
watch(
  () => [
    uiStore.showCharacterLine,
    uiStore.showCharacterEmotion,
    uiStore.currentAvatarAudio,
    gameStore.currentStatus,
    uiStore.notification.isVisible,
    uiStore.notification.message,
    scale.value,
    bubbleAlignInset.value,
  ],
  emitMirror,
);

// ─────────────────────────────── 交互 ───────────────────────────────

/** 光标在桌宠窗口内就显示输入框，离开则隐藏；草稿非空（正在打字）时保持显示 */
const setShowChatInput = (insideWindow: boolean) => {
  showChatInput.value = insideWindow || (ChatInputRef.value?.isTyping() ?? false);
};

const handleMouseEnter = () => setShowChatInput(true);
const handleMouseLeave = () => setShowChatInput(false);

/** 补全气泡里的打字动画（点击推进时先补全文本、不推进，与主界面一致） */
const requestFinishTyping = () => {
  void appWindow.emitTo("pet_bubble", PET_FINISH_TYPING_EVENT);
};

/**
 * 推进状态机 —— 与主界面 GameDialog 共用 `useDialogAdvance`。
 *
 * 打字机在气泡窗，`isTyping` 用镜像回来的 `bubbleTyping`，`finishTyping`
 * 通过事件让气泡窗补全 —— 桌宠无动作文本（两段式），故不传 `motion`。
 */
const { continueDialog } = useDialogAdvance({
  isTyping: computed(() => bubbleTyping.value),
  finishTyping: requestFinishTyping,
});

const {
  typingFinished,
  onAudioStarted: handleAudioStarted,
  onAudioFinished: handleAudioFinished,
  manualTriggerContinue,
  cancelAdvance,
  scheduleAdvance,
  toggleAutoMode: handleSwitchAutoMode,
} = useAutoAdvance({
  // 气泡窗只有一个受控的显示组件，没有组件句柄：用镜像的 isTyping + 共用状态机拼一个
  dialog: () => ({ isTyping: bubbleTyping.value, continueDialog }),
  mergeEnabled: false,
});

/**
 * 新台词到达：先按「还在打字」压住调度器，等气泡窗回报真实状态再放行。
 *
 * 主界面同窗口，`isTyping` 在定时器触发前必定已更新；桌宠的打字机在另一个 webview，
 * 状态要过一趟 IPC 才回来。若沿用主界面「先按未打字调度」，autoAdvanceDelay 很小时
 * 定时器会抢在气泡回报之前触发 —— 台词还没打完就跳下一句。
 * 非空台词必定触发气泡重画，气泡要么回报 true（开始打字）、要么回报 false
 * （整段复现），所以只在空台词（无气泡可画）时跳过，避免等一个不会来的回报。
 */
watch(
  () => uiStore.showCharacterLine,
  (line) => {
    if (!line) return;
    typingFinished.value = false;
    cancelAdvance();
  },
);

/** 气泡窗回报打字状态：显式写回调度器（整段复现时 isTyping 不变，内部 watch 不触发） */
const onBubbleTyping = (typing: boolean) => {
  bubbleTyping.value = typing;
  typingFinished.value = !typing;
  if (typing) cancelAdvance();
  else scheduleAdvance();
};

/** 推进对话：先取消待调度，再走共用状态机（打字中先补全、不推进） */
const handleAvatarClick = () => {
  manualTriggerContinue();
  continueDialog(true);
};

const handleOpenSettings = async () => {
  try {
    const existing = await WebviewWindow.getByLabel("settings");
    if (existing) {
      await existing.setFocus();
      return;
    }

    new WebviewWindow("settings", {
      url: "/second",
      title: t("views.petMode.settingsWindowTitle"),
      width: 1200,
      height: 800,
      resizable: true,
      shadow: false,
      decorations: false,
      transparent: true,
      alwaysOnTop: false,
    }).once("tauri://error", (e) => console.error("创建设置窗口失败:", e));
  } catch (error) {
    console.error("打开设置窗口时出错:", error);
  }
};

// 自动推进与语音收尾仍在本窗口调度；对话框句柄由镜像的打字状态拼出（见上）

const handleExitPetMode = async () => {
  try {
    const settingsWindow = await WebviewWindow.getByLabel("settings");
    if (settingsWindow) await settingsWindow.close();
  } catch {
    // 窗口不存在，忽略
  }

  await invoke("update_solid_regions", { rects: [] });
  await invoke("set_pet_mode", { enable: false });
  router.push("/chat");
};

watch(
  () => gameStore.dialogHistory.length,
  () => {
    appWindow.emit("dialog-history-changed", {
      dialogHistory: JSON.parse(JSON.stringify(gameStore.dialogHistory)),
    });
  },
);
</script>

<style scoped>
#pet-app {
  position: relative;
  width: 100vw;
  height: 100dvh;
  overflow: hidden;
}
</style>
