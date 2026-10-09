/**
 * 拓扑画布上的共用几何。
 *
 * 抽出来是因为它曾经在两处各写了一份魔法数字，而两处都与节点的实际渲染尺寸
 * 不一致——一个 50 字符的节点 id 会把节点撑到 550px，布局却还按 190px 排，
 * 结果后一个节点整个落进前一个的矩形里，连线也跟着指错方向。
 *
 * **改这里就要同步改 `HubNode.vue` / `EditorNode.vue 的宽度约束**：
 * 估算与实际不一致时，差几个像素只是留白难看，差几百像素就是节点重叠。
 */

/** 节点宽度的下限。与节点组件里的 `min-width` 一致 */
export const NODE_MIN_WIDTH = 150;

/** 节点宽度的上限。与节点组件里的 `max-width` 一致 */
export const NODE_MAX_WIDTH = 240;

/** 等宽 13px 下一个字符的宽度，用于把 id 长度折算成像素 */
const CHAR_WIDTH = 7.8;

/** 左右 padding 合计 */
const PADDING = 28;

/**
 * 估算一个节点占多大。
 *
 * 估的是宽度不是像素级还原：差几个像素只影响留白，差 360px 才会让两个节点叠在一起。
 * 高度按「id 放不下就折行」算——不算进去的话，折行后的节点会压到下一层。
 */
export function estimateNodeSize(id) {
  const textWidth = String(id ?? "").length * CHAR_WIDTH;
  const width = Math.min(NODE_MAX_WIDTH, Math.max(NODE_MIN_WIDTH, textWidth + PADDING));
  const idLines = Math.max(1, Math.ceil(textWidth / Math.max(width - PADDING, 1)));
  return { width, height: 42 + (idLines - 1) * 18 };
}

/** dagre 的布局参数。两份画布用同一套，图的样子才不会因为页面不同而变 */
export const LAYOUT_GRAPH_OPTIONS = {
  rankdir: "LR",
  nodesep: 30,
  ranksep: 70,
  marginx: 30,
  marginy: 30,
};
