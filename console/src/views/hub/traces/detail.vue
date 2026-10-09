<template>
  <div class="hub-trace-detail">
    <el-card class="title-card">
      <div class="page-header">
        <div class="header-left">
          <el-button link @click="goBack">
            <el-icon><ArrowLeft /></el-icon> 返回
          </el-button>
          <div>
            <div class="header-title">
              <span>调用链</span>
              <el-tag v-if="errorSpans.length" type="danger" effect="plain">
                {{ errorSpans.length }} 个异常 span
              </el-tag>
              <el-tag v-else-if="spans.length" type="success" effect="plain">全部正常</el-tag>
            </div>
            <div class="header-subtitle mono">{{ traceId }}</div>
          </div>
        </div>
        <el-button :loading="loading" @click="load">
          <el-icon><Refresh /></el-icon> 刷新
        </el-button>
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

    <el-card v-if="spans.length" class="section-card">
      <template #header>
        <div class="card-header">
          <span><el-icon><DataLine /></el-icon> 时间轴</span>
          <span class="muted small">
            墙钟 {{ formatDuration(wallClock) }} · {{ spans.length }} 个 span
          </span>
        </div>
      </template>

      <div class="waterfall">
        <div
          v-for="span in orderedSpans"
          :key="span.id"
          class="wf-row"
          :class="{ 'is-selected': selectedSpanId === span.id }"
          @click="selectedSpanId = selectedSpanId === span.id ? '' : span.id"
        >
          <div class="wf-label" :style="{ paddingLeft: `${span.depth * 18 + 8}px` }">
            <el-icon v-if="span.status !== 'ok'" class="wf-icon" :class="span.status">
              <WarningFilled />
            </el-icon>
            <span class="wf-name">{{ span.name }}</span>
            <span v-if="span.node_id" class="wf-node mono">{{ span.node_id }}</span>
          </div>
          <div class="wf-track">
            <div
              class="wf-bar"
              :class="span.status"
              :style="{ left: `${span.left}%`, width: `${span.width}%` }"
              :title="`${formatTime(span.started_at)} · ${formatDuration(span.duration_ms)}`"
            />
          </div>
          <div class="wf-duration">{{ formatDuration(span.duration_ms) }}</div>
        </div>
      </div>

      <el-collapse-transition>
        <div v-if="selectedSpan" class="span-detail">
          <el-descriptions :column="2" border size="small">
            <el-descriptions-item label="span id">
              <span class="mono">{{ selectedSpan.span_id }}</span>
            </el-descriptions-item>
            <el-descriptions-item label="父 span">
              <span v-if="selectedSpan.parent_span_id" class="mono">
                {{ selectedSpan.parent_span_id }}
              </span>
              <span v-else class="muted">无（根 span）</span>
            </el-descriptions-item>
            <el-descriptions-item label="开始时间">
              {{ formatTime(selectedSpan.started_at) }}
            </el-descriptions-item>
            <el-descriptions-item label="耗时">
              {{ formatDuration(selectedSpan.duration_ms) }}
            </el-descriptions-item>
            <el-descriptions-item label="状态">
              <el-tag :type="statusTagType(selectedSpan.status)" effect="plain" size="small">
                {{ statusText(selectedSpan.status) }}
              </el-tag>
            </el-descriptions-item>
            <el-descriptions-item label="节点">
              <span v-if="selectedSpan.node_id" class="mono">{{ selectedSpan.node_id }}</span>
              <span v-else class="muted">—</span>
            </el-descriptions-item>
          </el-descriptions>

          <div v-if="attributeRows(selectedSpan).length" class="attr-list">
            <div v-for="attr in attributeRows(selectedSpan)" :key="attr.key" class="attr-row">
              <span class="attr-key mono">{{ attr.key }}</span>
              <span class="attr-value mono" :class="{ 'error-text': attr.isError }">
                {{ attr.value }}
              </span>
            </div>
          </div>
        </div>
      </el-collapse-transition>
    </el-card>

    <el-card v-if="runs.length" class="section-card" style="margin-top: 16px">
      <template #header>
        <div class="card-header">
          <span><el-icon><Tickets /></el-icon> 这条链上的执行</span>
        </div>
      </template>
      <el-table :data="runs" size="small" stripe row-key="run_id" max-height="calc(100vh - 320px)">
        <el-table-column label="流程" min-width="140">
          <template #default="{ row }">
            <span class="mono strong">{{ flowNameOf(row) }}</span>
            <el-tag size="small" effect="plain" style="margin-left: 6px">
              rev {{ row.flow_revision }}
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column label="状态" width="100">
          <template #default="{ row }">
            <el-tag :type="statusTagType(row.status)" effect="plain" size="small">
              {{ statusText(row.status) }}
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column label="开始时间" width="180">
          <template #default="{ row }">{{ formatTime(row.started_at) }}</template>
        </el-table-column>
        <el-table-column label="错误" min-width="240" show-overflow-tooltip>
          <template #default="{ row }">
            <span v-if="row.error" class="error-text">{{ row.error }}</span>
            <span v-else class="muted">—</span>
          </template>
        </el-table-column>
        <el-table-column label="操作" width="100" align="center">
          <template #default="{ row }">
            <el-button link type="primary" @click="openRun(row)">执行详情</el-button>
          </template>
        </el-table-column>
      </el-table>
    </el-card>
  </div>
</template>

<script setup>
import { computed, onMounted, ref } from "vue";
import { useRoute, useRouter } from "vue-router";
import { getTrace, listFlows, HubError } from "@/api/hub";
import { formatDuration, formatTime, statusTagType, statusText } from "../shared/format";

const route = useRoute();
const router = useRouter();

const traceId = computed(() => String(route.params.traceId || ""));

const loading = ref(false);
const hubOffline = ref(false);
const hubOfflineMessage = ref("");
const loadError = ref("");
const spans = ref([]);
const runs = ref([]);
const flowNames = ref({});
const selectedSpanId = ref("");

const errorSpans = computed(() => spans.value.filter((s) => s.status !== "ok"));

const selectedSpan = computed(() =>
  spans.value.find((s) => s.id === selectedSpanId.value) || null,
);

/** 时间轴的原点与总跨度。跨度取 0 时兜底为 1，避免除零。 */
const timeline = computed(() => {
  if (!spans.value.length) return { start: 0, total: 1 };
  let start = Infinity;
  let end = -Infinity;
  for (const span of spans.value) {
    const s = new Date(span.started_at).getTime();
    start = Math.min(start, s);
    end = Math.max(end, s + (span.duration_ms || 0));
  }
  return { start, total: Math.max(end - start, 1) };
});

const wallClock = computed(() => timeline.value.total);

/**
 * 排成树并算出每个 span 在时间轴上的位置。
 *
 * 按 `parent_span_id` 建父子关系而不是靠数组顺序：接口不保证 span 的返回顺序，
 * 而排错了会让缩进层级显示成另一条链路。
 * 找不到父的 span（父不在这次查询里）当根处理——总比丢掉不显示强。
 */
const orderedSpans = computed(() => {
  const byId = new Map(spans.value.map((s) => [s.span_id, s]));
  const children = new Map();
  const roots = [];

  for (const span of spans.value) {
    const parent = span.parent_span_id ? byId.get(span.parent_span_id) : null;
    if (parent) {
      if (!children.has(parent.span_id)) children.set(parent.span_id, []);
      children.get(parent.span_id).push(span);
    } else {
      roots.push(span);
    }
  }

  const ordered = [];
  const walk = (span, depth) => {
    const startedAt = new Date(span.started_at).getTime();
    const { start, total } = timeline.value;
    ordered.push({
      ...span,
      depth,
      left: ((startedAt - start) / total) * 100,
      width: Math.max(((span.duration_ms || 0) / total) * 100, 1.5),
    });
    const kids = (children.get(span.span_id) || []).sort(
      (a, b) => new Date(a.started_at) - new Date(b.started_at),
    );
    for (const kid of kids) walk(kid, depth + 1);
  };

  for (const root of roots.sort((a, b) => new Date(a.started_at) - new Date(b.started_at))) {
    walk(root, 0);
  }
  return ordered;
});

function flowNameOf(row) {
  return flowNames.value[row.flow_id] || `#${row.flow_id}`;
}

/** span 的 attributes 摊平成可读的键值行 */
function attributeRows(span) {
  const attributes = span.attributes || {};
  return Object.entries(attributes)
    .filter(([, value]) => value !== null && value !== undefined && value !== "")
    .map(([key, value]) => ({
      key,
      value: typeof value === "object" ? JSON.stringify(value) : String(value),
      isError: key === "error",
    }));
}

function openRun(row) {
  router.push(`/hub/runs/${encodeURIComponent(row.run_id)}`);
}

function goBack() {
  router.push("/hub/traces");
}

async function load() {
  loading.value = true;
  loadError.value = "";
  try {
    const [detail, flows] = await Promise.all([
      getTrace(traceId.value),
      listFlows().catch(() => []),
    ]);
    spans.value = detail.spans || [];
    runs.value = detail.runs || [];
    flowNames.value = Object.fromEntries(flows.map((f) => [f.id, f.name]));
    selectedSpanId.value = "";
    hubOffline.value = false;
  } catch (error) {
    if (error instanceof HubError && error.isNetwork) {
      hubOffline.value = true;
      hubOfflineMessage.value = error.message;
    } else if (error instanceof HubError && error.isNotFound) {
      loadError.value = `调用链 ${traceId.value} 不存在（span 默认只留 7 天）`;
    } else {
      loadError.value = error.message || "加载失败";
    }
    spans.value = [];
    runs.value = [];
  } finally {
    loading.value = false;
  }
}

onMounted(load);
</script>

<style scoped>
.hub-trace-detail {
  padding: 16px;
}

.title-card {
  margin-bottom: 16px;
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
  font-size: 12px;
  color: var(--el-text-color-secondary);
}

.card-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
}

.mono {
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
}

.strong {
  font-weight: 600;
}

.muted {
  color: var(--el-text-color-secondary);
}

.small {
  font-size: 12px;
}

.error-text {
  color: var(--el-color-danger);
}

.waterfall {
  display: flex;
  flex-direction: column;
}

.wf-row {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 4px 0;
  border-bottom: 1px solid var(--el-border-color-lighter);
}

.wf-row:last-child {
  border-bottom: none;
}

.wf-row.is-selected {
  background: var(--el-color-primary-light-9);
}

.wf-label {
  width: 260px;
  flex-shrink: 0;
  display: flex;
  align-items: center;
  gap: 6px;
  overflow: hidden;
}

.wf-icon.error,
.wf-icon.rejected {
  color: var(--el-color-danger);
}

.wf-name {
  font-size: 13px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.wf-node {
  font-size: 11px;
  color: var(--el-text-color-secondary);
  flex-shrink: 0;
}

.wf-track {
  position: relative;
  flex: 1;
  height: 16px;
  background: var(--el-fill-color-lighter);
  border-radius: 3px;
  min-width: 120px;
}

.wf-bar {
  position: absolute;
  top: 0;
  height: 100%;
  border-radius: 3px;
  background: var(--el-color-primary);
  min-width: 3px;
}

.wf-bar.error {
  background: var(--el-color-danger);
}

.wf-bar.rejected {
  background: var(--el-color-warning);
}

.wf-duration {
  width: 74px;
  flex-shrink: 0;
  text-align: right;
  font-size: 12px;
  color: var(--el-text-color-secondary);
}

.span-detail {
  margin-top: 14px;
  padding-top: 14px;
  border-top: 1px solid var(--el-border-color-lighter);
}

.attr-list {
  margin-top: 12px;
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.attr-row {
  display: flex;
  gap: 12px;
  font-size: 12px;
}

.attr-key {
  width: 140px;
  flex-shrink: 0;
  color: var(--el-text-color-secondary);
}

.attr-value {
  word-break: break-all;
}
</style>
