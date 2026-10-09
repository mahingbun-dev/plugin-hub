<template>
  <div class="hub-run-detail">
    <el-card class="title-card">
      <div class="page-header">
        <div class="header-left">
          <el-button link @click="goBack">
            <el-icon><ArrowLeft /></el-icon> 返回
          </el-button>
          <div>
            <div class="header-title">
              <span class="mono">{{ flowLabel }}</span>
              <el-tag v-if="run" :type="statusTagType(run.status)" effect="plain">
                {{ statusText(run.status) }}
              </el-tag>
            </div>
            <div class="header-subtitle mono">{{ runId }}</div>
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

    <template v-if="run">
      <!-- 失败原因单独摆出来：排障时第一眼要看的就是它 -->
      <el-card v-if="run.error" class="error-card">
        <div class="error-box">
          <el-icon :size="20"><WarningFilled /></el-icon>
          <div>
            <div class="error-title">失败原因</div>
            <div class="error-detail">{{ run.error }}</div>
          </div>
        </div>
      </el-card>

      <el-row :gutter="16" style="margin-bottom: 16px">
        <el-col :span="16">
          <el-card class="section-card">
            <template #header>
              <div class="card-header"><span><el-icon><InfoFilled /></el-icon> 执行概览</span></div>
            </template>
            <el-descriptions :column="3" border size="small">
              <el-descriptions-item label="流程">
                <span class="mono">{{ flowLabel }}</span>
              </el-descriptions-item>
              <el-descriptions-item label="修订">rev {{ run.flow_revision }}</el-descriptions-item>
              <el-descriptions-item label="耗时">
                {{ elapsedBetween(run.started_at, run.finished_at) }}
              </el-descriptions-item>
              <el-descriptions-item label="开始时间">
                {{ formatTime(run.started_at) }}
              </el-descriptions-item>
              <el-descriptions-item label="结束时间">
                {{ run.finished_at ? formatTime(run.finished_at) : "未结束" }}
              </el-descriptions-item>
              <el-descriptions-item label="调用链">
                <el-button link type="primary" class="mono" @click="openTrace">
                  {{ shortTrace(run.trace_id) }}
                </el-button>
              </el-descriptions-item>
              <el-descriptions-item label="触发来源" :span="3">
                <span v-if="run.trigger" class="mono">{{ triggerText }}</span>
                <span v-else class="muted">—</span>
              </el-descriptions-item>
              <el-descriptions-item v-if="run.input_summary" label="输入摘要" :span="3">
                <span class="mono wrap">{{ run.input_summary }}</span>
              </el-descriptions-item>
              <el-descriptions-item v-if="run.subject" label="调用主体" :span="3">
                <span class="mono wrap">{{ subjectText }}</span>
              </el-descriptions-item>
            </el-descriptions>
          </el-card>
        </el-col>
        <el-col :span="8">
          <el-card class="section-card">
            <template #header>
              <div class="card-header"><span><el-icon><DataLine /></el-icon> 节点耗时</span></div>
            </template>
            <div v-if="nodes.length" class="bar-list">
              <div v-for="node in nodes" :key="node.id" class="bar-row">
                <div class="bar-label mono">{{ node.node_id }}</div>
                <div class="bar-track">
                  <div
                    class="bar-fill"
                    :class="{ 'is-failed': node.status === 'failed', 'is-rejected': node.status === 'rejected' }"
                    :style="{ width: barWidth(node) }"
                  />
                </div>
                <div class="bar-value">{{ formatDuration(node.duration_ms) }}</div>
              </div>
            </div>
            <el-empty v-else description="没有节点记录" :image-size="60" />
          </el-card>
        </el-col>
      </el-row>

      <el-card class="section-card">
        <template #header>
          <div class="card-header">
            <span><el-icon><Grid /></el-icon> 节点明细</span>
            <span class="muted small">{{ nodes.length }} 个节点</span>
          </div>
        </template>
        <el-table :data="nodes" size="small" stripe row-key="id" max-height="calc(100vh - 320px)">
          <el-table-column prop="node_id" label="节点" width="130">
            <template #default="{ row }">
              <span class="mono strong">{{ row.node_id }}</span>
            </template>
          </el-table-column>
          <el-table-column label="插件" min-width="140">
            <template #default="{ row }">
              <span class="mono">{{ row.plugin }}</span>
              <span v-if="row.version" class="muted mono">@{{ row.version }}</span>
            </template>
          </el-table-column>
          <el-table-column label="实例" min-width="170">
            <template #default="{ row }">
              <span v-if="row.instance_id" class="mono">{{ row.instance_id }}</span>
              <span v-else class="muted">—</span>
            </template>
          </el-table-column>
          <el-table-column label="状态" width="100">
            <template #default="{ row }">
              <el-tag :type="statusTagType(row.status)" effect="plain" size="small">
                {{ statusText(row.status) }}
              </el-tag>
            </template>
          </el-table-column>
          <el-table-column prop="attempt" label="尝试" width="80" align="center">
            <template #default="{ row }">
              <!-- 第 1 次是首次，不算「重试」——文案上要分开，否则 1 看起来像是重试了一次 -->
              <span :class="{ 'retry-count': row.attempt > 1 }">
                {{ row.attempt > 1 ? `第 ${row.attempt} 次` : "首次" }}
              </span>
            </template>
          </el-table-column>
          <el-table-column label="耗时" width="100" align="right">
            <template #default="{ row }">{{ formatDuration(row.duration_ms) }}</template>
          </el-table-column>
          <el-table-column label="错误" min-width="240" show-overflow-tooltip>
            <template #default="{ row }">
              <span v-if="row.error" class="error-text">{{ row.error }}</span>
              <span v-else class="muted">—</span>
            </template>
          </el-table-column>
          <template #empty>
            <el-empty description="这次执行没有留下节点记录" />
          </template>
        </el-table>
      </el-card>
    </template>
  </div>
</template>

<script setup>
import { computed, onMounted, ref } from "vue";
import { useRoute, useRouter } from "vue-router";
import { getRun, listFlows, HubError } from "@/api/hub";
import {
  elapsedBetween,
  formatDuration,
  formatTime,
  statusTagType,
  statusText,
} from "../shared/format";

const route = useRoute();
const router = useRouter();

const runId = computed(() => String(route.params.runId || ""));

const loading = ref(false);
const hubOffline = ref(false);
const hubOfflineMessage = ref("");
const loadError = ref("");
const run = ref(null);
const nodes = ref([]);
const flowNames = ref({});

const flowLabel = computed(() => {
  if (!run.value) return "";
  const name = flowNames.value[run.value.flow_id] || `#${run.value.flow_id}`;
  return `${name} · rev ${run.value.flow_revision}`;
});

/** 节点耗时条：按最慢的那个节点归一化，一眼看出谁拖慢了整条链 */
const maxDuration = computed(() =>
  Math.max(1, ...nodes.value.map((n) => n.duration_ms || 0)),
);

function barWidth(node) {
  const ratio = ((node.duration_ms || 0) / maxDuration.value) * 100;
  // 给一个最小可见宽度：0ms 的节点如果画成 0 宽，看起来像是「没有这条记录」
  return `${Math.max(ratio, 3)}%`;
}

const triggerText = computed(() => {
  const trigger = run.value?.trigger;
  if (!trigger) return "";
  if (trigger.kind === "http" && trigger.path) return `HTTP ${trigger.path}`;
  if (trigger.kind === "cron") return `定时 ${trigger.schedule || ""}`.trim();
  if (trigger.kind === "mq") return `消息订阅 ${trigger.stream || ""}`.trim();
  return JSON.stringify(trigger);
});

const subjectText = computed(() => JSON.stringify(run.value?.subject));

function shortTrace(traceId) {
  return traceId ? `…${traceId.slice(-8)}` : "—";
}

function openTrace() {
  router.push(`/traces/${encodeURIComponent(run.value.trace_id)}`);
}

function goBack() {
  router.push("/runs");
}

async function load() {
  loading.value = true;
  loadError.value = "";
  try {
    const [detail, flows] = await Promise.all([
      getRun(runId.value),
      listFlows().catch(() => []),
    ]);
    run.value = detail;
    nodes.value = detail.nodes || [];
    flowNames.value = Object.fromEntries(flows.map((f) => [f.id, f.name]));
    hubOffline.value = false;
  } catch (error) {
    if (error instanceof HubError && error.isNetwork) {
      hubOffline.value = true;
      hubOfflineMessage.value = error.message;
    } else if (error instanceof HubError && error.isNotFound) {
      loadError.value = `执行记录 ${runId.value} 不存在`;
    } else {
      loadError.value = error.message || "加载失败";
    }
    run.value = null;
  } finally {
    loading.value = false;
  }
}

onMounted(load);
</script>

<style scoped>
.hub-run-detail {
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

.error-card {
  margin-bottom: 16px;
  border-left: 4px solid var(--el-color-danger);
}

.error-box {
  display: flex;
  gap: 12px;
  align-items: flex-start;
  color: var(--el-color-danger);
}

.error-title {
  font-weight: 600;
  margin-bottom: 4px;
}

.error-detail {
  font-size: 13px;
  line-height: 1.6;
  word-break: break-all;
  color: var(--el-text-color-primary);
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

.wrap {
  word-break: break-all;
}

.error-text {
  color: var(--el-color-danger);
}

.retry-count {
  color: var(--el-color-warning);
}

.bar-list {
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.bar-row {
  display: flex;
  align-items: center;
  gap: 8px;
}

.bar-label {
  width: 80px;
  font-size: 12px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  flex-shrink: 0;
}

.bar-track {
  flex: 1;
  height: 14px;
  background: var(--el-fill-color-light);
  border-radius: 3px;
  overflow: hidden;
}

.bar-fill {
  height: 100%;
  background: var(--el-color-primary);
  border-radius: 3px;
  transition: width 0.3s;
}

.bar-fill.is-failed {
  background: var(--el-color-danger);
}

.bar-fill.is-rejected {
  background: var(--el-color-warning);
}

.bar-value {
  width: 64px;
  font-size: 12px;
  text-align: right;
  color: var(--el-text-color-secondary);
  flex-shrink: 0;
}
</style>
