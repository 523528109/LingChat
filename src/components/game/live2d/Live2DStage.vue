<template>
  <div
    v-bind="$attrs"
    ref="host"
    class="pointer-events-none absolute inset-0 overflow-hidden"
    aria-hidden="true"
  >
    <!-- 抚摸粒子放在画布容器内部：它要盖住模型却压在气泡之下，而 PIXI 的画布是
         运行时才追加到这个 div 末尾的，只能靠 z-1 压过它那个 auto -->
    <TouchParticles ref="touchParticles" />
  </div>
  <slot></slot>
</template>

<script setup lang="ts">
import { convertFileSrc } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { onBeforeUnmount, onMounted, provide, readonly, ref, watch } from "vue";

import { getLive2dFilePath, getLive2dVariantAssets } from "@/api/services/character";
import { EMOTION_CONFIG_EMO } from "@/controllers/emotion/config";
import type { GameRole } from "@/stores/modules/game/state";
import { useSettingsStore } from "@/stores/modules/settings";
import {
  prefersLive2d,
  resolveLive2dVariant,
  type Live2dMotionBinding,
  type Live2dVariant,
  type Live2dVariantAssets,
} from "@/types/live2d";
import {
  areEyesOpen,
  gazeFromPointer,
  GAZE_MAGNITUDE_MIN,
  radialReferenceDistance,
  type ScreenBox,
} from "./live2d-interaction";
import { live2dStageContextKey } from "./live2d-stage-context";
import { calculatePetLayout } from "./live2d-layout";
import { trackMotionLifecycle } from "./live2d-motion";
import { loadLive2dRuntime, type Live2dRuntime } from "./live2d-runtime";
import TouchParticles from "./TouchParticles.vue";
import {
  createStrokeTracker,
  hitTouchPart,
  resolveTouchRegions,
  type TouchBounds,
  type TouchRegion,
} from "./live2d-touch";
import {
  configureRuntimeIdle,
  mergeVariantAssets,
  rewriteModelReferences,
  type Live2dModelSource,
} from "./model-source";
import { decodeVoiceForLipSync, sampleVoiceAmplitude, type DecodedVoice } from "./useLive2dLipSync";

defineOptions({ inheritAttrs: false });

const props = defineProps<{
  roles: GameRole[];
  mode: "standard" | "pet";
  activeSpeakerId: number | null;
  audioElement: HTMLAudioElement | null;
  voiceDataUrl: string;
  /** 投屏全局缩放：乘在角色基础 scale 上，作用于 Live2D 模型本体
      （标准模式下仅投屏窗口传入，主窗口缺省为 1，无影响） */
  castScale?: number;
  /** 投屏全局垂直偏移（像素，正值下移；标准模式下仅投屏窗口传入，主窗口缺省 0）。
      水平偏移由投屏窗口 .cast-role-layer 的 CSS translateX 整层平移，不在此处理。 */
  castOffsetY?: number;
  /** 渲染帧率上限（0 = 不限制）。桌宠窗口很小，30fps 足够且大幅降低挂机 CPU；
      仅桌宠舞台（pet/GameRolesStage）传入，标准模式/预览不传保持原行为 */
  maxFps?: number;
  /** 抚摸交互开关：标准模式下处于「触摸模式」时由 GameRolesStage 传 true。
      缺省 false 是刻意的 —— 设置界面的预览用同一个组件挂 mode="standard"，
      不传此 prop 就自动免疫，不必再单设一个预览开关。 */
  touchEnabled?: boolean;
}>();

const emit = defineEmits<{
  activeChange: [roleIds: number[]];
  failedChange: [roleIds: number[]];
}>();

interface CursorPayload {
  x: number;
  y: number;
  /** 当前显示器工作区，已按与 x/y 相同的公式换算到窗口相对逻辑像素。
      旧版 Rust 载荷没有这个字段（undefined），取不到显示器信息时为 null。 */
  screen?: ScreenBox | null;
}

interface RoleModel {
  roleId: number;
  variantName: string;
  model: any;
  variant: Live2dVariant;
  runtimeIdle: Live2dMotionBinding | null;
  /** 当前表情态，存的是**原始**情绪词（分类器/剧本产出的那个）。它是状态变化本身的
      标识，也是查绑定的第一优先键；用 `EMOTION_CONFIG_EMO` 的映射词去重会让
      哭泣与伤心、难为情与羞耻各塌成同一个键，切换时动作不会重放。 */
  emotion: string;
  requestId: number;
  mouthParameterIndex: number;
  mouthValue: number;
  eyeLeftParameterIndex: number;
  eyeRightParameterIndex: number;
  eyeBallXParameterIndex: number;
  eyeBallYParameterIndex: number;
  eyesOpen: boolean;
  focusFrozen: boolean;
  /** 视线幅度：锚点到鼠标的距离 ÷ 该方向上锚点到屏幕边缘的距离，已夹到
      [GAZE_MAGNITUDE_MIN, 1]。1 表示不衰减（维持旧行为）。 */
  gazeMagnitude: number;
  /** 视线原点在模型局部坐标系里的位置，首次用到时由 drawable bounds 与
      focus_anchor 算出后缓存——bounds 随呼吸/动作漂移，每帧重算会让锚点抖动。 */
  focusOrigin: { x: number; y: number } | null;
  /** 抚摸命中区域（归一化到 drawable bounds）与推导它所用的 bounds。
      与 focusOrigin 在同一次 getLocalBounds() 里算出并一起缓存——bounds 随呼吸/
      动作漂移，分两次读会让锚点与区域互相错位。无绑定或无锚点时为 null。 */
  touchRegions: Record<string, TouchRegion> | null;
  touchBounds: TouchBounds | null;
  reactionSequence: number;
  reactionLifecycleCleanup: (() => void) | null;
}

const host = ref<HTMLDivElement | null>(null);
const touchParticles = ref<InstanceType<typeof TouchParticles> | null>(null);
let runtime: Live2dRuntime | null = null;
let application: any = null;
let disposed = false;
let syncPromise = Promise.resolve();
let requestSequence = 0;
let decodedVoice: DecodedVoice | null = null;
let decodeSequence = 0;
let resizeObserver: ResizeObserver | null = null;
let pointerPosition: { clientX: number; clientY: number } | null = null;
/** 当前显示器工作区（窗口相对逻辑像素），由 Rust 侧的 pet:cursor 广播带过来。
    前端自己读 window.screenX/availLeft 在混合 DPI 多显示器下会混用设备像素与
    CSS 像素；尺寸可靠但位置不可靠，所以位置必须跟指针走同一个来源。 */
let screenBox: ScreenBox | null = null;
let cursorUnlisten: (() => void) | null = null;
/** 抚摸的晃动方向。松手后仍由它衰减回正，所以它先于角色存在。 */
const stroke = createStrokeTracker();
/** 当前归抚摸管的角色。松手后仍归它管，直到晃动回正才交还，否则头会卡在半路。 */
let swayRoleId: number | null = null;
/** 正被抚摸表情占着的角色。它比晃动活得久：要等动作也播完才把表情收回去。 */
let touchExpressionRoleId: number | null = null;
/** 抚摸音效走到音阶的哪一级，每次重新摸都从根音起 */
let petSoundStep = 0;
/** 距上一声过去了多久，毫秒 */
let petSoundElapsed = 0;
/** 闭眼权重，1 为完全闭合。只作用在被摸的那个角色身上。 */
let eyeCloseWeight = 0;
/** 本次抚摸闭眼持续到什么时候（时间戳）。松开就作废，剩下的时间不再补。 */
let eyesClosedUntil = 0;
/** 触摸模式下的视线权重：进入触摸模式后趋向 1、退出后衰减回 0。
    整块功能按模式而不是按手势生效，所以要有一个比单次抚摸活得久的量。 */
let gazeWeight = 0;
let gazeFrameAt = 0;
/** 抚摸结束后待播的动作：等晃动回正再起，免得整个动作顶着最后那点偏角播完。 */
let pendingTouch: { roleId: number; part: string; since: number } | null = null;
const models = new Map<number, RoleModel>();
const failedRoleIds = new Set<number>();
const readyRoleIds = ref<ReadonlySet<number>>(new Set());
const unavailableRoleIds = ref<ReadonlySet<number>>(new Set());

provide(live2dStageContextKey, {
  readyRoleIds: readonly(readyRoleIds),
  unavailableRoleIds: readonly(unavailableRoleIds),
});

function emitFailedRoles() {
  const roleIds = [...failedRoleIds];
  unavailableRoleIds.value = new Set(roleIds);
  emit("failedChange", roleIds);
}

function emitActiveRoles() {
  const roleIds = [...models.keys()];
  readyRoleIds.value = new Set(roleIds);
  emit("activeChange", roleIds);
}

function motionBindingEquals(
  left: Live2dMotionBinding | null | undefined,
  right: Live2dMotionBinding | null | undefined,
) {
  if (!left || !right) return left == null && right == null;
  return (
    left.group === right.group &&
    left.index === right.index &&
    (left.loop ?? true) === (right.loop ?? true)
  );
}

/**
 * 查情绪绑定用的键，按优先级排列。
 *
 * 第一项是分类器与剧本产出的原始情绪词（见 `data/third_party/emotion_model_19emo/label_mapping.json`），
 * 也正是设置界面写进 `settings.yml` 的键，所以它必须先命中。
 *
 * 第二项是 `EMOTION_CONFIG_EMO` 的映射词。那张表是给静态立绘挑气泡图和音效用的
 * （哭泣 → 伤心.webp），Live2D 这里带上它只是让已经写成映射词的配置不回归。少了第一项
 * 会让「哭泣」「难为情」两行变成死键——设置界面绑得上，运行时永远查不到。
 *
 * 表外情绪（如剧本里的「尴尬」）映射词就是「正常」，与映射表出现前的行为一致。
 */
function emotionBindingKeys(emotion: string): string[] {
  const mapped = EMOTION_CONFIG_EMO[emotion] || "正常";
  return emotion === mapped ? [emotion] : [emotion, mapped];
}

/**
 * 按上述顺序取第一个「存在」的绑定。
 *
 * 判空用 `!== undefined` 而不是真值判断：设置界面的「无表情」选项把值写成空串，
 * 那表示用户显式关掉了这个情绪的表情，不能穿透到下一级——这正是 Live2D 文档里
 * 「只在绑定缺失时才回退到 default_expression」的意思。
 */
function pickEmotionBinding<T>(table: Record<string, T>, emotion: string): T | undefined {
  for (const key of emotionBindingKeys(emotion)) {
    const value = table[key];
    if (value !== undefined) return value;
  }
  return undefined;
}

function variantNameFor(role: GameRole): string | null {
  const settings = role.live2d;
  if (!settings || !prefersLive2d(role, props.mode)) return null;
  const clothes = !role.clothesName || role.clothesName === "默认" ? "default" : role.clothesName;
  const mapped = settings.clothes_variants[clothes];
  return mapped || settings.default_variant;
}

async function loadModelSource(
  roleId: number,
  modelFile: string,
  assets: Promise<Live2dVariantAssets | null>,
) {
  const modelPath = await getLive2dFilePath(roleId, modelFile);
  const modelUrl = convertFileSrc(modelPath);
  const response = await fetch(modelUrl);
  if (!response.ok) throw new Error(`Failed to load Live2D settings: HTTP ${response.status}`);
  const source = (await response.json()) as Live2dModelSource;
  // 注入必须夹在解析与改写之间：rewriteModelReferences 会把 FileReferences 里的相对路径
  // 就地转成文件 URL，比它晚注入的路径永远不会被转换，引擎会拿到裸相对路径去取资源
  mergeVariantAssets(source, await assets);
  await rewriteModelReferences(source, modelFile, async (relative) => {
    return convertFileSrc(await getLive2dFilePath(roleId, relative));
  });
  source.url = modelUrl;
  return source;
}

function destroyApplication() {
  resizeObserver?.disconnect();
  resizeObserver = null;
  if (application) {
    application.destroy({ removeView: true, releaseGlobalResources: false }, true);
    application = null;
  }
  pointerPosition = null;
  runtime = null;
}

async function ensureApplication() {
  if (application || !host.value || disposed) return;
  runtime = await loadLive2dRuntime();
  const app = new runtime.pixi.Application();
  await app.init({
    resizeTo: host.value,
    preference: "webgl",
    backgroundAlpha: 0,
    antialias: true,
    autoDensity: true,
    resolution: Math.min(window.devicePixelRatio, props.mode === "pet" ? 1.5 : 2),
  });
  if (disposed || !host.value) {
    app.destroy({ removeView: true, releaseGlobalResources: false }, true);
    return;
  }
  app.canvas.className = "absolute inset-0 w-full h-full";
  host.value.appendChild(app.canvas);
  app.ticker.speed = 1.35;
  // 帧率上限：0/undefined 视为不限帧（保持标准模式/预览原行为）
  const fpsCap = props.maxFps ?? 0;
  if (fpsCap > 0) app.ticker.maxFPS = fpsCap;
  app.ticker.add(updateLipSync);
  app.ticker.add(updateTouchFrame);
  resizeObserver = new ResizeObserver(() => {
    for (const entry of models.values()) {
      const role = props.roles.find((item) => item.roleId === entry.roleId);
      if (role) applyLayout(entry, role);
    }
  });
  resizeObserver.observe(host.value);
  application = app;
}

function findParameterIndex(entry: RoleModel, parameter: string): number {
  const core = entry.model.internalModel.coreModel;
  for (let index = 0; index < core.getParameterCount(); index += 1) {
    if (core.getParameterId(index).isEqual(parameter)) return index;
  }
  return -1;
}

/** 工作区矩形缺失时（移动端、浏览器 dev）的径向参考距离。
    window.screen 的尺寸在多 DPI 下可靠、位置不可靠，所以只取尺寸；
    连尺寸都拿不到时返回 0，gazeFromPointer 据此退回「不衰减」。 */
function fallbackReferenceDistance() {
  const screen = window.screen;
  return radialReferenceDistance(screen?.availWidth ?? 0, screen?.availHeight ?? 0);
}

/** 一次性解析并缓存模型局部几何：视线原点与抚摸命中区域。
 *
 * 视线原点取 drawable bounds 上的 focus_anchor，没配则取 bounds 中心。两处必须在
 * 同一次 getLocalBounds() 里算出：bounds 随呼吸与动作漂移，分两次读会让摸到的
 * 地方和角色看的方向对不上。只算一次缓存也是这个原因，局部 bounds 不受 model 的
 * 缩放与位移影响，桌宠改缩放不会让它失效。
 *
 * 抚摸区域用的是 variant 上的原始锚点，不用回落到 0.5 之后的那个。回落后头区会
 * 落在躯干正中，所以宁可一个区域都不生成。
 */
function resolveLocalGeometry(entry: RoleModel) {
  if (entry.focusOrigin && entry.touchBounds) return entry.focusOrigin;
  const bounds = entry.model.getLocalBounds();
  const minX = bounds.minX ?? bounds.x ?? 0;
  const minY = bounds.minY ?? bounds.y ?? 0;
  const anchor = entry.variant.focus_anchor ?? null;
  entry.focusOrigin = {
    x: minX + bounds.width * (anchor?.x ?? 0.5),
    y: minY + bounds.height * (anchor?.y ?? 0.5),
  };
  entry.touchBounds = { minX, minY, width: bounds.width, height: bounds.height };
  entry.touchRegions = resolveTouchRegions(anchor, entry.variant.touch_motions);
  return entry.focusOrigin;
}

/** 视线原点换算到视口坐标。取不到舞台几何时返回 null。
    反算用 application.screen 而非 rect 做分母——PIXI 的 ResizePlugin 读
    clientWidth，application.screen 不受 CSS transform 影响，而 rect 会
    （桌宠入场有 scale(0.8→1) 动画，期间两者差 0.8 倍）。 */
function focusOriginViewport(entry: RoleModel): { x: number; y: number } | null {
  if (!host.value || !application) return null;
  const rect = host.value.getBoundingClientRect();
  const stage = application.screen;
  if (rect.width <= 0 || rect.height <= 0 || stage.width <= 0 || stage.height <= 0) return null;
  const origin = entry.model.toGlobal(resolveLocalGeometry(entry));
  return {
    x: rect.left + origin.x * (rect.width / stage.width),
    y: rect.top + origin.y * (rect.height / stage.height),
  };
}

/** 指针相对该角色视线原点的方向（单位向量，已按 Live2D 约定取反 y）。
    抚摸期间用它驱动瞳孔追踪，与桌宠模式的视线用的是同一套算法。 */
function pointerGazeDirection(entry: RoleModel) {
  if (!pointerPosition) return null;
  const anchor = focusOriginViewport(entry);
  if (!anchor) return null;
  return gazeFromPointer(
    { x: pointerPosition.clientX, y: pointerPosition.clientY },
    anchor,
    screenBox,
    screenBox ? 0 : fallbackReferenceDistance(),
  );
}

function updateModelFocus(entry: RoleModel) {
  if (entry.focusFrozen) return;
  const focusController = entry.model.internalModel.focusController;
  // 标准聊天模式：默认直视前方；抚摸期间把焦点通道临时交给手，松手后由晃动
  // 自行衰减回正，再交还。桌宠模式才用指针驱动视线。
  if (props.mode !== "pet") {
    const ownsSway = swayRoleId === entry.roleId;
    focusController.focus(ownsSway ? stroke.sway.x : 0, ownsSway ? stroke.sway.y : 0);
    return;
  }
  // 眨眼/隐藏期间冻结视线目标：不重置回中。引擎的眨眼控制器会在
  // beforeModelUpdate 之前把眼部参数写成闭眼值，此时若走回中分支，
  // 弹簧插值会把瞳孔/头短暂拽向正中，表现为眨眼瞬间“瞬视中间”。
  // 下面每条早退分支都保持 gazeMagnitude 不变：焦点弹簧自己会衰减到 0，
  // 瞳孔补偿量随之归零，路径连续；清零反而会漏掉补偿、多出一个小跳变。
  if (!entry.eyesOpen || !entry.model.visible) return;
  // 全程在视口坐标里算距离：指针与工作区矩形都在这个坐标系
  const anchor = focusOriginViewport(entry);
  if (!pointerPosition || !anchor) {
    focusController.focus(0, 0);
    return;
  }
  const gaze = gazeFromPointer(
    { x: pointerPosition.clientX, y: pointerPosition.clientY },
    anchor,
    screenBox,
    screenBox ? 0 : fallbackReferenceDistance(),
  );
  // 方向按单位向量交给引擎驱动瞳孔；幅度只用来衰减头部旋转，
  // 被缩掉的瞳孔偏转由 beforeModelUpdate 补回。
  const magnitude = Math.max(gaze.magnitude, GAZE_MAGNITUDE_MIN);
  entry.gazeMagnitude = magnitude;
  focusController.focus(gaze.x * magnitude, gaze.y * magnitude);
}

function handlePointerMove(event: PointerEvent) {
  pointerPosition = { clientX: event.clientX, clientY: event.clientY };
}

/** 视口坐标换算到模型局部坐标，与 updateModelFocus 的映射互为逆运算。
    分母同样取 application.screen 而不是 rect，理由见那里的注释。
    toLocal 与 getLocalBounds 同属一套 PIXI 局部空间，结果可直接与区域矩形比较。 */
function localPointFor(entry: RoleModel, clientX: number, clientY: number) {
  if (!host.value || !application) return null;
  const rect = host.value.getBoundingClientRect();
  const stage = application.screen;
  if (rect.width <= 0 || rect.height <= 0 || stage.width <= 0 || stage.height <= 0) return null;
  return entry.model.toLocal({
    x: (clientX - rect.left) * (stage.width / rect.width),
    y: (clientY - rect.top) * (stage.height / rect.height),
  });
}

/** 指针落在哪个角色的哪个部位。多角色站位重叠时取舞台层级最高的那个。 */
function partAtPoint(clientX: number, clientY: number): { entry: RoleModel; part: string } | null {
  if (!application) return null;
  const candidates = [...models.values()]
    .filter((entry) => entry.model.visible && entry.variant.touch_motions)
    // 用 children.indexOf 而不是 getChildIndex：后者在模型不在舞台上时会抛错
    .sort(
      (a, b) =>
        application.stage.children.indexOf(b.model) - application.stage.children.indexOf(a.model),
    );
  for (const entry of candidates) {
    // 几何只在首次用到时算并缓存。标准模式的视线路径不会走到 resolveLocalGeometry，
    // 所以这里必须自己触发，否则区域永远是空的
    resolveLocalGeometry(entry);
    const local = localPointFor(entry, clientX, clientY);
    if (!local || !entry.touchBounds || !entry.touchRegions) continue;
    const part = hitTouchPart(local, entry.touchBounds, entry.touchRegions);
    if (part) return { entry, part };
  }
  return null;
}

/** 窗口级监听的守卫：抚摸挂在 window 上，会连带吃到输入框选词、按钮按下、对话框
    拖拽。画布是 pointer-events-none，所以 elementFromPoint 命中的是该处真正的 UI
    元素。这是启发式黑名单，不是完备解，漏网的 UI 要往选择器里补。 */
const INTERACTIVE_SELECTOR =
  "input, textarea, select, button, a, [contenteditable='true'], .game-dialog";

function isInteractiveTarget(clientX: number, clientY: number): boolean {
  return !!document.elementFromPoint(clientX, clientY)?.closest(INTERACTIVE_SELECTOR);
}

/** 沿途粒子的间距（CSS 像素）。按走过的距离撒而不是按时间撒，手停下就不再冒，
    而且速度不同也不会时密时疏。 */
const PARTICLE_TRAIL_DISTANCE = 46;

/** 摸头音效的播放间隔，毫秒。 */
const PET_SOUND_INTERVAL_MS = 300;

/**
 * 音阶用的是五声音阶的几个音，而且是来回走不是循环回根音，听感像轻轻荡着。
 * 用 playbackRate 变调：只改音高，不改音色，也就不必准备多个音频文件。
 * 跨度压在一个五度内，避免连着摸时越摸越尖。
 */
const PET_SOUND_RATES = [1, 1.1225, 1.2599, 1.4983, 1.2599, 1.1225];

const PET_SOUND_URL = `${import.meta.env.BASE_URL}audio/pet.mp3`;

/** 一次抚摸闭眼保持多久，毫秒。松手提前结束，不把这 2 秒补完。 */
const EYE_CLOSE_HOLD_MS = 2000;

/** 睁眼与闭眼的缓动时长，毫秒。 */
const EYE_LID_MS = 160;

/** 放一声抚摸音效。每次新建一个播放器，因为上一声多半还没放完，
    同一个元素重复 play 会把前一声掐断，听不出音阶的层叠。
    音量挂在气泡音效上，它就是这个项目里短音效的那条总线。 */
function playPetSound() {
  const voice = new Audio(PET_SOUND_URL);
  voice.volume = useSettingsStore().bubbleVolume / 100;
  voice.playbackRate = PET_SOUND_RATES[petSoundStep % PET_SOUND_RATES.length]!;
  petSoundStep += 1;
  // 浏览器可能因为还没有用户手势而拒绝播放，静默跳过这一声就是了
  void voice.play().catch(() => {});
}

let trailX = 0;
let trailY = 0;
let trailTravelled = 0;

function handleStrokeDown(event: PointerEvent) {
  if (disposed || event.button !== 0) return;
  // 标准模式没有桌宠那条 pointermove 监听，指针位置得由抚摸自己维护，
  // 否则瞳孔追踪拿不到位置、整块逻辑静默失效
  pointerPosition = { clientX: event.clientX, clientY: event.clientY };
  if (isInteractiveTarget(event.clientX, event.clientY)) return;
  const hit = partAtPoint(event.clientX, event.clientY);
  const accepted = stroke.begin(hit?.part ?? null, event.clientX, event.clientY, performance.now());
  if (accepted && hit) {
    swayRoleId = hit.entry.roleId;
    pendingTouch = null; // 新的一次抚摸作废上一次还没播出去的动作
    applyTouchExpression(hit.entry, hit.part);
    trailX = event.clientX;
    trailY = event.clientY;
    trailTravelled = 0;
    // 第一声立刻响，音阶也从根音重新起，每次摸的听感都一致
    petSoundStep = 0;
    petSoundElapsed = 0;
    playPetSound();
    eyesClosedUntil = performance.now() + EYE_CLOSE_HOLD_MS;
  }
}

function handleStrokeMove(event: PointerEvent) {
  // 先记位置再判早退：松手后视线还要淡出一小会儿，那期间指针可能已经移开了
  pointerPosition = { clientX: event.clientX, clientY: event.clientY };
  // 未处于抚摸中时，绝大多数 mousemove 都在这一行返回，不碰几何计算
  if (!stroke.active) return;
  const hit = partAtPoint(event.clientX, event.clientY);
  trailTravelled += Math.hypot(event.clientX - trailX, event.clientY - trailY);
  trailX = event.clientX;
  trailY = event.clientY;
  // 只在仍落在可摸部位上时冒粒子：拖到模型外就不该再撒了
  if (hit && trailTravelled >= PARTICLE_TRAIL_DISTANCE) {
    trailTravelled = 0;
    touchParticles.value?.spawn(event.clientX, event.clientY);
  }
  stroke.move(hit?.part ?? null, event.clientX, event.clientY, performance.now());
}

function handleStrokeRelease() {
  if (!stroke.active) return;
  const { part, stroked } = stroke.end();
  if (stroked && part && swayRoleId !== null) {
    pendingTouch = { roleId: swayRoleId, part, since: performance.now() };
  }
}

let touchListenersAttached = false;

function attachTouchListeners() {
  if (touchListenersAttached) return;
  touchListenersAttached = true;
  window.addEventListener("pointerdown", handleStrokeDown, { passive: true });
  window.addEventListener("pointermove", handleStrokeMove, { passive: true });
  window.addEventListener("pointerup", handleStrokeRelease, { passive: true });
  // 抬手不一定到得了：拖动中窗口失焦、或指针被浏览器收走时只有 pointercancel/blur，
  // 缺了这两条 active 会永久为 true，之后每次 mousemove 都白做一遍命中测试
  window.addEventListener("pointercancel", handleStrokeRelease, { passive: true });
  window.addEventListener("blur", handleStrokeRelease);
}

function detachTouchListeners() {
  if (!touchListenersAttached) return;
  touchListenersAttached = false;
  window.removeEventListener("pointerdown", handleStrokeDown);
  window.removeEventListener("pointermove", handleStrokeMove);
  window.removeEventListener("pointerup", handleStrokeRelease);
  window.removeEventListener("pointercancel", handleStrokeRelease);
  window.removeEventListener("blur", handleStrokeRelease);
  stroke.cancel();
  pendingTouch = null;
  // 离开触摸模式时表情得立刻收回去，否则它会一直挂在角色脸上
  clearTouchExpression();
}

/** 晃动衰减到这个幅度以下就让动作开演。此时头还剩不到两度的偏角，看不出来，
    但比等它彻底归零要快得多。 */
const PENDING_SWAY_THRESHOLD = 0.06;

/** 上面的兜底上限。这只是给动作排期，不是用计时器去猜动作播完没有。 */
const PENDING_MOTION_TIMEOUT_MS = 900;

/** 进触摸模式时视线转向玩家的淡入时长，毫秒。 */
const GAZE_FADE_MS = 180;

/** 每帧推进晃动回正、收抚摸表情、淡入淡出视线权重，并在回正之后起待播的动作。 */
function updateTouchFrame() {
  const now = performance.now();
  // dt 每帧都要推进，否则空闲一段时间之后的第一帧会拿到一个陈旧的 dt
  const dt = Math.min(64, Math.max(0, now - gazeFrameAt));
  gazeFrameAt = now;

  const wantGaze = isStrokeEnabled() ? 1 : 0;
  const idle =
    !stroke.active &&
    !pendingTouch &&
    swayRoleId === null &&
    touchExpressionRoleId === null &&
    gazeWeight === wantGaze &&
    eyeCloseWeight === 0;
  if (idle) return;

  gazeWeight += (wantGaze - gazeWeight) * (1 - Math.exp(-dt / GAZE_FADE_MS));
  // 收敛到目标就定住，免得留下一个永远衰减不掉的尾巴
  if (Math.abs(wantGaze - gazeWeight) < 0.002) gazeWeight = wantGaze;

  stroke.update(now);

  // 抚摸音效。要求还落在可摸部位上，手滑出模型就停，与粒子的判定一致。
  // dt 已经被夹在 64ms 内，所以卡顿之后最多补一声，不会突然炸出一串。
  if (stroke.active && stroke.part !== null) {
    petSoundElapsed += dt;
    if (petSoundElapsed >= PET_SOUND_INTERVAL_MS) {
      petSoundElapsed -= PET_SOUND_INTERVAL_MS;
      playPetSound();
    }
  } else {
    petSoundElapsed = 0;
  }

  // 抚摸时闭眼，按满 2 秒或者到松手为止，两侧都走缓动
  const wantClosed = stroke.active && stroke.part !== null && now < eyesClosedUntil;
  const lidTarget = wantClosed ? 1 : 0;
  if (eyeCloseWeight !== lidTarget) {
    // 按时间线性步进而不是指数逼近：指数永远到不了目标，权重就永远不等于 0，
    // 上面那条空闲判定再也无法成立，帧循环会一直空转下去
    const step = dt / EYE_LID_MS;
    eyeCloseWeight =
      lidTarget > eyeCloseWeight
        ? Math.min(lidTarget, eyeCloseWeight + step)
        : Math.max(lidTarget, eyeCloseWeight - step);
  }
  // 回正之后才把焦点通道交还，中途交还头会跳一下
  if (!stroke.active && stroke.settled) swayRoleId = null;
  if (pendingTouch) {
    const entry = models.get(pendingTouch.roleId);
    if (!entry) {
      pendingTouch = null;
    } else {
      const settledEnough = stroke.magnitude < PENDING_SWAY_THRESHOLD;
      if (settledEnough || now - pendingTouch.since >= PENDING_MOTION_TIMEOUT_MS) {
        const { part } = pendingTouch;
        pendingTouch = null;
        playTouchReaction(entry, part);
      }
    }
  }
  // 表情比晃动活得久：等手松开、晃动回正、并且动作也播完了才收回去。动作还没起时
  // 后两个条件同样成立，所以只绑了表情没绑动作的部位也能自然收回。
  // 必须带 !stroke.active：按住不动时晃动会衰减到回正，否则表情会在还在摸的时候就没了
  if (touchExpressionRoleId !== null && !stroke.active && stroke.settled) {
    const entry = models.get(touchExpressionRoleId);
    if (!entry) {
      touchExpressionRoleId = null;
    } else if (!entry.reactionLifecycleCleanup) {
      clearTouchExpression();
    }
  }
}

/** 把抚摸表情换回当前情绪的表情，并解除登记。 */
function clearTouchExpression() {
  const entry = touchExpressionRoleId === null ? undefined : models.get(touchExpressionRoleId);
  touchExpressionRoleId = null;
  if (entry) applyExpression(entry, emotionExpression(entry));
}

function destroyModel(model: any) {
  // Textures loaded through Pixi Assets are shared across stages and owned by the global cache.
  model.destroy({ children: true, texture: false, baseTexture: false });
}

function applyLayout(entry: RoleModel, role: GameRole) {
  if (!application || !host.value) return;
  const model = entry.model;
  const bounds = model.getLocalBounds();
  const width = bounds.width || model.internalModel.width || model.width || 1;
  const height = bounds.height || model.internalModel.height || model.height || 1;
  if (props.mode === "pet") {
    const layout = calculatePetLayout(
      application.screen,
      { width, height },
      role.scaleP || 1,
      role.offsetXP || 0,
      role.offsetYP || 0,
    );
    model.anchor.set(layout.anchorX, layout.anchorY);
    model.scale.set(layout.scale);
    model.position.set(layout.x, layout.y);
  } else {
    const index = props.roles.findIndex((item) => item.roleId === role.roleId);
    const count = props.roles.length;
    const xPercent = index < 0 ? 0.5 : (index + 1) / (count + 1);
    // 投屏 scale 折进原公式里的 roleScale（与 model.scale / position.y 同步相乘，
    // 保持贴底定位）；主窗口缺省 castScale=1，行为与原先完全一致
    const roleScale = (role.scale || 1) * (props.castScale ?? 1);
    const baseScale = application.screen.height / height;
    model.anchor.set(0.5, 1);
    model.scale.set(baseScale * roleScale);
    // 投屏垂直偏移（castOffsetY，正值下移）折进角色脚底位置，但只夹紧「投屏自己下移
    // 的那段」：角色自身配置的 role.offsetY 不参与夹紧，保持原语义。脚底默认在
    // H*roleScale + role.offsetY；投屏下移最多补到窗口底沿（脚底触底即止），人物下方
    // 不会被窗口 overflow:hidden 截断；上移（负值）自由。水平偏移由投屏窗口的
    // .cast-role-layer CSS translateX 整层平移（见 CastWindow.vue）。
    const defaultFootY = application.screen.height * roleScale + (role.offsetY || 0);
    const downLimit = application.screen.height - defaultFootY;
    const castOffsetY = props.castOffsetY ?? 0;
    const effectiveOffsetY =
      castOffsetY > 0 ? Math.min(castOffsetY, Math.max(0, downLimit)) : castOffsetY;
    model.position.set(
      application.screen.width * xPercent + (role.offsetX || 0),
      defaultFootY + effectiveOffsetY,
    );
  }
  model.visible = role.show;
}

function startIdle(entry: RoleModel) {
  if (!entry.runtimeIdle || !runtime) return;
  const idle = entry.runtimeIdle;
  void entry.model.motion(idle.group, idle.index, runtime.engine.MotionPriority.IDLE, {
    loop: idle.loop ?? true,
    resetExpression: false,
  });
}

function freezeModelFocus(entry: RoleModel) {
  const focusController = entry.model.internalModel.focusController;
  focusController.focus(focusController.x, focusController.y, true);
  entry.focusFrozen = true;
}

function finishReaction(entry: RoleModel, sequence: number) {
  if (sequence !== entry.reactionSequence) return;
  entry.reactionLifecycleCleanup?.();
  entry.reactionLifecycleCleanup = null;
  entry.focusFrozen = false;
  updateModelFocus(entry);
}

/** 启动一次 FORCE 优先级的动作反应，并接管它的完成回调。
 *
 * 步骤顺序由 docs/live2d/development.md 的 Reaction Completion 钉死，情绪反应与
 * 抚摸反应共用这一份实现，不能各写一份，也不能改成超时或强制写参数，那会绕开
 * 引擎状态机，在变长与被别的动作打断时失效。
 */
function startReaction(entry: RoleModel, binding: Live2dMotionBinding) {
  if (!runtime) return;
  const sequence = ++entry.reactionSequence;
  freezeModelFocus(entry);
  entry.reactionLifecycleCleanup?.();
  entry.reactionLifecycleCleanup = trackMotionLifecycle(
    entry.model.internalModel.motionManager,
    binding.group,
    binding.index,
    runtime.engine.MotionPriority.FORCE,
    () => finishReaction(entry, sequence),
  );
  void entry.model
    .motion(binding.group, binding.index, runtime.engine.MotionPriority.FORCE, {
      loop: binding.loop ?? false,
      resetExpression: false,
    })
    .then((started: boolean) => {
      if (!started) finishReaction(entry, sequence);
    })
    .catch((error: unknown) => {
      finishReaction(entry, sequence);
      console.warn(`[Live2D] motion failed for role ${entry.roleId}`, error);
    });
}

/** 播放某个部位绑定的抚摸动作。绑定里没给动作就什么都不做，晃动本身已经发生过了。
 *
 * 已有反应在跑时不打断，一是保住剧本的情绪节拍，二是顺带当冷却用。
 *
 * 优先级必须是 FORCE。引擎对不高于当前优先级的请求直接拒绝，用 NORMAL 的话会被
 * 已经在播的待机动作直接拒掉。
 */
function playTouchReaction(entry: RoleModel, part: string): boolean {
  const binding = entry.variant.touch_motions?.[part];
  const { group, index } = binding ?? {};
  if (!binding || group === undefined || index === undefined || !runtime) return false;
  if (entry.reactionLifecycleCleanup) {
    // 反应在飞时静默丢弃。若这条日志持续出现而画面不动，说明上一个反应的
    // motionFinish 没回来，焦点被冻住了，是既有情绪反应的同源风险
    console.debug(`[Live2D] touch reaction dropped, reaction in flight (role ${entry.roleId})`);
    return false;
  }
  startReaction(entry, { group, index, loop: binding.loop ?? false });
  return true;
}

function applyExpression(entry: RoleModel, expression: string | null | undefined) {
  if (!expression) return;
  void entry.model
    .expression(expression)
    .catch((error: unknown) =>
      console.warn(`[Live2D] expression failed for role ${entry.roleId}`, error),
    );
}

/** 当前情绪对应的表情。抚摸结束后用它把表情收回去，所以必须与情绪路径取同一份。 */
function emotionExpression(entry: RoleModel): string | undefined {
  return (
    pickEmotionBinding(entry.variant.expressions, entry.emotion) ??
    entry.variant.default_expression ??
    undefined
  );
}

/** 换上某个部位绑定的抚摸表情。没绑就什么都不做，等于沿用当前情绪的表情。
 *
 * 表情在按下时就换上，比等到动作开演更跟手；收回去的时机见 updateTouchFrame。
 */
function applyTouchExpression(entry: RoleModel, part: string) {
  const expression = entry.variant.touch_motions?.[part]?.expression;
  if (!expression) return;
  touchExpressionRoleId = entry.roleId;
  applyExpression(entry, expression);
}

function applyEmotion(entry: RoleModel, emotion: string) {
  if (entry.emotion === emotion || !runtime) return;
  entry.emotion = emotion;
  applyExpression(entry, emotionExpression(entry));
  const motion = pickEmotionBinding(entry.variant.motions, emotion);
  // 情绪反应无条件抢占：entry.emotion 已经写进去了，这次丢掉就再也不会重播
  if (motion) startReaction(entry, motion);
}

function destroyEntry(entry: RoleModel) {
  entry.reactionSequence += 1;
  entry.reactionLifecycleCleanup?.();
  entry.reactionLifecycleCleanup = null;
  application?.stage.removeChild(entry.model);
  destroyModel(entry.model);
  models.delete(entry.roleId);
  emitActiveRoles();
}

async function loadRole(
  role: GameRole,
  variantName: string,
  variant: Live2dVariant,
  requestId: number,
) {
  await ensureApplication();
  if (!application || !runtime || disposed) return;
  let pendingModel: any = null;
  const previous = models.get(role.roleId);
  let previousDetached = false;
  try {
    // 与模型文件并行取回。资源表拿不到不该拖垮模型加载：它只是补声明，缺了顶多
    // 某个表情选了不生效，而抛出去会让角色退化成静态立绘，明显更糟
    const assets = getLive2dVariantAssets(role.roleId, variantName).catch((error: unknown) => {
      console.warn(`[Live2D] failed to load variant assets for role ${role.roleId}`, error);
      return null;
    });
    const source = await loadModelSource(role.roleId, variant.model, assets);
    // 必须在注入之后：扫描出来的待机组（小写 idle）只有注入完才解析得到，
    // 解析不到会抛错，被下面的 catch 兜成静态立绘
    const runtimeIdle = configureRuntimeIdle(source, variant.idle);
    const model = await runtime.engine.Live2DModel.from(source, {
      ticker: application.ticker,
      anchorMode: "drawable",
      autoFocus: false,
      autoHitTest: false,
      eyeBlink: true,
      idleMotionGroup: runtimeIdle?.group ?? variant.idle?.group ?? "Idle",
      motionPreload: runtime.engine.MotionPreloadStrategy.IDLE,
      useHighPrecisionMask: "auto",
      textureOptions: { lod: "single-auto" },
    });
    pendingModel = model;
    const currentRole = props.roles.find((item) => item.roleId === role.roleId);
    if (
      disposed ||
      requestId !== requestSequenceFor(role.roleId) ||
      !currentRole ||
      variantNameFor(currentRole) !== variantName
    ) {
      destroyModel(model);
      pendingModel = null;
      return;
    }
    const entry: RoleModel = {
      roleId: role.roleId,
      variantName,
      model,
      variant,
      runtimeIdle,
      emotion: "",
      requestId,
      mouthParameterIndex: -1,
      mouthValue: 0,
      eyeLeftParameterIndex: -1,
      eyeRightParameterIndex: -1,
      eyeBallXParameterIndex: -1,
      eyeBallYParameterIndex: -1,
      eyesOpen: true,
      focusFrozen: false,
      gazeMagnitude: 1,
      focusOrigin: null,
      touchRegions: null,
      touchBounds: null,
      reactionSequence: 0,
      reactionLifecycleCleanup: null,
    };
    if (variant.lip_sync?.parameter) {
      entry.mouthParameterIndex = findParameterIndex(entry, variant.lip_sync.parameter);
    }
    if (variant.eye_blink) {
      entry.eyeLeftParameterIndex = findParameterIndex(entry, variant.eye_blink.left);
      entry.eyeRightParameterIndex = findParameterIndex(entry, variant.eye_blink.right);
    }
    // 瞳孔参数名是 Cubism 标准 id，不像 eye_blink 那样需要按模型配置；
    // 缺失时索引为 -1，该模型就只衰减头部、瞳孔也跟着衰减（降级而非报错）
    entry.eyeBallXParameterIndex = findParameterIndex(entry, "ParamEyeBallX");
    entry.eyeBallYParameterIndex = findParameterIndex(entry, "ParamEyeBallY");
    model.internalModel.on("beforeModelUpdate", () => {
      const coreModel = model.internalModel.coreModel as {
        addParameterValueByIndex(index: number, value: number, weight?: number): void;
        getParameterValueByIndex(index: number): number;
      };
      if (entry.mouthParameterIndex >= 0) {
        coreModel.addParameterValueByIndex(entry.mouthParameterIndex, entry.mouthValue, 1);
      }
      const eyeValues: number[] = [];
      if (entry.eyeLeftParameterIndex >= 0) {
        eyeValues.push(coreModel.getParameterValueByIndex(entry.eyeLeftParameterIndex));
      }
      if (entry.eyeRightParameterIndex >= 0) {
        eyeValues.push(coreModel.getParameterValueByIndex(entry.eyeRightParameterIndex));
      }
      entry.eyesOpen = areEyesOpen(eyeValues);
      // 抚摸闭眼。眨眼控制器本帧已经写过开合参数，这里是覆盖它。
      // 用「加上差值」而不是直接赋值，是为了不必把 coreModel 的类型拓宽到 set 方法；
      // 目标值与当前值都落在 0..1 内，所以中间值不会越界，写入夹紧削不掉。
      if (eyeCloseWeight > 0 && swayRoleId === entry.roleId) {
        const openness = 1 - eyeCloseWeight;
        for (const index of [entry.eyeLeftParameterIndex, entry.eyeRightParameterIndex]) {
          if (index < 0) continue;
          coreModel.addParameterValueByIndex(
            index,
            openness - coreModel.getParameterValueByIndex(index),
            1,
          );
        }
      }
      // 瞳孔补偿。引擎已按衰减后的焦点写了一次眼球参数，这里把被缩掉的那份补回，
      // 使瞳孔仍然是满幅追踪（头部不受影响，只衰减那一份）。
      // 幅度必须在 updateModelFocus 覆写之前读：focusController.update(dt) 在帧首
      // 执行，追赶的是上一帧 handler 里设的 target，所以本帧的 fc 对应的是旧幅度。
      // fc 是「径向 + 限速」的弹簧，从原点出发时恒为 s·magnitude·u，故 fc/magnitude
      // 恰是未衰减时瞳孔应有的值，且 |fc/magnitude| ≤ 1，这次写入不会被参数 clamp 削掉。
      const magnitude = entry.gazeMagnitude;
      if (props.mode === "pet" && magnitude < 1) {
        const focusController = model.internalModel.focusController;
        const gain = 1 / magnitude - 1;
        if (entry.eyeBallXParameterIndex >= 0) {
          coreModel.addParameterValueByIndex(
            entry.eyeBallXParameterIndex,
            focusController.x * gain,
            1,
          );
        }
        if (entry.eyeBallYParameterIndex >= 0) {
          coreModel.addParameterValueByIndex(
            entry.eyeBallYParameterIndex,
            focusController.y * gain,
            1,
          );
        }
      }
      // 触摸模式下瞳孔看向指针，也就是玩家。引擎这一帧已经按焦点写过一次眼球参数，
      // 这里把那一份减掉、换成指向指针的方向：两者落在同一个参数上，且焦点值在本次
      // 更新里不会再变，所以能精确抵消，不会被写入夹紧削掉增量。摸头时焦点是晃动方向，
      // 于是头摆它的、眼睛盯玩家，互不干扰。
      // 权重按触摸模式淡入淡出，所以进模式和退模式都不会硬切。
      if (gazeWeight > 0) {
        const gaze = pointerGazeDirection(entry);
        if (gaze) {
          const focusController = model.internalModel.focusController;
          const deltaX = gaze.x * gazeWeight - focusController.x;
          const deltaY = gaze.y * gazeWeight - focusController.y;
          if (entry.eyeBallXParameterIndex >= 0) {
            coreModel.addParameterValueByIndex(entry.eyeBallXParameterIndex, deltaX, 1);
          }
          if (entry.eyeBallYParameterIndex >= 0) {
            coreModel.addParameterValueByIndex(entry.eyeBallYParameterIndex, deltaY, 1);
          }
        }
      }
      updateModelFocus(entry);
    });
    if (previous) {
      application.stage.removeChild(previous.model);
      previousDetached = true;
    }
    application.stage.addChild(model);
    applyLayout(entry, role);
    // Verify the new model in isolation before replacing the active variant.
    application.render();
    if (previous) destroyEntry(previous);
    models.set(role.roleId, entry);
    pendingModel = null;
    startIdle(entry);
    applyEmotion(entry, role.emotion);
    failedRoleIds.delete(role.roleId);
    emitFailedRoles();
    emitActiveRoles();
  } catch (error) {
    if (pendingModel) {
      application?.stage.removeChild(pendingModel);
      destroyModel(pendingModel);
    }
    if (requestId === requestSequenceFor(role.roleId)) {
      const current = models.get(role.roleId);
      if (current) {
        if (previousDetached && !application.stage.children.includes(current.model)) {
          application.stage.addChild(current.model);
          applyLayout(current, role);
        }
        failedRoleIds.delete(role.roleId);
      } else {
        failedRoleIds.add(role.roleId);
      }
      emitFailedRoles();
    }
    console.warn(
      `[Live2D] model load failed for role ${role.roleId}; keeping static avatar`,
      error,
    );
  }
}

const roleRequests = new Map<number, number>();
function nextRequest(roleId: number) {
  const id = ++requestSequence;
  roleRequests.set(roleId, id);
  return id;
}
function requestSequenceFor(roleId: number) {
  return roleRequests.get(roleId);
}

async function syncRoles() {
  // 形象由角色设定决定（主对话/桌宠各自一项），切成静态立绘的角色在此被排除，
  // 模型根本不加载；全部排除时下面会 destroyApplication()。
  const liveRoles = props.roles.filter((role) => prefersLive2d(role, props.mode));
  const liveIds = new Set(liveRoles.map((role) => role.roleId));
  let failedChanged = false;
  for (const roleId of [...failedRoleIds]) {
    if (!liveIds.has(roleId)) {
      failedRoleIds.delete(roleId);
      failedChanged = true;
    }
  }
  if (failedChanged) emitFailedRoles();
  for (const entry of [...models.values()]) {
    if (!liveIds.has(entry.roleId)) {
      nextRequest(entry.roleId);
      failedRoleIds.delete(entry.roleId);
      emitFailedRoles();
      destroyEntry(entry);
    }
  }
  if (!liveRoles.length) {
    destroyApplication();
    return;
  }
  await ensureApplication();
  for (const [index, role] of liveRoles.entries()) {
    const settings = role.live2d;
    if (!settings) continue;
    const variantName = variantNameFor(role);
    const variant = variantName ? resolveLive2dVariant(settings, role.clothesName) : undefined;
    if (!variantName || !variant) continue;
    const entry = models.get(role.roleId);
    if (
      !entry ||
      entry.variantName !== variantName ||
      entry.variant.model !== variant.model ||
      !motionBindingEquals(entry.variant.idle, variant.idle)
    ) {
      await loadRole(role, variantName, variant, nextRequest(role.roleId));
      continue;
    }
    if (entry.variant !== variant) {
      entry.variant = variant;
      entry.emotion = "";
      // 变体换了但模型没换（例如只改了 focus_anchor 或抚摸绑定）：缓存的视线原点与
      // 抚摸区域都已经失效，不清掉的话新锚点要等模型下次重新加载才生效
      entry.focusOrigin = null;
      entry.touchRegions = null;
      entry.touchBounds = null;
      startIdle(entry);
    }
    applyLayout(entry, role);
    applyEmotion(entry, role.emotion);
    application.stage.setChildIndex(
      entry.model,
      Math.min(index, application.stage.children.length - 1),
    );
  }
}

function queueSync() {
  syncPromise = syncPromise
    .then(syncRoles)
    .catch((error) => console.warn("[Live2D] stage sync failed", error));
}

function updateLipSync() {
  const audio = props.audioElement;
  for (const entry of models.values()) {
    const isSpeaker =
      entry.roleId === props.activeSpeakerId && audio && !audio.paused && !audio.ended;
    const target = isSpeaker
      ? sampleVoiceAmplitude(decodedVoice, audio.currentTime) * (entry.variant.lip_sync?.gain ?? 1)
      : 0;
    entry.mouthValue += (Math.min(1, target) - entry.mouthValue) * 0.38;
  }
}

watch(
  () =>
    props.roles.map(
      (role) =>
        [
          role.roleId,
          role.emotion,
          role.clothesName,
          role.show,
          role.scale,
          role.offsetX,
          role.offsetY,
          role.scaleP,
          role.offsetXP,
          role.offsetYP,
          role.live2d,
          role.avatarMode,
          role.avatarModeP,
        ] as const,
    ),
  queueSync,
  { deep: true },
);

// 投屏全局缩放 / 垂直偏移变化时重新布局（滑块拖动即时生效，复用 queueSync 幂等重排；
// 水平偏移由投屏窗口 CSS translateX 处理，不在此触发）
watch(() => [props.castScale, props.castOffsetY] as const, queueSync);

// 帧率上限设置热更新：设置窗口改完即时生效，无需重进桌宠模式
watch(
  () => props.maxFps ?? 0,
  (fps) => {
    if (!application) return;
    // 0 = 不限帧（PIXI Ticker 语义：maxFPS=0 即关闭上限）
    application.ticker.maxFPS = fps > 0 ? fps : 0;
  },
);

// 抚摸开关热更新：进出触摸模式即时生效，不必重进 /chat。桌宠模式本次不接抚摸，
// 所以两个条件必须同时成立（mode 是静态 prop，实际只有 touchEnabled 会变）。
function isStrokeEnabled() {
  return props.mode === "standard" && props.touchEnabled === true;
}

watch(isStrokeEnabled, (enabled) => (enabled ? attachTouchListeners() : detachTouchListeners()));

watch(
  () => [props.voiceDataUrl, props.roles.some((role) => prefersLive2d(role, props.mode))] as const,
  async ([url, hasLive2dRole]) => {
    const id = ++decodeSequence;
    decodedVoice = null;
    if (!hasLive2dRole) return;
    const decoded = await decodeVoiceForLipSync(url);
    if (id === decodeSequence) decodedVoice = decoded;
  },
);

onMounted(() => {
  // 桌宠模式：窗口非全屏，DOM pointermove 在鼠标移出窗口后停发，视线会冻结在
  // 最后一次窗口内位置。除窗口内 DOM 监听外，还需订阅 Rust 侧全局鼠标轮询
  // （每 50ms 上报窗口内逻辑坐标，即 webview 视口坐标，与 clientX/clientY 同源）。
  if (props.mode === "pet") {
    window.addEventListener("pointermove", handlePointerMove, { passive: true });
    void listen<CursorPayload>("pet:cursor", (event) => {
      pointerPosition = { clientX: event.payload.x, clientY: event.payload.y };
      // 旧版 Rust 载荷没有 screen 字段：保持上一次的值，别把参考系清掉
      if (event.payload.screen !== undefined) screenBox = event.payload.screen ?? null;
    })
      .then((unlisten) => {
        if (disposed) {
          unlisten();
          return;
        }
        cursorUnlisten = unlisten;
      })
      .catch(() => {
        // 非 Tauri 环境或事件系统不可用时静默降级（DOM 监听仍覆盖窗口内移动）
      });
  }
  if (isStrokeEnabled()) attachTouchListeners();
  queueSync();
});
onBeforeUnmount(() => {
  disposed = true;
  if (props.mode === "pet") {
    window.removeEventListener("pointermove", handlePointerMove);
    cursorUnlisten?.();
    cursorUnlisten = null;
  }
  detachTouchListeners();
  decodeSequence += 1;
  for (const entry of [...models.values()]) destroyEntry(entry);
  destroyApplication();
});
</script>
