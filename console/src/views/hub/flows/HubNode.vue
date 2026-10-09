<template>
  <!--
    flow 图上的一个节点。

    **只读画布**：手柄不做连线用，只标出数据流的进出方向——
    入口节点的左边没有手柄，出口节点的右边没有手柄，一眼能看出数据从哪进从哪出。
  -->
  <div class="hub-node" :class="classes">
    <Handle v-if="!data.isEntry" type="target" :position="Position.Left" class="hub-handle" />

    <div class="node-head">
      <span class="node-id">{{ data.label }}</span>
      <el-tag v-if="data.isEntry" size="small" type="success" effect="plain">入口</el-tag>
    </div>
    <div class="node-plugin">{{ data.plugin }}</div>
    <div class="node-meta">
      <span class="node-version">{{ data.version || "最新版本" }}</span>
      <span v-if="data.retries" class="node-retry">重试 {{ data.retries }}</span>
    </div>

    <Handle v-if="!data.isExit" type="source" :position="Position.Right" class="hub-handle" />
  </div>
</template>

<script setup>
import { computed } from "vue";
import { Handle, Position } from "@vue-flow/core";

const props = defineProps({
  data: { type: Object, required: true },
  selected: { type: Boolean, default: false },
});

const classes = computed(() => ({
  "is-selected": props.selected,
  "is-entry": props.data.isEntry,
  "is-exit": props.data.isExit,
  "has-error": props.data.hasError,
}));
</script>

<style scoped>
.hub-node {
  min-width: 150px;
  /* 上限必须有：节点 id 没有长度限制（中台只约束 flow 名），
     一个 50 字符的 id 能把节点撑到 550px。布局那边按固定宽度算坐标，
     于是后一个节点会整个落进前一个的矩形里——连线也跟着画错方向。
     超出部分用换行而不是省略号：节点 id 会出现在调用链、span 与日志里，
     在图上把它截断，「说的到底是哪个节点」就变成一个要猜的问题。 */
  max-width: 240px;
  padding: 10px 14px;
  border: 1.5px solid var(--el-border-color);
  border-radius: 8px;
  background: var(--el-bg-color);
  box-shadow: 0 1px 4px rgb(0 0 0 / 6%);
  cursor: pointer;
  transition: border-color 0.15s, box-shadow 0.15s;
}

.hub-node:hover {
  border-color: var(--el-color-primary-light-5);
}

.hub-node.is-selected {
  border-color: var(--el-color-primary);
  box-shadow: 0 0 0 3px var(--el-color-primary-light-8);
}

/* 入口用左侧色条标出来——同层有多个入口时，比一个 tag 更快看出哪些是起点 */
.hub-node.is-entry {
  border-left: 3px solid var(--el-color-success);
}

.hub-node.has-error {
  border-color: var(--el-color-danger);
}

.node-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
}

.node-id {
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  font-size: 13px;
  font-weight: 600;
  /* flex 子项默认 min-width:auto，不加这行它宁可溢出也不收缩，换行规则就没机会生效 */
  min-width: 0;
  word-break: break-all;
}

.node-plugin {
  margin-top: 3px;
  font-size: 12px;
  color: var(--el-text-color-regular);
}

.node-meta {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-top: 5px;
}

.node-version {
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  font-size: 11px;
  color: var(--el-text-color-secondary);
}

.node-retry {
  font-size: 11px;
  color: var(--el-color-warning);
}

/* 只读画布：手柄只做方向指示，不参与交互 */
.hub-handle {
  width: 6px;
  height: 6px;
  border: none;
  background: var(--el-border-color-darker);
  pointer-events: none;
}
</style>
