<template>
  <!--
    编辑器里的一个节点。

    与只读的 `HubNode` 分开而不是加个 `editable` 开关：两者的手柄行为、
    交互、错误呈现都不一样（只读版的手柄根本不可点），
    塞进一个组件会让每个分支都要问「现在是哪种模式」。
  -->
  <div class="editor-node" :class="classes">
    <Handle type="target" :position="Position.Left" class="handle" />

    <div class="node-head">
      <span class="node-id">{{ data.label }}</span>
      <el-icon class="remove" title="删除这个节点" @click.stop="data.onRemove?.()">
        <Close />
      </el-icon>
    </div>
    <div class="node-plugin">
      <span v-if="data.plugin">{{ data.plugin }}</span>
      <span v-else class="placeholder">未选择插件</span>
    </div>
    <div class="node-meta">
      <span class="node-version">{{ data.version || "最新版本" }}</span>
      <span v-if="data.timeout_ms" class="node-badge">{{ data.timeout_ms }}ms</span>
      <span v-if="data.retries" class="node-badge retry">重试{{ data.retries }}</span>
    </div>

    <Handle type="source" :position="Position.Right" class="handle" />
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
  "is-invalid": props.data.invalid,
  "is-empty": !props.data.plugin,
}));
</script>

<style scoped>
.editor-node {
  min-width: 150px;
  /* 上限与 `shared/graph.js` 的估算一致。节点 id 是用户自己填的、没有长度限制，
     不设上限时一个长 id 能把节点撑到几百像素，而布局按估算值排坐标——
     结果就是两个节点叠在一起、连线指错方向（详情页踩过一模一样的坑）。 */
  max-width: 240px;
  padding: 10px 14px;
  border: 1.5px solid var(--el-border-color);
  border-radius: 8px;
  background: var(--el-bg-color);
  box-shadow: 0 1px 4px rgb(0 0 0 / 6%);
  cursor: grab;
}

.editor-node.is-selected {
  border-color: var(--el-color-primary);
  box-shadow: 0 0 0 3px var(--el-color-primary-light-8);
}

.editor-node.is-invalid {
  border-color: var(--el-color-danger);
}

.editor-node.is-empty {
  border-style: dashed;
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
  /* flex 子项默认 min-width:auto，不加这行它宁可溢出也不收缩，折行规则就没机会生效 */
  min-width: 0;
  word-break: break-all;
}

.remove {
  font-size: 13px;
  color: var(--el-text-color-secondary);
  cursor: pointer;
}

.remove:hover {
  color: var(--el-color-danger);
}

.node-plugin {
  margin-top: 3px;
  font-size: 12px;
  color: var(--el-text-color-regular);
}

.placeholder {
  color: var(--el-text-color-placeholder);
}

.node-meta {
  display: flex;
  align-items: center;
  gap: 6px;
  margin-top: 5px;
}

.node-version {
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  font-size: 11px;
  color: var(--el-text-color-secondary);
}

.node-badge {
  font-size: 11px;
  color: var(--el-color-warning);
}

/* 编辑模式下两个手柄都要能拖：入口/叶子是「连出来之后」才确定的 */
.handle {
  width: 9px;
  height: 9px;
  background: var(--el-color-primary);
  border: 2px solid var(--el-bg-color);
}
</style>
