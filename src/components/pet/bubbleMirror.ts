/**
 * 桌宠显示状态的跨窗口镜像。
 *
 * 宠物窗与气泡窗是两个 webview，Pinia 不共享。气泡窗要画的台词、情绪、标题、
 * 通知都来自宠物窗的 store，所以在宠物窗侧监听这些字段并广播，气泡窗侧应用。
 * 与投屏镜像（`useCastMirror` + `cast:mirror`）同构：显示层单向同步，不反向写回。
 */

/** 气泡窗需要复现的最小显示状态 */
export interface BubbleMirror {
  status: string;
  line: string;
  title: string;
  subtitle: string;
  emotion: string;
  motionText: string;
  /**
   * 当前角色语音（`uiStore.currentAvatarAudio`）。
   *
   * 气泡窗不跑事件处理器（main.ts 里 `window=bubble` 不注册 `initializeTauriEventListeners`），
   * 该字段在本窗口恒为默认值 `"None"` —— 于是气泡里的打字机读不到「这句有语音」，
   * 打字音效就会在角色语音同时播放。必须随台词一起镜像过来。
   */
  avatarAudio: string;
  /** 打字速度（来自 settings.textSpeed，气泡窗的 uiStore 是只读派生，故直接镜像设置值） */
  textSpeed: number;
  /** 桌宠缩放。气泡窗的窗口尺寸由 Rust 按它创建，CSS 缩放必须用同一个值 ——
   *  否则两边对「窗口有多大」的认知不一致，内容会超出窗口被硬边裁切。 */
  petScale: number;
  /** 气泡在宠物的哪一侧（手动设置与自动判定都在宠物窗算好后镜像过来）。
   *  气泡窗据此翻转版式：贴窗口哪条边、长尾朝向、通知位置、进出场位移方向。 */
  bubbleSide: BubbleSide;
  /** 左右置时气泡贴窗口上边还是下边（由宠物在屏幕上的高低决定，见 PetMode.vue）。
   *  上下置时无意义 —— 它们的贴边由 `bubbleSide` 唯一决定。 */
  bubbleAlign: BubbleAlign;
  /** 换位过程中（先淡出、窗口跳位、再淡入），气泡窗用它把整段内容淡掉，遮住跳位。 */
  swapping: boolean;
  notification: { isVisible: boolean; title: string; message: string; type: string };
}

/** 气泡相对宠物的方位 */
export type BubbleSide = "above" | "below" | "left" | "right";
/** 左右置时气泡在气泡窗里贴哪条边 */
export type BubbleAlign = "top" | "bottom";

export const PET_BUBBLE_EVENT = "pet:bubble-mirror";
export const PET_BUBBLE_REQUEST = "pet:bubble-request";

/**
 * 气泡窗 → 宠物窗：气泡内打字机的打字状态变化。
 *
 * 打字机动效绑在气泡窗的 DOM 上（DialogueBox 在 BubbleWindow 里），而自动推进调度器
 * 在宠物窗（事件队列、语音、AUTO 开关都在那边），跨窗口拿不到组件的响应式 ref，
 * 只能由气泡窗把 `isTyping` 广播回去。没有它，宠物窗会以为「永远不在打字」，
 * 自动推进要么抢跑（打字没完就跳下一句）要么根本不推进。
 */
export const PET_BUBBLE_TYPING_EVENT = "pet:bubble-typing";

/**
 * 宠物窗 → 气泡窗：请求补全当前打字动画（点击头像则先补全文本、不推进队列），
 * 与主界面 GameDialog 点击时先 `finishTyping` 的语义一致。
 */
export const PET_FINISH_TYPING_EVENT = "pet:finish-typing";
