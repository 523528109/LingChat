<template>
  <!--
    外层**常驻**,不用 v-if/v-show：TypeWriter 在构造时缓存 element，节点一旦被销毁
    重建，后续字符就会写进已脱离文档的旧节点 —— 表现为「气泡在、文字空白」。
    显隐与动效交给 CSS 类 + 作用域关键帧（见文件末尾 .enter/.leave）。
  -->
  <div
    class="absolute z-30 flex cursor-pointer"
    :class="[horizontal ? 'w-[85%] flex-col' : 'inset-x-0 justify-center px-2', frameClass]"
    :style="frameStyle"
    @click="emit('advance')"
  >
    <div
      ref="bubbleRef"
      class="hover-up relative rounded-[calc(20px*var(--pet-ui-scale,1))] border border-white/10 bg-neutral-950/50 px-[calc(18px*var(--pet-ui-scale,1))] py-[calc(6px*var(--pet-ui-scale,1))] text-white backdrop-blur-xl backdrop-saturate-200 transition-all duration-300 [text-shadow:0_1px_4px_rgba(0,0,0,0.5)] hover:scale-[1.02] hover:border-white/20 hover:bg-neutral-950/65"
      :class="[horizontal ? 'w-full' : 'w-[85%]', animClass]"
      :style="{ maxHeight: `${maxHeight}px` }"
    >
      <div class="relative overflow-hidden">
        <Transition name="emotion-slide">
          <div
            v-if="emotion"
            :key="emotion"
            class="mb-0.5 inline-block max-w-full truncate text-[calc(12px*var(--pet-ui-scale,1))] font-semibold tracking-wider text-cyan-400 italic drop-shadow-[0_1px_4px_rgba(0,176,255,0.5)]"
          >
            {{ emotion }}
          </div>
        </Transition>
      </div>

      <!-- 正文：高度按文本实测动态设定（见 applyTextHeight），上限之外在盒内滚动 -->
      <div
        ref="textRef"
        class="dialog-text-lock [scrollbar-width:none] overflow-y-auto pb-[0.4em] text-[calc(15px*var(--pet-ui-scale,1))] leading-snug font-medium break-all whitespace-pre-line [text-shadow:0_0_3px_rgba(0,0,0,0.9),0_1px_4px_rgba(0,0,0,0.5)] [&::-webkit-scrollbar]:hidden"
      ></div>

      <!-- 长尾：从气泡盒**朝向宠物**的那条边伸出（上置朝下、下置朝上、左右置水平伸出），
           一半在盒内一半在盒外，外半截落在窗口让出的 TAIL_OVERHANG 预留区里
           （原版直接写 -bottom-2.5/-2，但那会伸到窗口底边之外被裁） -->
      <div class="absolute h-0 w-0 drop-shadow-md" :class="tailOuterClass"></div>
      <div class="absolute h-0 w-0" :class="tailInnerClass"></div>
    </div>

    <!-- 通知：气泡的**兄弟**节点，不是子节点。
         通知与气泡的显隐互相独立（无台词时也可能有通知），塞进气泡里就会被气泡的
         淡出一起藏掉 —— 表现为"通知永远不显示"。它钉在气泡的背宠一侧：竖向模式下在
         气泡的上/下方，横向模式下在气泡的下/上方（贴在气泡背离窗口边的那一侧）。
         气泡高度随文本变化，通知因此始终紧贴气泡，而不是钉在窗口边。 -->
    <div
      class="absolute inset-x-0 flex justify-center"
      :class="top ? 'top-full mt-1' : 'bottom-full mb-1'"
      :style="{ maxHeight: 'var(--notify-h)' }"
    >
      <PetNotification />
    </div>
  </div>
</template>

<script setup lang="ts">
/**
 * 气泡（桌宠显示层）。**受控组件**：可见性与文本都由父级给，自己不读对话状态、
 * 不碰事件队列 —— 气泡窗与宠物窗是两个 webview，各自的事件队列互不相识，
 * 让气泡自己跑状态机就会与镜像来的状态打架，气泡永远停在隐藏态。
 *
 * 只有打字机动画归本组件，因为它绑在 DOM 上。
 */
import { computed, ref, watch } from "vue";
import type { BubbleAlign, BubbleSide } from "./bubbleMirror";
import { useTypeWriter } from "@/composables/ui/useTypeWriter";
import { createCharRevealWriter } from "@/utils/typewriter/charReveal";
import { charRevealCharHtml } from "@/utils/typewriter/charHtml";
import PetNotification from "./PetNotification.vue";

const props = defineProps<{
  visible: boolean;
  /** 完整台词；与上一次不同才（重新）打字 */
  line: string;
  emotion?: string;
  speed?: number;
  /** 直接整段显示、不播打字动画 */
  instant?: boolean;
  /** 气泡高度上限（px），父级按带高算好传入；正文超出部分在盒内滚动 */
  maxHeight: number;
  /** 气泡在宠物的哪一侧：决定贴窗口哪条边、长尾朝向、通知位置与进出场位移方向 */
  side?: BubbleSide;
  /** 左右置时气泡贴窗口上边还是下边（父级按宠物在屏幕上的高低镜像过来）；
   *  上下置时无意义 —— 它们的贴边由 `side` 唯一决定（上置贴底、下置贴顶） */
  align?: BubbleAlign;
  /**
   * 左右置时内容相对**所贴那条边**的额外内缩（CSS px，父级镜像过来）。
   *
   * 本组件的内容贴的是本窗口的上/下边，而本窗口与宠物窗同边对齐；宠物窗下半截是透明的
   * 输入带，宠物贴到屏幕上/下沿时那条边会跑到工作区外，内容跟着被推出去。父级把溢出量
   * 算好（见 PetMode.vue 的 bubbleAlignInset），这里把内容拉回工作区内 —— 边角处气泡
   * 因此紧贴屏幕边缘。上下置不传（它们的贴边由 `side` 决定，不靠窗口的上下边）。
   */
  alignInset?: number;
}>();

const emit = defineEmits<{ advance: []; drained: []; "typing-change": [typing: boolean] }>();

const side = computed<BubbleSide>(() => props.side ?? "above");
/** 左右置：版式整体转 90° —— 贴窗口左右边、长尾水平指向宠物、内容纵向贴上/下边 */
const horizontal = computed(() => side.value === "left" || side.value === "right");
/**
 * 气泡盒在窗口里贴哪条边：true = 贴上边。
 *
 * 上下置由方位唯一决定（上置贴底、下置贴顶）；左右置由父级给（往离屏幕边远的一侧靠，
 * 这样气泡始终朝屏幕中部、不会被最近的屏幕边裁掉）。
 */
const top = computed(() => (horizontal.value ? props.align !== "bottom" : side.value === "below"));

/**
 * 根节点只管定位：气泡盒贴窗口哪条边（+ 不可见时不吃鼠标事件）。
 * 显隐动效不在这里 —— 根节点带着通知，动效挂根上会连通知一起淡掉。
 */
const frameClass = computed(() => [
  ...(horizontal.value
    ? [
        // 左右置：贴窗口左边（气泡在宠物右侧）或右边（气泡在宠物左侧），长尾余量留在宠物那一侧
        side.value === "left" ? "right-(--tail) items-end" : "left-(--tail) items-start",
      ]
    : [top.value ? "top-(--tail) items-start" : "bottom-(--tail) items-end"]),
  props.visible ? "" : "pointer-events-none",
]);

/**
 * 左右置时的纵向锚点（贴在窗口上边还是下边，再让出内缩量）。
 *
 * 内缩量只能是内联样式：它由宠物窗按「宠物窗贴边出屏多少」实算，值会随拖动连续变化，
 * 写死成 Tailwind 类既表达不了也没必要。上下置时返回 undefined，定位完全交给 frameClass。
 *
 * 补一条短过渡：内缩量是跨窗口（宠物窗算 → IPC → 本窗口）来的，落地时刻与气泡窗被 Rust
 * 挪动的时刻差着几毫秒到一两帧，拖动时表现为内容在边缘附近抖；过渡把它抹平，同时让
 * 「换边/换贴边」那种整块跳变也能滑过去，而不是硬切。
 */
const ALIGN_INSET_TRANSITION_MS = 120;

const frameStyle = computed(() => {
  if (!horizontal.value) return undefined;
  const inset = Math.max(0, props.alignInset ?? 0);
  const transition = `top ${ALIGN_INSET_TRANSITION_MS}ms ease-out, bottom ${ALIGN_INSET_TRANSITION_MS}ms ease-out`;
  return top.value ? { top: `${inset}px`, transition } : { bottom: `${inset}px`, transition };
});

/**
 * 长尾的朝向与位置：尾尖永远指向宠物（外半截落在窗口让出的 TAIL 预留区里）。
 *
 * 横向三角形的朝向由"哪条边的 border 有颜色"决定：border-r 有颜色 = 指向右。
 * 类名必须写成完整字面量，拼接的字符串 Tailwind 扫不到、生不出样式。
 */
const tailOuterClass = computed(() => {
  if (side.value === "left") {
    // 气泡在宠物左侧 → 尾尖朝右
    return "-right-2.5 top-1/2 -translate-y-1/2 border-t-10 border-b-10 border-r-white/10 border-t-transparent border-b-transparent";
  }
  if (side.value === "right") {
    return "-left-2.5 top-1/2 -translate-y-1/2 border-t-10 border-b-10 border-l-white/10 border-t-transparent border-b-transparent";
  }
  return top.value
    ? "-top-2.5 left-1/2 -translate-x-1/2 border-r-10 border-l-10 border-b-white/10 border-r-transparent border-l-transparent"
    : "-bottom-2.5 left-1/2 -translate-x-1/2 border-r-10 border-l-10 border-t-white/10 border-r-transparent border-l-transparent";
});

/** 内层长尾（比外层窄 2px，做出描边感），朝向与外层一致 */
const tailInnerClass = computed(() => {
  if (side.value === "left") {
    return "-right-2 top-1/2 -translate-y-1/2 border-t-8 border-b-8 border-r-white/8 border-t-transparent border-b-transparent";
  }
  if (side.value === "right") {
    return "-left-2 top-1/2 -translate-y-1/2 border-t-8 border-b-8 border-l-white/8 border-t-transparent border-b-transparent";
  }
  return top.value
    ? "-top-2 left-1/2 -translate-x-1/2 border-r-8 border-l-8 border-b-white/8 border-r-transparent border-l-transparent"
    : "-bottom-2 left-1/2 -translate-x-1/2 border-r-8 border-l-8 border-t-white/8 border-r-transparent border-l-transparent";
});

/**
 * 是否已经显示过。
 *
 * 气泡盒的隐藏态是靠退场动画的 forwards 停在 opacity:0 上的，而 animation 在**挂载
 * 时就会播**：没台词切进桌宠时，气泡盒会先按"从可见到不可见"完整播一遍退场动画 ——
 * 表现为气泡闪一下才消失。所以没显示过之前用无动画的静态隐藏态。
 */
const hasShown = ref(false);
watch(
  () => props.visible,
  (visible) => {
    if (visible) hasShown.value = true;
  },
  { immediate: true },
);

/** 气泡盒自己的显隐动效（与下面的通知无关，两者互不牵连）；方向 = 从宠物那一侧进出 */
const animClass = computed(() => {
  if (props.visible) return `enter-${side.value}`;
  if (!hasShown.value) return "bubble-hidden";
  return `leave-${side.value}`;
});

const textRef = ref<HTMLElement | null>(null);
const bubbleRef = ref<HTMLElement | null>(null);

// 逐字符淡入+上浮渲染器（颜色/阴影继承气泡样式）
const charReveal = createCharRevealWriter({ charHtml: charRevealCharHtml });

const { startTyping, stopTyping, finishTyping, isTyping } = useTypeWriter(
  textRef,
  undefined,
  // 正文是普通 <div>（非 textarea/input），必须提供 writeFn 做增量字符渲染
  charReveal.writeFn,
);

// 打字状态广播给父级（BubbleWindow 再转报宠物窗）——自动推进调度器在宠物窗，
// 跨窗口拿不到本组件的响应式 ref，只能靠事件同步，否则桌宠的自动推进看不到打字机。
watch(isTyping, (typing) => emit("typing-change", typing), { immediate: true });

/**
 * 去重与"重播"判据。
 *
 * 背景：本组件会在三种情况下被重渲染 —— 切情绪、改尺寸、以及隐藏后重新显示。
 * 前两者显示区内容还在，不该动；后者内容已随 stopTyping 清空，必须重画，
 * 但**不能重播打字机**（用户要求打字机只在新台词时播）。
 *
 * 关键在于"内容还在不在"必须与"是不是新台词"分开判断：
 * 之前把两者揉进一个条件里，导致新台词也被当成"重画"，打字机再没跑过。
 */
const displayEmpty = ref(true);
/** 已完整呈现的台词（渲染收尾时记录）；null = 显示区没有有效内容 */
const shownLine = ref<string | null>(null);

/**
 * 渲染代次：每次渲染自增，旧渲染 await 回来时若代次已变就自我作废，
 * 避免旧渲染的收尾踩到新渲染、或误发 drained 打乱调度。
 */
let renderToken = 0;

/**
 * 设定正文高度：按整段文本实测，钳到高度上限。
 *
 * 必须**先锁到最终高度再开始打字** —— 否则盒子会随逐字换行而抖动。
 * 高度变化由 `.dialog-text-lock` 的 height 过渡负责，所以气泡是"长"出来的。
 * 上限之外的部分留给 `overflow-y` 滚动，因此盒子永远不会超出窗口。
 */
const applyTextHeight = (line: string) => {
  const el = textRef.value;
  if (!el) return;
  if (!line.trim()) {
    el.style.height = "0px";
    return;
  }
  // 离屏克隆量"整段渲染后"的高度：借用真实元素的行宽与全部排版样式
  const clone = el.cloneNode(false) as HTMLDivElement;
  clone.style.cssText = `position:fixed;left:-9999px;top:0;visibility:hidden;height:auto;overflow:visible;width:${el.clientWidth}px`;
  el.parentElement?.appendChild(clone);
  charReveal.renderInstant(clone, line);
  const measured = clone.offsetHeight;
  clone.remove();
  charReveal.reset();
  el.style.height = `${Math.min(measured, props.maxHeight)}px`;
};

/** 清空显示区并把"已呈现"作废 —— 内容没了，就不能再说这一句画过了 */
const clearDisplay = () => {
  stopTyping();
  if (textRef.value) {
    textRef.value.innerHTML = "";
    textRef.value.style.height = "0px";
  }
  charReveal.reset();
  displayEmpty.value = true;
  shownLine.value = null;
};

/**
 * 重画这一句。
 * @param instant 整段显示、不播打字机
 */
const render = async (line: string, instant: boolean) => {
  const token = ++renderToken;

  stopTyping();
  if (textRef.value) {
    textRef.value.innerHTML = "";
    // 归零并强制重排，让浏览器把"0"记为过渡起点，
    // 随后设到新高度 → .dialog-text-lock 的 height 过渡才会生效
    textRef.value.style.height = "0px";
    void textRef.value.offsetHeight;
  }
  charReveal.reset();
  applyTextHeight(line);

  if (instant) {
    if (textRef.value) charReveal.renderInstant(textRef.value, line);
    displayEmpty.value = false;
    shownLine.value = line;
    emit("drained");
    // 整段复现不经过打字机，isTyping 从头到尾都是 false，上面那条 watch 不会触发 ——
    // 必须显式回报一次，否则宠物窗会一直等一个不会来的「打字结束」，自动推进卡死
    emit("typing-change", false);
    return;
  }

  await startTyping(line, props.speed);
  // 已被更新的渲染接管（或被隐藏）：不记已呈现、也不发 drained，它属于上一句
  if (token !== renderToken) return;
  displayEmpty.value = false;
  shownLine.value = line;
  if (!isTyping.value) emit("drained");
};

watch(
  () => [props.visible, props.line, props.instant] as const,
  ([visible, line, instant], prev) => {
    if (!visible || !line) {
      // 作废在跑的渲染，避免它稍后发 drained 干扰下一句的调度
      renderToken++;
      clearDisplay();
      return;
    }

    const sameLine = shownLine.value === line;
    const stillShown = prev?.[0] === true && !displayEmpty.value;

    // 状态没变（切情绪、改尺寸、通知变化）→ 什么都不做，保持气泡现状
    if (stillShown && sameLine) return;

    // 显示区已空但显示状态没变（隐藏后重新显示）→ 重画，但整段复现，不重播打字机
    const restoring = prev?.[0] === false && sameLine;
    void render(line, instant || restoring);
  },
  { immediate: true, flush: "post" },
);

defineExpose({
  isTyping,
  /** 补全当前打字动画（点头像时先补全、不推进） */
  finishTyping,
  bubbleRef,
});
</script>

<style scoped>
/**
 * 出现/消失动效 —— 复刻自 5f61eeec「feat: 添加气泡/通知换位动效」与当时 DialogueBox 的定义：
 *   容器 transition-all duration-300 ease-out，位移 ±2（translate-y-0 ↔ -translate-y-2）配合 opacity 0 ↔ 100。
 *
 * 三处改动：
 *   1. 用关键帧而非类切换：本组件节点常驻（见模板注释），必须在没有"上一次状态"的
 *      情况下也能播出进场动效，animation 天然满足。
 *   2. 位移方向 = **从宠物那一侧进出**（上置自下而上、下置自上而下、左右置水平推入推出），
 *      原版两个方向都朝上，进场像缩回宠物头顶。
 *   3. 动效挂在**气泡盒**而不是根节点上：根节点还带着通知，挂根上会连通知一起淡掉。
 *
 * 四种方位各一套（而不是在关键帧里用 var() 取方向），避免依赖自定义属性在 keyframes 中
 * 的求值细节 —— 那在部分 WebView 上不生效。
 */
/* 从未显示过的静止隐藏态：不带动画，避免挂载那一刻播一遍退场动画（气泡闪一下） */
.bubble-hidden {
  opacity: 0;
}

.enter-above {
  animation: bubble-in-above 300ms cubic-bezier(0, 0, 0.2, 1);
}

.leave-above {
  animation: bubble-out-above 300ms cubic-bezier(0, 0, 0.2, 1) forwards;
}

@keyframes bubble-in-above {
  from {
    opacity: 0;
    transform: translateY(6px) scale(0.98);
  }
  to {
    opacity: 1;
    transform: none;
  }
}

@keyframes bubble-out-above {
  from {
    opacity: 1;
    transform: none;
  }
  to {
    opacity: 0;
    transform: translateY(4px) scale(0.98);
  }
}

/* 下置：整套镜像 —— 进场自上而下、退场向上（气泡在宠物下方，动效应朝宠物方向收放） */
.enter-below {
  animation: bubble-in-below 300ms cubic-bezier(0, 0, 0.2, 1);
}

.leave-below {
  animation: bubble-out-below 300ms cubic-bezier(0, 0, 0.2, 1) forwards;
}

@keyframes bubble-in-below {
  from {
    opacity: 0;
    transform: translateY(-6px) scale(0.98);
  }
  to {
    opacity: 1;
    transform: none;
  }
}

@keyframes bubble-out-below {
  from {
    opacity: 1;
    transform: none;
  }
  to {
    opacity: 0;
    transform: translateY(-4px) scale(0.98);
  }
}

/* 右置（气泡在宠物右侧）：自左向右推入、向左收回 —— 位移方向朝着宠物 */
.enter-right {
  animation: bubble-in-right 300ms cubic-bezier(0, 0, 0.2, 1);
}

.leave-right {
  animation: bubble-out-right 300ms cubic-bezier(0, 0, 0.2, 1) forwards;
}

@keyframes bubble-in-right {
  from {
    opacity: 0;
    transform: translateX(-6px) scale(0.98);
  }
  to {
    opacity: 1;
    transform: none;
  }
}

@keyframes bubble-out-right {
  from {
    opacity: 1;
    transform: none;
  }
  to {
    opacity: 0;
    transform: translateX(-4px) scale(0.98);
  }
}

/* 左置（气泡在宠物左侧）：镜像一套 */
.enter-left {
  animation: bubble-in-left 300ms cubic-bezier(0, 0, 0.2, 1);
}

.leave-left {
  animation: bubble-out-left 300ms cubic-bezier(0, 0, 0.2, 1) forwards;
}

@keyframes bubble-in-left {
  from {
    opacity: 0;
    transform: translateX(6px) scale(0.98);
  }
  to {
    opacity: 1;
    transform: none;
  }
}

@keyframes bubble-out-left {
  from {
    opacity: 1;
    transform: none;
  }
  to {
    opacity: 0;
    transform: translateX(4px) scale(0.98);
  }
}

/* 悬浮上浮 0.8px。原版写的是 hover:-translate-y-0.2，但 Tailwind 不为 .2 这种
   非 scale 小数算子生成规则，该效果实际从未生效；这里用 scoped 规则补上。 */
.hover-up:hover {
  transform: translateY(-0.8px);
}

/* 情绪标签切换：上一个向左滑出，下一个从右侧滑入（推挤效果） */
.emotion-slide-enter-active,
.emotion-slide-leave-active {
  transition:
    transform 0.3s cubic-bezier(0.25, 0.46, 0.45, 0.94),
    opacity 0.3s ease;
}
/* 离开中的旧情绪脱离文档流，覆盖在新情绪上方向左滑出，容器宽度由新情绪决定 */
.emotion-slide-leave-active {
  position: absolute;
  left: 0;
  top: 0;
}
.emotion-slide-enter-from {
  transform: translateX(100%);
  opacity: 0;
}
.emotion-slide-leave-to {
  transform: translateX(-100%);
  opacity: 0;
}

/* 打字期间高度锁定为最终高度；行切换时高度平滑扩展/收缩 */
.dialog-text-lock {
  transition: height 0.25s cubic-bezier(0.25, 0.46, 0.45, 0.94);
}
</style>
