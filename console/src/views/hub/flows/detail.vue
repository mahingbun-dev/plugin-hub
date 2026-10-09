<template>
  <div class="hub-flow-detail">
    <el-card class="title-card">
      <div class="page-header">
        <div class="header-left">
          <el-button link @click="goBack">
            <el-icon><ArrowLeft /></el-icon> 返回
          </el-button>
          <div>
            <div class="header-title">
              <span class="mono">{{ flowName }}</span>
              <el-tag v-if="published" type="success" effect="plain" size="small">
                已发布 rev {{ published.revision }}
              </el-tag>
              <el-tag v-if="detail?.draft" type="warning" effect="plain" size="small">
                有草稿
              </el-tag>
            </div>
            <div class="header-subtitle">{{ detail?.description || "（无描述）" }}</div>
          </div>
        </div>
        <div class="header-actions">
          <el-select v-model="selectedRevisionId" style="width: 240px" placeholder="选择修订">
            <el-option
              v-for="option in revisionOptions"
              :key="option.value"
              :label="option.label"
              :value="option.value"
            />
          </el-select>
          <el-button :disabled="revisionOptions.length < 2" @click="openDiff">
            <el-icon><DocumentCopy /></el-icon> 版本对比
          </el-button>
          <el-button :loading="loading" @click="load">
            <el-icon><Refresh /></el-icon> 刷新
          </el-button>
        </div>
      </div>
    </el-card>

    <el-alert
      v-if="hubOffline"
      type="error"
      :closable="false"
      show-icon
      style="margin-bottom: 16px"
      title="连不上控制中台"
      :description="hubOfflineMessage"
    />
    <el-alert
      v-else-if="loadError"
      type="error"
      :closable="false"
      show-icon
      style="margin-bottom: 16px"
      title="加载失败"
      :description="loadError"
    />

    <div v-loading="loading" class="body">
      <!-- 画布 -->
      <el-card class="canvas-card" body-style="padding: 0">
        <template #header>
          <div class="card-header">
            <span>
              <el-icon><Share /></el-icon> 拓扑
              <span v-if="currentRevision" class="muted">
                · {{ revisionLabel(currentRevision) }}
              </span>
            </span>
            <span class="muted small">
              {{ nodeCount }} 个节点 · {{ edgeCount }} 条边
            </span>
          </div>
        </template>
        <div class="graph-container">
          <VueFlow
            :id="CANVAS_ID"
            v-model:nodes="nodes"
            v-model:edges="edges"
            :node-types="nodeTypes"
            :nodes-draggable="false"
            :nodes-connectable="false"
            :edges-updatable="false"
            :default-edge-options="defaultEdgeOptions"
            :min-zoom="0.3"
            :max-zoom="2"
            @init="onCanvasInit"
            @nodes-initialized="applyFitView"
            @node-click="onNodeClick"
            @pane-click="selectedNodeId = ''"
          >
            <Background :gap="16" :size="1" pattern-color="#e0e0e0" />
            <Controls :show-interactive="false" />
            <MiniMap :node-color="minimapNodeColor" :mask-color="minimapMaskColor" />
          </VueFlow>
          <el-empty
            v-if="!loading && nodeCount === 0"
            class="canvas-empty"
            description="这个修订里没有节点"
          />
        </div>
      </el-card>

      <!-- 侧栏 -->
      <div class="side">
        <!-- 选中的节点 -->
        <el-card v-if="selectedNode" class="side-card">
          <template #header>
            <div class="card-header">
              <span><el-icon><Aim /></el-icon> 节点 {{ selectedNode.id }}</span>
            </div>
          </template>
          <el-descriptions :column="1" border size="small">
            <el-descriptions-item label="插件">
              <span class="mono">{{ selectedNode.plugin }}</span>
            </el-descriptions-item>
            <el-descriptions-item label="版本约束">
              <span v-if="selectedNode.version" class="mono">{{ selectedNode.version }}</span>
              <span v-else class="muted">跟随最新版本</span>
            </el-descriptions-item>
            <el-descriptions-item label="节点超时">
              <span v-if="selectedNode.timeout_ms">{{ selectedNode.timeout_ms }} ms</span>
              <span v-else class="muted">不单独设限</span>
            </el-descriptions-item>
            <el-descriptions-item label="失败重试">
              <span v-if="selectedNode.retries">{{ selectedNode.retries }} 次</span>
              <span v-else class="muted">不重试</span>
            </el-descriptions-item>
            <el-descriptions-item label="上游">
              <span v-if="upstreams.length" class="mono">{{ upstreams.join(", ") }}</span>
              <span v-else class="muted">无（入口节点）</span>
            </el-descriptions-item>
            <el-descriptions-item label="下游">
              <span v-if="downstreams.length" class="mono">{{ downstreams.join(", ") }}</span>
              <span v-else class="muted">无（叶子节点）</span>
            </el-descriptions-item>
          </el-descriptions>
        </el-card>
        <el-card v-else class="side-card">
          <el-empty description="点画布上的节点看它的配置" :image-size="60" />
        </el-card>

        <!-- 校验结果 -->
        <el-card class="side-card">
          <template #header>
            <div class="card-header">
              <span><el-icon><CircleCheck /></el-icon> 保存时的校验</span>
              <el-tag v-if="issues.length" size="small" :type="hasError ? 'danger' : 'warning'" effect="plain">
                {{ issues.length }} 条
              </el-tag>
              <el-tag v-else-if="currentRevision" size="small" type="success" effect="plain">
                无问题
              </el-tag>
            </div>
          </template>

          <template v-if="issues.length">
            <div v-for="(issue, index) in issues" :key="index" class="issue">
              <el-tag
                size="small"
                :type="issue.severity === 'Error' ? 'danger' : 'warning'"
                effect="plain"
              >
                {{ issue.severity === "Error" ? "阻断" : "提示" }}
              </el-tag>
              <div class="issue-body">
                <div class="issue-subject mono">{{ issue.subject }}</div>
                <div class="issue-detail">{{ issue.detail }}</div>
              </div>
            </div>
            <el-alert
              v-if="hasError"
              type="error"
              :closable="false"
              style="margin-top: 10px"
              title="有阻断性问题"
              description="这份修订发布不了。保存不会被拒，但发布时中台会重新校验并拦下。"
            />
          </template>
          <el-empty
            v-else-if="currentRevision"
            description="这次保存没有发现结构性问题"
            :image-size="60"
          />
          <el-empty v-else description="没有修订" :image-size="60" />
        </el-card>

        <!-- 修订信息 -->
        <el-card v-if="currentRevision" class="side-card">
          <template #header>
            <div class="card-header">
              <span><el-icon><Clock /></el-icon> 修订信息</span>
            </div>
          </template>
          <el-descriptions :column="1" border size="small">
            <el-descriptions-item label="修订号">rev {{ currentRevision.revision }}</el-descriptions-item>
            <el-descriptions-item label="状态">
              <el-tag size="small" :type="statusTagType(currentRevision.status)" effect="plain">
                {{ statusText(currentRevision.status) }}
              </el-tag>
            </el-descriptions-item>
            <el-descriptions-item label="保存人">
              {{ currentRevision.created_by || "—" }}
            </el-descriptions-item>
            <el-descriptions-item label="保存于">
              {{ formatTime(currentRevision.created_at) }}
            </el-descriptions-item>
            <el-descriptions-item label="发布于">
              {{ currentRevision.published_at ? formatTime(currentRevision.published_at) : "—" }}
            </el-descriptions-item>
          </el-descriptions>

          <!--
            回滚只做到「存成草稿」为止，不顺手发布：发布改变生产流量走向，
            按设计那是人点头的动作。把两步合成一步会让「回滚」变成一个
            点下去就生效的按钮，而它改的是线上在跑的编排。
          -->
          <el-button
            v-if="!isCurrentDraft"
            size="small"
            style="margin-top: 12px; width: 100%"
            :loading="rollingBack"
            @click="rollbackTo(currentRevision)"
          >
            <el-icon><RefreshLeft /></el-icon> 把这一版存为草稿
          </el-button>
          <div v-else class="muted small" style="margin-top: 12px">
            这就是当前草稿。改它请去编辑器。
          </div>
        </el-card>
      </div>
    </div>

    <!-- 版本对比。比的是 definition 的 JSON 文本——
         「这一版到底改了什么」在结构化视图里看得出来的是拓扑，看不出的是
         超时、重试、版本约束这些字段上的一字之差，而它们恰恰最容易改错 -->
    <el-dialog v-model="diffVisible" title="版本对比" width="86%" top="5vh" destroy-on-close>
      <div class="diff-toolbar">
        <el-select v-model="diffLeft" style="width: 260px">
          <el-option v-for="option in revisionOptions" :key="option.value" :label="option.label" :value="option.value" />
        </el-select>
        <el-icon class="diff-arrow"><Right /></el-icon>
        <el-select v-model="diffRight" style="width: 260px">
          <el-option v-for="option in revisionOptions" :key="option.value" :label="option.label" :value="option.value" />
        </el-select>
        <span class="muted small" style="margin-left: 12px">
          只比 definition；校验结果与时间戳不在比较范围内
        </span>
      </div>

      <div class="diff-body">
        <CodeDiff
          :old-string="diffLeftText"
          :new-string="diffRightText"
          output-format="side-by-side"
          language="json"
          :hide-header="true"
        />
      </div>
    </el-dialog>
  </div>
</template>

<script setup>
import { computed, markRaw, onMounted, ref, watch } from "vue";
import { useRoute, useRouter } from "vue-router";
import { ElMessage, ElMessageBox } from "element-plus";
import { VueFlow, MarkerType } from "@vue-flow/core";
import { Background } from "@vue-flow/background";
import { Controls } from "@vue-flow/controls";
import { MiniMap } from "@vue-flow/minimap";
import dagre from "dagre";
import { getFlow, saveDraft, HubError } from "@/api/hub";
import { formatTime, statusTagType, statusText } from "../shared/format";
import { estimateNodeSize, LAYOUT_GRAPH_OPTIONS } from "../shared/graph";
import { useTheme } from "@/composables/useTheme";
import HubNode from "./HubNode.vue";

import "@vue-flow/core/dist/style.css";
import "@vue-flow/core/dist/theme-default.css";
import "@vue-flow/controls/dist/style.css";
import "@vue-flow/minimap/dist/style.css";

const route = useRoute();
const router = useRouter();

const flowName = computed(() => String(route.params.name || ""));
/** 画布 id。一次只渲染一张拓扑画布，用固定值即可 */
const CANVAS_ID = "hub-flow-canvas";
// Vue Flow 用固定 id：一次只渲染一张拓扑画布，按 flow 名做 id 反而要处理路由切换时的重挂载
/**
 * 画布实例。
 *
 * **不能用 `useVueFlow({ id })`**：在父组件 setup 里按 id 拿到的 store 与
 * `<VueFlow>` 组件内部实际使用的那个不是同一个实例——调它的 `fitView` 会「成功」
 * 但画布纹丝不动（实测：调用前后 zoom 都是 1，而 DOM 上的缩放是 0.49）。
 * `@init` 回调给的是画布真正在用的 store，从它上面取。
 */
const canvas = ref(null);

const { isDark } = useTheme();

/**
 * 小地图的颜色要跟着主题走。
 *
 * 它默认是亮色的：在暗色主题下这是一块刺眼的白板，而且它的视口遮罩是浅色，
 * 在白底上几乎看不出来——小地图唯一的作用「当前视野在哪」就此失效。
 */
const minimapNodeColor = computed(() => (isDark.value ? "#6b7280" : "#B0B0B0"));
const minimapMaskColor = computed(() =>
  isDark.value ? "rgba(0, 0, 0, 0.55)" : "rgba(240, 242, 245, 0.6)",
);

function onCanvasInit(instance) {
  canvas.value = instance;
}

/**
 * 把视野适配到所有节点。
 *
 * **必须由 `@nodes-initialized` 触发，不能放在 layout 之后的 `nextTick` 里**：
 * `fitView` 靠每个节点**测量出来的 DOM 尺寸**算边界，而节点刚进 `nodes.value` 时
 * Vue Flow 还没量过它们，这时调用是个 no-op——zoom 前后都是 1，画布一动不动。
 * `nodes-initialized` 正是「节点都量好了」这个时刻。
 *
 * `padding` 从 0.2 收到 0.08：它决定 fitView 认为「可用的画布」有多大。
 * 留 20% 的边距意味着同样一张图要多缩 20% 才装得下——一条 7 节点链因此被压到 0.5
 * （节点 75px，节点名只有 6.5px，读不出来），而画布左右其实各空着 75px。
 * 这点边距够节点不贴边了，也不必再浪费两成宽度。
 *
 * `minZoom` 是「宁可不全，也要看清」的地板：内容真的装不下时让它溢出、由用户平移，
 * 而不是缩到一片灰糊。`maxZoom: 1` 反方向同理——两个节点时把卡片拉成两倍大只会显得空。
 */
function applyFitView() {
  if (!canvas.value || !nodes.value.length) return;
  canvas.value.fitView({ padding: 0.08, minZoom: 0.5, maxZoom: 1 });
}

const loading = ref(false);
const hubOffline = ref(false);
const hubOfflineMessage = ref("");
const loadError = ref("");
const detail = ref(null);
const selectedRevisionId = ref("");
const selectedNodeId = ref("");

const diffVisible = ref(false);
const diffLeft = ref("");
const diffRight = ref("");
const rollingBack = ref(false);

const nodes = ref([]);
const edges = ref([]);
// markRaw：Vue Flow 的节点类型表不能是响应式的，否则组件定义会被代理后失效
const nodeTypes = markRaw({ hub: markRaw(HubNode) });
const defaultEdgeOptions = {
  type: "smoothstep",
  style: { stroke: "#A0A0A0", strokeWidth: 2 },
  markerEnd: { type: MarkerType.ArrowClosed, color: "#A0A0A0", width: 18, height: 18 },
};

const published = computed(() => detail.value?.published || null);

/** 修订下拉的选项：草稿排最前（它是最常看的），其余按修订号倒序。 */
const revisionOptions = computed(() => {
  if (!detail.value) return [];
  const options = [];
  if (detail.value.draft) {
    options.push({
      value: `rev-${detail.value.draft.id}`,
      label: `草稿 · rev ${detail.value.draft.revision}`,
    });
  }
  if (detail.value.published) {
    options.push({
      value: `rev-${detail.value.published.id}`,
      label: `已发布 · rev ${detail.value.published.revision}`,
    });
  }
  for (const revision of detail.value.revisions) {
    // 草稿与已发布已经在上面列过，避免同一个修订出现两次
    if (revision.id === detail.value.draft?.id) continue;
    if (revision.id === detail.value.published?.id) continue;
    options.push({
      value: `rev-${revision.id}`,
      label: `历史 · rev ${revision.revision}（${statusText(revision.status)}）`,
    });
  }
  return options;
});

const currentRevision = computed(() => {
  if (!detail.value) return null;
  if (!selectedRevisionId.value) {
    return detail.value.draft || detail.value.published || detail.value.revisions[0] || null;
  }
  const id = Number(selectedRevisionId.value.replace("rev-", ""));
  const all = [
    detail.value.draft,
    detail.value.published,
    ...detail.value.revisions,
  ].filter(Boolean);
  return all.find((r) => r.id === id) || null;
});

const definition = computed(() => currentRevision.value?.definition || null);

// 计数按**定义**算，不按渲染出来的算。
//
// Vue Flow 会丢掉两端不存在的边（比如指向一个不存在的节点的悬空边），
// 拿渲染结果计数的话，一条「1 节点 1 边」的编排会显示成「1 个节点 · 0 条边」——
// 看的人会以为自己少存了一条边。定义里有什么就报什么，接不上的部分
// 由校验列表回答（那里会给出 UnknownNode）。
const nodeCount = computed(() => definition.value?.nodes?.length || 0);
const edgeCount = computed(() => definition.value?.edges?.length || 0);

/** 校验结果。中台存的是 FlowIssue 数组；结构对不上时按「没有」处理而不是崩掉。 */
const issues = computed(() => {
  const validation = currentRevision.value?.validation;
  return Array.isArray(validation) ? validation : [];
});
const hasError = computed(() => issues.value.some((i) => i.severity === "Error"));

const selectedNode = computed(() => {
  if (!selectedNodeId.value || !definition.value) return null;
  return (definition.value.nodes || []).find((n) => n.id === selectedNodeId.value) || null;
});

const upstreams = computed(() =>
  (definition.value?.edges || [])
    .filter((e) => e.to === selectedNodeId.value)
    .map((e) => e.from),
);
const downstreams = computed(() =>
  (definition.value?.edges || [])
    .filter((e) => e.from === selectedNodeId.value)
    .map((e) => e.to),
);

function revisionLabel(revision) {
  if (!revision) return "";
  if (revision.id === detail.value?.draft?.id) return `草稿 rev ${revision.revision}`;
  return `rev ${revision.revision} · ${statusText(revision.status)}`;
}

/**
 * 把 flow 定义排成一张从左到右的图。
 *
 * 用 dagre 而不是手算坐标：编排是 DAG 不是树，同层可能有多个节点、
 * 也可能有节点被多条路径指向，手算会很快失控。rankdir 取 LR——
 * 「数据从左流到右」比从上到下更符合读代码时的心智。
 */
/**
 * 估算节点占多大 —— 实现的说明见 `shared/graph.js`。
 *
 * 它与 `HubNode.vue` 的宽度约束（min 150 / max 240，超出折行）必须一致：
 * 这里原来写死 `190 × 78`，而节点宽度其实由 id 长度决定，于是一个 50 字符的 id
 * 渲染到 550px、布局却还按 190px 排，后一个节点整个落进前一个的矩形里。
 */

function layout(definition) {
  const flowNodes = definition?.nodes || [];
  const flowEdges = definition?.edges || [];
  if (!flowNodes.length) {
    nodes.value = [];
    edges.value = [];
    return;
  }
  const withIncoming = new Set(flowEdges.map((e) => e.to));
  const withOutgoing = new Set(flowEdges.map((e) => e.from));

  const graph = new dagre.graphlib.Graph();
  graph.setGraph(LAYOUT_GRAPH_OPTIONS);
  graph.setDefaultEdgeLabel(() => ({}));

  const sizes = new Map(flowNodes.map((node) => [node.id, estimateNodeSize(node.id)]));
  for (const node of flowNodes) {
    graph.setNode(node.id, sizes.get(node.id));
  }
  for (const edge of flowEdges) {
    graph.setEdge(edge.from, edge.to);
  }
  dagre.layout(graph);

  // 校验问题点名了哪条边、哪个节点，就在图上把它们标出来。
  //
  // 右栏写的是「step2 → step3」，而画布上六个节点长得一模一样、
  // 全靠 id 认——不标的话，读的人得在图上把这两个 id 找出来再顺着连线找那条边。
  // 边的 subject 用 ` → ` 连接两端（中台 `FlowIssue` 的写法），节点则直接是 id。
  const badEdges = new Set();
  const badNodes = new Set();
  for (const issue of issues.value) {
    if (issue.severity !== "Error") continue;
    const subject = String(issue.subject || "");
    const parts = subject.split("→").map((part) => part.trim());
    if (parts.length === 2 && parts[0] && parts[1]) {
      badEdges.add(`${parts[0]}->${parts[1]}`);
    } else if (subject) {
      badNodes.add(subject);
    }
  }

  nodes.value = flowNodes.map((node) => {
    const positioned = graph.node(node.id);
    const size = sizes.get(node.id);
    return {
      id: node.id,
      type: "hub",
      position: {
        x: positioned ? positioned.x - size.width / 2 : 0,
        y: positioned ? positioned.y - size.height / 2 : 0,
      },
      data: {
        label: node.id,
        plugin: node.plugin,
        version: node.version,
        retries: node.retries,
        isEntry: !withIncoming.has(node.id),
        isExit: !withOutgoing.has(node.id),
        hasError: badNodes.has(node.id),
      },
    };
  });

  edges.value = flowEdges.map((edge) => {
    const bad = badEdges.has(`${edge.from}->${edge.to}`);
    return {
      id: `e-${edge.from}-${edge.to}`,
      source: edge.from,
      target: edge.to,
      ...defaultEdgeOptions,
      ...(bad
        ? {
            style: { stroke: "#F56C6C", strokeWidth: 2.5 },
            markerEnd: {
              type: MarkerType.ArrowClosed,
              color: "#F56C6C",
              width: 18,
              height: 18,
            },
          }
        : {}),
    };
  });

  // 视野适配交给 `@nodes-initialized`（见 applyFitView 的说明）——
  // 这里赋值完之后 Vue Flow 会重新测量节点并触发那个事件。
}

function onNodeClick({ node }) {
  selectedNodeId.value = node.id;
}

function goBack() {
  router.push("/flows");
}

async function load() {
  loading.value = true;
  loadError.value = "";
  try {
    detail.value = await getFlow(flowName.value);
    hubOffline.value = false;
    // 默认看草稿：排障时最关心「改了什么还没发」
    const preferred = detail.value.draft || detail.value.published || detail.value.revisions[0];
    selectedRevisionId.value = preferred ? `rev-${preferred.id}` : "";
  } catch (error) {
    if (error instanceof HubError && error.isNetwork) {
      hubOffline.value = true;
      hubOfflineMessage.value = error.message;
    } else if (error instanceof HubError && error.isNotFound) {
      loadError.value = `flow「${flowName.value}」不存在`;
    } else {
      loadError.value = error.message || "加载失败";
    }
    detail.value = null;
  } finally {
    loading.value = false;
  }
}

// 切换修订要重排画布，并且清掉上一个修订里选中的节点。
//
// **校验结果也要盯着**：`layout()` 会按 issues 给边和节点上标记，
// 只盯修订 id 的话，保存草稿之后新出现的阻断问题不会反映到画布上。
watch(
  () => [currentRevision.value?.id, issues.value],
  () => {
    selectedNodeId.value = "";
    layout(definition.value);
  },
);

onMounted(load);
</script>

<style scoped>
.hub-flow-detail {
  display: flex;
  flex-direction: column;
  height: 100%;
  padding: 16px;
  box-sizing: border-box;
}

.title-card {
  margin-bottom: 16px;
  flex-shrink: 0;
}

.page-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
}

.header-left {
  display: flex;
  align-items: flex-start;
  gap: 12px;
}

.header-title {
  display: flex;
  align-items: center;
  gap: 10px;
  font-size: 20px;
  font-weight: 600;
}

.header-subtitle {
  margin-top: 6px;
  font-size: 13px;
  color: var(--el-text-color-secondary);
}

.header-actions {
  display: flex;
  align-items: center;
  gap: 12px;
}

/* 画布与侧栏并排；画布撑满剩余高度 */
.body {
  display: flex;
  gap: 16px;
  flex: 1;
  min-height: 520px;
}

.canvas-card {
  flex: 1;
  display: flex;
  flex-direction: column;
  min-width: 0;
}

.canvas-card :deep(.el-card__body) {
  flex: 1;
  min-height: 0;
  display: flex;
}

.graph-container {
  position: relative;
  width: 100%;
  height: 100%;
  min-height: 460px;
}

/* 小地图的**底色**由 CSS 控制（组件 props 只管节点与遮罩）。
   不覆盖的话它在暗色主题下就是一块白板 */
:deep(.vue-flow__minimap) {
  background-color: var(--el-bg-color-overlay);
}

.canvas-empty {
  position: absolute;
  inset: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  pointer-events: none;
}

.side {
  width: 360px;
  flex-shrink: 0;
  display: flex;
  flex-direction: column;
  gap: 16px;
  overflow-y: auto;
  max-height: 100%;
}

/* 卡片**不许被压缩**。
   `.side` 是 flex column，子项默认 `flex-shrink: 1`：空间不够时它们会被压扁，
   而 Element Plus 的 `.el-card` 带 `overflow: hidden`，于是超出卡片高度的内容
   被静静吃掉——「有阻断性问题，发布会被拒」这句提示就此消失，用户只能看到
   一个空白的校验卡。
   更坏的是压扁之后容器**不再溢出**，`.side` 上那条 `overflow-y: auto` 永远不触发，
   被裁的内容连滚都滚不到。
   `flex-shrink: 0` 让卡片保持自然高度、由 `.side` 滚动来兜底，两者才配套。 */
.side-card {
  flex-shrink: 0;
}

.side-card :deep(.el-card__header) {
  padding: 12px 16px;
}

.card-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
}

.mono {
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
}

.muted {
  color: var(--el-text-color-secondary);
}

.small {
  font-size: 12px;
}

.issue {
  display: flex;
  gap: 8px;
  padding: 8px 0;
  border-bottom: 1px solid var(--el-border-color-lighter);
}

.issue:last-of-type {
  border-bottom: none;
}

.issue-body {
  min-width: 0;
}

.issue-subject {
  font-size: 12px;
  font-weight: 600;
}

.issue-detail {
  margin-top: 2px;
  font-size: 12px;
  color: var(--el-text-color-secondary);
  line-height: 1.5;
}
</style>
