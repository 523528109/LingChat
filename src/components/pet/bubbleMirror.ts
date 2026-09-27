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
