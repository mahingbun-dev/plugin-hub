<template>
  <div class="hub-editor">
    <el-card class="title-card">
      <div class="page-header">
        <div class="header-left">
          <el-button link @click="goBack">
            <el-icon><ArrowLeft /></el-icon> 返回
          </el-button>
          <div>
            <div class="header-title">
              <el-input
                v-if="nameEditable"
                v-model="editName"
                class="mono-input name-input"
                placeholder="order-intake"
                @change="markDirty"
                @keyup.enter="$event.target.blur()"
              />
              <span v-else class="mono">{{ flowName }}</span>
              <el-tag v-if="isNew" type="info" effect="plain" size="small">新流程</el-tag>
              <el-tag v-else-if="dirty" type="warning" effect="plain" size="small">有未保存的改动</el-tag>
              <el-tag v-if="publishedRevision > 0" type="success" effect="plain" size="small">
                已发布 rev {{ publishedRevision }}
              </el-tag>
            </div>
            <div class="header-subtitle">
              从左侧加节点、拖动连线。保存草稿不校验拦截，发布时才必须没有阻断性问题。
              <template v-if="nameEditable">改了名字的话，点保存时一并生效。</template>
            </div>
          </div>
        </div>
        <div class="header-actions">
          <el-button :disabled="saving" @click="save">保存草稿</el-button>
          <el-button
            type="primary"
            :disabled="saving || blocked"
            :loading="publishing"
            @click="publish"
          >
            发布
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

    <div class="body">
      <!-- 左：可用的插件 -->
      <el-card class="panel side-panel">
        <template #header>
          <div class="card-header">
            <span><el-icon><Grid /></el-icon> 插件</span>
            <el-input
              v-model="pluginKeyword"
              size="small"
              placeholder="过滤"
              clearable
              style="width: 110px"
            />
          </div>
        </template>
        <div v-loading="pluginsLoading" class="plugin-list">
          <div
            v-for="plugin in filteredPlugins"
            :key="plugin.name"
            class="plugin-item"
            @click="addNode(plugin)"
          >
            <div class="plugin-name mono">{{ plugin.name }}</div>
            <div class="muted small">
              {{ plugin.version_count }} 个版本 · {{ plugin.instance_count }} 个实例
            </div>
          </div>
          <el-empty
            v-if="!pluginsLoading && !filteredPlugins.length"
            description="没有可用插件"
            :image-size="60"
          />
        </div>
      </el-card>

      <!-- 中：画布 -->
      <el-card class="canvas-card" body-style="padding: 0">
        <template #header>
          <div class="card-header">
            <span>
              <el-icon><Share /></el-icon> 编排
              <span class="muted small"> · {{ nodes.length }} 节点 / {{ edges.length }} 连线</span>
            </span>
            <el-button size="small" :disabled="!nodes.length" @click="autoLayout">
              <el-icon><MagicStick /></el-icon> 重新排版
            </el-button>
          </div>
        </template>
        <div class="graph-container">
          <VueFlow
            :id="CANVAS_ID"
            v-model:nodes="nodes"
            v-model:edges="edges"
            :node-types="nodeTypes"
            :default-edge-options="defaultEdgeOptions"
            :min-zoom="0.3"
            :max-zoom="2"
            @init="onCanvasInit"
            @nodes-initialized="applyFitView"
            @node-click="onNodeClick"
            @pane-click="selectedNodeId = ''"
            @connect="onConnect"
            @edge-click="onEdgeClick"
          >
            <Background :gap="16" :size="1" pattern-color="#e0e0e0" />
            <Controls />
            <MiniMap node-color="#B0B0B0" />
          </VueFlow>
          <div v-if="!nodes.length" class="canvas-hint">
            <el-empty description="从左侧点一个插件开始编排" :image-size="80" />
          </div>
        </div>
      </el-card>

      <!-- 右：属性与校验 -->
      <div class="panel side-panel right">
        <el-card class="side-card">
          <template #header>
            <div class="card-header">
              <span><el-icon><Setting /></el-icon> 属性</span>
            </div>
          </template>

          <template v-if="selectedNode">
            <el-form label-width="76px" size="small">
              <el-form-item label="节点 id">
                <el-input v-model="selectedNode.data.label" class="mono-input" @change="onNodeIdChange" />
              </el-form-item>
              <el-form-item label="插件">
                <el-select
                  v-model="selectedNode.data.plugin"
                  filterable
                  placeholder="选择插件"
                  style="width: 100%"
                  @change="onPluginChange"
                >
                  <el-option v-for="p in plugins" :key="p.name" :label="p.name" :value="p.name" />
                </el-select>
              </el-form-item>
              <el-form-item label="版本">
                <el-select
                  v-model="selectedNode.data.version"
                  clearable
                  placeholder="跟随最新版本"
                  style="width: 100%"
                  @change="markDirty"
                >
                  <el-option
                    v-for="v in versionsOf(selectedNode.data.plugin)"
                    :key="v"
                    :label="v"
                    :value="v"
                  />
                </el-select>
              </el-form-item>
              <el-form-item label="超时">
                <el-input-number
                  v-model="selectedNode.data.timeout_ms"
                  :min="1"
                  :max="300000"
                  :step="1000"
                  controls-position="right"
                  style="width: 100%"
                  @change="markDirty"
                />
              </el-form-item>
              <el-form-item label="重试">
                <el-input-number
                  v-model="selectedNode.data.retries"
                  :min="0"
                  :max="10"
                  controls-position="right"
                  style="width: 100%"
                  @change="markDirty"
                />
              </el-form-item>
            </el-form>
            <div class="muted small">
              超时是上限不是保证：真正生效的是它与信封剩余预算里更小的那个。
              重试只对可重试的失败生效（超时与校验拒绝都不重试）。
            </div>
            <el-button
              size="small"
              type="danger"
              plain
              style="margin-top: 10px; width: 100%"
              @click="removeNode(selectedNode.id)"
            >
              删除这个节点
            </el-button>
          </template>

          <template v-else-if="selectedEdge">
            <el-descriptions :column="1" border size="small">
              <el-descriptions-item label="从">{{ selectedEdge.source }}</el-descriptions-item>
              <el-descriptions-item label="到">{{ selectedEdge.target }}</el-descriptions-item>
            </el-descriptions>
            <el-button
              size="small"
              type="danger"
              plain
              style="margin-top: 10px; width: 100%"
              @click="removeEdge(selectedEdge.id)"
            >
              删除这条连线
            </el-button>
          </template>

          <el-empty v-else description="点节点或连线看它的配置" :image-size="60" />
        </el-card>

        <el-card class="side-card">
          <template #header>
            <div class="card-header">
              <span><el-icon><CircleCheck /></el-icon> 校验</span>
              <el-tag v-if="issues.length" size="small" :type="hasError ? 'danger' : 'warning'" effect="plain">
                {{ issues.length }} 条
              </el-tag>
              <el-tag v-else-if="validated" size="small" type="success" effect="plain">无问题</el-tag>
            </div>
          </template>

          <div v-if="localIssues.length" class="issues">
            <div v-for="(issue, index) in localIssues" :key="`local-${index}`" class="issue">
              <el-tag size="small" type="info" effect="plain">本地</el-tag>
              <div class="issue-body">
                <div class="issue-subject mono">{{ issue.subject }}</div>
                <div class="issue-detail">{{ issue.detail }}</div>
              </div>
            </div>
          </div>

          <div v-if="issues.length" class="issues">
            <div v-for="(issue, index) in issues" :key="`remote-${index}`" class="issue">
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
          </div>

          <el-alert
            v-if="hasError"
            type="error"
            :closable="false"
            style="margin-top: 10px"
            title="有阻断性问题，发布会被拒"
          />

          <el-empty
            v-if="!issues.length && !localIssues.length"
            :description="validated ? '没有发现问题' : '保存后这里会显示中台的校验结果'"
            :image-size="60"
          />
        </el-card>
      </div>
    </div>
  </div>
</template>

<script setup>
import { computed, markRaw, onMounted, ref } from "vue";
import { useRoute, useRouter } from "vue-router";
import { ElMessage, ElMessageBox } from "element-plus";
import { VueFlow, MarkerType } from "@vue-flow/core";
import { Background } from "@vue-flow/background";
import { Controls } from "@vue-flow/controls";
import { MiniMap } from "@vue-flow/minimap";
import dagre from "dagre";
import { getFlow, getPlugin, listPlugins, publishFlow, renameFlow, saveDraft, HubError } from "@/api/hub";
import { estimateNodeSize, LAYOUT_GRAPH_OPTIONS } from "../shared/graph";
import { NAME_HINT, isValidFlowName } from "../shared/name";
import EditorNode from "./EditorNode.vue";

import "@vue-flow/core/dist/style.css";
import "@vue-flow/core/dist/theme-default.css";
import "@vue-flow/controls/dist/style.css";
import "@vue-flow/minimap/dist/style.css";

const CANVAS_ID = "hub-flow-editor";

const route = useRoute();
const router = useRouter();

const flowName = computed(() => String(route.params.name || ""));

/** 名字输入框的草稿。保存（或发布前的自动保存）时才真正生效 */
const editName = ref("");

/**
 * 名字能不能改：没落库的新流程随意改；落库了但从未发布过也能改
 * （走 rename 接口）。发布过的不能改——调用方与触发器都按名字引用它。
 */
const nameEditable = computed(() => isNew.value || publishedRevision.value === 0);

const loading = ref(false);
const saving = ref(false);
const publishing = ref(false);
const hubOffline = ref(false);
const hubOfflineMessage = ref("");
const isNew = ref(false);
const dirty = ref(false);
const publishedRevision = ref(0);
const validated = ref(false);

const nodes = ref([]);
const edges = ref([]);
const plugins = ref([]);
const pluginsLoading = ref(false);
const pluginKeyword = ref("");
/** plugin@version → {produces, consumes}，连线的即时提示用它 */
const contracts = ref({});

const issues = ref([]);
const selectedNodeId = ref("");
const selectedEdgeId = ref("");

const canvas = ref(null);
const nodeTypes = markRaw({ editor: markRaw(EditorNode) });
const defaultEdgeOptions = {
  type: "smoothstep",
  style: { stroke: "#A0A0A0", strokeWidth: 2 },
  markerEnd: { type: MarkerType.ArrowClosed, color: "#A0A0A0", width: 18, height: 18 },
};

const filteredPlugins = computed(() => {
  const kw = pluginKeyword.value.trim().toLowerCase();
  if (!kw) return plugins.value;
  return plugins.value.filter((p) => p.name.toLowerCase().includes(kw));
});

const selectedNode = computed(
  () => nodes.value.find((n) => n.id === selectedNodeId.value) || null,
);
const selectedEdge = computed(
  () => edges.value.find((e) => e.id === selectedEdgeId.value) || null,
);

const hasError = computed(() => issues.value.some((i) => i.severity === "Error"));
const blocked = computed(() => hasError.value);

/** 本地即时提示：中台要等保存才回答，而这些在连线的当下就能看出来 */
const localIssues = computed(() => {
  const found = [];
  const ids = new Set();
  for (const node of nodes.value) {
    const id = node.data.label;
    if (!id) {
      found.push({ subject: "未命名节点", detail: "节点 id 不能为空" });
      continue;
    }
    if (ids.has(id)) {
      found.push({ subject: id, detail: "节点 id 重复，flow 内必须唯一" });
    }
    ids.add(id);
    if (!node.data.plugin) {
      found.push({ subject: id, detail: "还没选插件" });
    }
  }
  if (nodes.value.length > 32) {
    found.push({
      subject: "flow",
      detail: `节点数 ${nodes.value.length} 超过上限 32：编排是给人看、给 agent 读的，超过这个规模该拆成多条`,
    });
  }
  return found;
});

function markDirty() {
  dirty.value = true;
  // 改了结构之后上一次的校验结果就过期了——继续显示它等于在骗人
  issues.value = [];
  validated.value = false;
}

function versionsOf(pluginName) {
  const versions = new Set();
  for (const key of Object.keys(contracts.value)) {
    const [name, version] = key.split("@");
    if (name === pluginName) versions.add(version);
  }
  return [...versions];
}

/** 生成一个没被占用的节点 id */
function nextNodeId() {
  const used = new Set(nodes.value.map((n) => n.data.label));
  let index = nodes.value.length + 1;
  while (used.has(`node${index}`)) index += 1;
  return `node${index}`;
}

function addNode(plugin) {
  const id = nextNodeId();
  nodes.value = [
    ...nodes.value,
    {
      id,
      type: "editor",
      position: { x: 80 + (nodes.value.length % 4) * 220, y: 80 + Math.floor(nodes.value.length / 4) * 130 },
      data: {
        label: id,
        plugin: plugin.name,
        version: undefined,
        timeout_ms: undefined,
        retries: undefined,
        invalid: false,
        // 节点上的删除按钮要能回调到这里；模板里没有 this，用闭包带上 id
        onRemove: () => removeNode(id),
      },
    },
  ];
  onNodeIdChange();
  selectedNodeId.value = id;
  markDirty();
}

function removeNode(id) {
  nodes.value = nodes.value.filter((n) => n.id !== id);
  // 连到它的边也要一起清掉：留下悬空边会让 Vue Flow 抛错，
  // 而且那种边在任何渲染下都没有意义
  edges.value = edges.value.filter((e) => e.source !== id && e.target !== id);
  if (selectedNodeId.value === id) selectedNodeId.value = "";
  markDirty();
}

function removeEdge(id) {
  edges.value = edges.value.filter((e) => e.id !== id);
  if (selectedEdgeId.value === id) selectedEdgeId.value = "";
  markDirty();
}

/**
 * 节点 id 改了要同步到 Vue Flow 的节点 key 与所有边上。
 *
 * 这两处不同步的话：边上引用的还是旧 id，渲染时找不到源/目标节点，
 * 那条边会静静消失——而用户以为自己只是改了个名字。
 */
function onNodeIdChange() {
  for (const node of nodes.value) {
    const label = node.data.label;
    if (node.id !== label) {
      const oldId = node.id;
      for (const edge of edges.value) {
        if (edge.source === oldId) edge.source = label;
        if (edge.target === oldId) edge.target = label;
      }
      node.id = label;
    }
  }
  if (selectedNodeId.value) {
    const current = nodes.value.find((n) => n.id === selectedNodeId.value);
    if (!current) selectedNodeId.value = "";
  }
  markDirty();
}

function onPluginChange() {
  // 换了插件，之前选的版本多半不属于它了——留着会变成一个查不到的版本约束
  const node = selectedNode.value;
  if (node && !versionsOf(node.data.plugin).includes(node.data.version)) {
    node.data.version = undefined;
  }
  markDirty();
}

function onNodeClick({ node }) {
  selectedNodeId.value = node.id;
  selectedEdgeId.value = "";
}

function onEdgeClick({ edge }) {
  selectedEdgeId.value = edge.id;
  selectedNodeId.value = "";
}

/**
 * 连线。
 *
 * 本地只拦「一定不成立」的：自环、重复边、成环。契约兼容不做硬拦截——
 * 那条规则的真相在中台（字段级兼容检查），前端复刻一份迟早会漂移；
 * 这里只在明显接不上时给个提示，权威判定留给保存。
 */
function onConnect(connection) {
  const { source, target } = connection;
  if (!source || !target) return;
  if (source === target) {
    ElMessage.warning("不能连到自己");
    return;
  }
  if (edges.value.some((e) => e.source === source && e.target === target)) {
    ElMessage.warning("这条连线已经存在");
    return;
  }
  if (wouldCycle(source, target)) {
    ElMessage.error(`连上 ${source} → ${target} 会形成环，编排必须是 DAG`);
    return;
  }

  edges.value = [
    ...edges.value,
    {
      id: `e-${source}-${target}`,
      source,
      target,
      ...defaultEdgeOptions,
    },
  ];
  markDirty();

  const hint = contractHint(source, target);
  if (hint) ElMessage.warning(hint);
}

/** 加上 source→target 之后会不会成环：从 target 出发能不能走回 source */
function wouldCycle(source, target) {
  const adjacency = new Map();
  for (const edge of edges.value) {
    if (!adjacency.has(edge.source)) adjacency.set(edge.source, []);
    adjacency.get(edge.source).push(edge.target);
  }

  const seen = new Set();
  const stack = [target];
  while (stack.length) {
    const current = stack.pop();
    if (current === source) return true;
    if (seen.has(current)) continue;
    seen.add(current);
    for (const next of adjacency.get(current) || []) stack.push(next);
  }
  return false;
}

/** 上游生产与下游消费有没有交集。拿不到契约时不提示（不知道 ≠ 有问题） */
function contractHint(sourceId, targetId) {
  const source = nodes.value.find((n) => n.id === sourceId);
  const target = nodes.value.find((n) => n.id === targetId);
  if (!source || !target) return "";

  const upstream = contractOf(source.data.plugin, source.data.version);
  const downstream = contractOf(target.data.plugin, target.data.version);
  if (!upstream || !downstream) return "";

  const common = upstream.produces.filter((fq) => downstream.consumes.includes(fq));
  if (common.length) return "";

  return `${sourceId} 与 ${targetId} 之间看不出能接上的消息类型：` +
    `上游生产 ${format(upstream.produces)}，下游消费 ${format(downstream.consumes)}。` +
    `连是能连的，但保存时中台会按契约判定。`;
}

function format(list) {
  return list.length ? `[${list.join(", ")}]` : "[（未声明）]";
}

function contractOf(pluginName, version) {
  if (!pluginName) return null;
  if (version && contracts.value[`${pluginName}@${version}`]) {
    return contracts.value[`${pluginName}@${version}`];
  }
  // 没锁版本时取该插件任一版本的契约做提示——只是提示，不必精确到版本
  for (const [key, value] of Object.entries(contracts.value)) {
    if (key.startsWith(`${pluginName}@`)) return value;
  }
  return null;
}

/** 把画布上的内容转成 hub-flow 的 FlowDefinition。名字用传入的生效名，不读路由 */
function toDefinition(name) {
  return {
    name,
    description: "",
    nodes: nodes.value.map((node) => {
      const item = { id: node.data.label, plugin: node.data.plugin };
      if (node.data.version) item.version = node.data.version;
      if (node.data.timeout_ms) item.timeout_ms = node.data.timeout_ms;
      if (node.data.retries) item.retries = node.data.retries;
      return item;
    }),
    edges: edges.value.map((edge) => ({ from: edge.source, to: edge.target })),
  };
}

/** 用 dagre 把图排开。手工拖过的位置会被覆盖——所以它是个显式按钮而不是自动行为 */
function autoLayout() {
  if (!nodes.value.length) return;

  const graph = new dagre.graphlib.Graph();
  graph.setGraph(LAYOUT_GRAPH_OPTIONS);
  graph.setDefaultEdgeLabel(() => ({}));

  // 尺寸与 `EditorNode.vue` 的宽度约束一致（见 shared/graph.js）。
  // 这里原来写死 200×80，而节点宽度由用户填的 id 决定、没有上限——
  // 一个长 id 就会让两个节点叠在一起。
  const sizes = new Map(
    nodes.value.map((node) => [node.id, estimateNodeSize(node.data.label)]),
  );
  for (const node of nodes.value) graph.setNode(node.id, sizes.get(node.id));
  for (const edge of edges.value) graph.setEdge(edge.source, edge.target);
  dagre.layout(graph);

  for (const node of nodes.value) {
    const positioned = graph.node(node.id);
    if (!positioned) continue;
    const size = sizes.get(node.id);
    node.position = { x: positioned.x - size.width / 2, y: positioned.y - size.height / 2 };
  }
  applyFitView();
}

function onCanvasInit(instance) {
  canvas.value = instance;
}

function applyFitView() {
  if (!canvas.value || !nodes.value.length) return;
  canvas.value.fitView({ padding: 0.2, minZoom: 0.5, maxZoom: 1 });
}

/**
 * 把输入框里的名字变成「当前生效的名字」，保存前调用。
 *
 * - 新流程还没落库，换名字零成本，直接换 URL；
 * - 已存在但从未发布过：走 rename 接口，成功后换 URL；
 * - 名字没变或校验不过时原样返回 / 返回 null。
 *
 * 返回值要一路传给 saveDraft 与 publishFlow——router.replace 之后路由参数
 * 不会立刻更新，这一步里不能依赖 flowName。
 */
async function ensureName() {
  const target = editName.value.trim();
  if (!target) {
    ElMessage.warning("请填流程名");
    return null;
  }
  if (!isValidFlowName(target)) {
    ElMessage.error(NAME_HINT);
    return null;
  }
  if (target === flowName.value) return flowName.value;

  if (!isNew.value) {
    try {
      await renameFlow(flowName.value, target);
    } catch (error) {
      ElMessage.error(error.message || "改名失败");
      return null;
    }
  }
  await router.replace(`/flows/${encodeURIComponent(target)}/edit`);
  return target;
}

/**
 * 保存草稿。发布前的自动保存也走这里。
 *
 * 返回 `{ ok, blocked, name }`：ok 为 false 时不该继续发布；
 * blocked 表示存是存下了、但发布会被中台拦。
 */
async function save() {
  if (!nodes.value.length) {
    ElMessage.warning("至少加一个节点");
    return { ok: false, blocked: false, name: flowName.value };
  }
  const ids = nodes.value.map((n) => n.data.label);
  if (new Set(ids).size !== ids.length) {
    ElMessage.error("节点 id 有重复");
    return { ok: false, blocked: false, name: flowName.value };
  }

  saving.value = true;
  try {
    const name = await ensureName();
    if (!name) return { ok: false, blocked: false, name: flowName.value };

    const result = await saveDraft(name, {
      definition: toDefinition(name),
      description: "",
      createdBy: "console",
    });
    issues.value = result.issues || [];
    validated.value = true;
    dirty.value = false;
    isNew.value = false;
    editName.value = name;
    if (result.blocked) {
      ElMessage.warning("已保存草稿，但有阻断性问题，发布会被拒");
    } else {
      ElMessage.success("草稿已保存");
    }
    return { ok: true, blocked: !!result.blocked, name };
  } catch (error) {
    ElMessage.error(error.message || "保存失败");
    return { ok: false, blocked: false, name: flowName.value };
  } finally {
    saving.value = false;
  }
}

async function publish() {
  try {
    await ElMessageBox.confirm(
      `发布「${editName.value.trim() || flowName.value}」？发布后这条编排就会按新版跑生产流量。`,
      "确认发布",
      { confirmButtonText: "发布", cancelButtonText: "取消", type: "warning" },
    );
  } catch {
    return;
  }

  // 「新建」从不落库，草稿没保存就发布只会得到一句莫名其妙的「未定义」——
  // 所以发布前把未保存的改动先存掉，存失败或有阻断问题就不往下走
  let nameForPublish = flowName.value;
  if (isNew.value || dirty.value) {
    const saved = await save();
    if (!saved.ok) {
      ElMessage.warning("草稿没保存成功，已取消发布");
      return;
    }
    if (saved.blocked) {
      ElMessage.error("草稿有阻断性问题，发布会被拒——先改画布上的问题");
      return;
    }
    nameForPublish = saved.name;
  }

  publishing.value = true;
  try {
    const revision = await publishFlow(nameForPublish);
    publishedRevision.value = revision.revision;
    issues.value = [];
    validated.value = true;
    dirty.value = false;
    ElMessage.success(`已发布 rev ${revision.revision}`);
  } catch (error) {
    // 发布前中台会重新校验，这里是阻断性问题最可能的落地处
    ElMessage.error(error.message || "发布失败");
  } finally {
    publishing.value = false;
  }
}

/** 拉插件与它们的契约：连线时要拿它做即时提示 */
async function loadPlugins() {
  pluginsLoading.value = true;
  try {
    plugins.value = await listPlugins();
    const details = await Promise.all(
      plugins.value.map((p) => getPlugin(p.name).catch(() => null)),
    );
    const map = {};
    for (const detail of details) {
      if (!detail) continue;
      for (const version of detail.versions || []) {
        map[`${detail.name}@${version.version}`] = {
          produces: (version.contracts || [])
            .filter((c) => c.direction === "produces")
            .map((c) => c.fq_name),
          consumes: (version.contracts || [])
            .filter((c) => c.direction === "consumes")
            .map((c) => c.fq_name),
        };
      }
    }
    contracts.value = map;
  } catch (error) {
    if (error instanceof HubError && error.isNetwork) {
      hubOffline.value = true;
      hubOfflineMessage.value = error.message;
    } else {
      ElMessage.error(error.message || "加载插件列表失败");
    }
  } finally {
    pluginsLoading.value = false;
  }
}

/** 把 definition 铺到画布上 */
function loadDefinition(definition) {
  const flowNodes = definition?.nodes || [];
  const flowEdges = definition?.edges || [];
  // 已保存的 definition 里没有坐标——位置是画布的事，不进 DSL
  nodes.value = flowNodes.map((node, index) => ({
    id: node.id,
    type: "editor",
    position: { x: 80 + (index % 4) * 240, y: 80 + Math.floor(index / 4) * 140 },
    data: {
      label: node.id,
      plugin: node.plugin,
      version: node.version,
      timeout_ms: node.timeout_ms,
      retries: node.retries,
      invalid: false,
      onRemove: () => removeNode(node.id),
    },
  }));
  edges.value = flowEdges.map((edge) => ({
    id: `e-${edge.from}-${edge.to}`,
    source: edge.from,
    target: edge.to,
    ...defaultEdgeOptions,
  }));
  autoLayout();
}

async function load() {
  loading.value = true;
  try {
    const detail = await getFlow(flowName.value);
    publishedRevision.value = detail.published_revision || 0;
    // 编辑草稿：草稿是「还没发出去的那一版」，是编辑器的默认工作面
    const definition = detail.draft?.definition || detail.published?.definition;
    if (definition) loadDefinition(definition);
    issues.value = detail.draft?.validation || [];
    validated.value = !!detail.draft;
    isNew.value = false;
    editName.value = flowName.value;
    hubOffline.value = false;
  } catch (error) {
    if (error instanceof HubError && error.isNetwork) {
      hubOffline.value = true;
      hubOfflineMessage.value = error.message;
    } else if (error instanceof HubError && error.isNotFound) {
      // 还不存在就是新建：给一张空画布，保存时中台会把它建出来
      isNew.value = true;
      hubsNewHint();
    } else {
      ElMessage.error(error.message || "加载失败");
    }
  } finally {
    loading.value = false;
  }
}

function hubsNewHint() {
  nodes.value = [];
  edges.value = [];
  issues.value = [];
  validated.value = false;
  editName.value = flowName.value;
}

function goBack() {
  router.push(`/flows/${encodeURIComponent(flowName.value)}`);
}

onMounted(async () => {
  await Promise.all([load(), loadPlugins()]);
});
</script>

<style scoped>
.hub-editor {
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

.body {
  display: flex;
  gap: 16px;
  flex: 1;
  min-height: 540px;
}

.side-panel {
  width: 280px;
  flex-shrink: 0;
  display: flex;
  flex-direction: column;
  gap: 16px;
  overflow-y: auto;
}

.side-panel.right {
  width: 320px;
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
  min-height: 480px;
}

.canvas-hint {
  position: absolute;
  inset: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  pointer-events: none;
}

.card-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
}

.plugin-list {
  display: flex;
  flex-direction: column;
  gap: 6px;
  max-height: 100%;
  overflow-y: auto;
}

.plugin-item {
  padding: 8px 10px;
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 6px;
  cursor: pointer;
  transition: border-color 0.15s, background 0.15s;
}

.plugin-item:hover {
  border-color: var(--el-color-primary-light-5);
  background: var(--el-color-primary-light-9);
}

.plugin-name {
  font-size: 13px;
  font-weight: 600;
}

.side-card :deep(.el-card__header) {
  padding: 12px 16px;
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

.issues {
  display: flex;
  flex-direction: column;
}

.issue {
  display: flex;
  gap: 8px;
  padding: 8px 0;
  border-bottom: 1px solid var(--el-border-color-lighter);
}

.issue:last-child {
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

:deep(.mono-input input) {
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
}

/* 标题位上的名字输入框：占一行标题的宽度，又不至于把右侧按钮挤走 */
.name-input {
  width: 260px;
}

:deep(.name-input .el-input__inner) {
  font-size: 20px;
  font-weight: 600;
}
</style>
