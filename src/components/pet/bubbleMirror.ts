export interface BubbleMirror {
  status: string;
  line: string;
  title: string;
  subtitle: string;
  emotion: string;
  motionText: string;
  avatarAudio: string;
  textSpeed: number;
  petScale: number;
  bubbleSide: BubbleSide;
  bubbleAlign: BubbleAlign;
  alignInset: number;
  swapping: boolean;
  notification: { isVisible: boolean; title: string; message: string; type: string };
}

export type BubbleSide = "above" | "below" | "left" | "right";
export type BubbleAlign = "top" | "bottom";

export const PET_BUBBLE_EVENT = "pet:bubble-mirror";
export const PET_BUBBLE_REQUEST = "pet:bubble-request";
export const PET_BUBBLE_TYPING_EVENT = "pet:bubble-typing";
export const PET_FINISH_TYPING_EVENT = "pet:finish-typing";
