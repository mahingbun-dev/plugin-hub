<template>
  <div class="hub-runs">
    <el-card class="title-card">
      <div class="page-header">
        <div>
          <div class="header-title">
            <el-icon :size="28" style="margin-right: 12px"><Tickets /></el-icon>
            <span>执行记录</span>
          </div>
          <div class="header-subtitle">
            每一次 flow 执行。点进去看它卡在了哪个节点、为什么。
          </div>
        </div>
        <div class="header-actions">
          <el-select
            v-model="flowFilter"
            placeholder="全部流程"
            clearable
            style="width: 180px"
            @change="load"
          >
            <el-option v-for="flow in flows" :key="flow.name" :label="flow.name" :value="flow.name" />
          </el-select>
          <el-select v-model="statusFilter" placeholder="全部状态" clearable style="width: 140px">
            <el-option label="成功" value="succeeded" />
            <el-option label="失败" value="failed" />
            <el-option label="被拒" value="rejected" />
            <el-option label="执行中" value="running" />
          </el-select>
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

    <el-row :gutter="16" style="margin-bottom: 16px">
      <el-col v-for="card in statCards" :key="card.label" :span="6">
        <el-card shadow="hover" class="stat-card">
          <div class="stat-content">
            <div class="stat-icon" :style="{ background: card.bg, color: card.color }">
              <el-icon :size="28"><component :is="card.icon" /></el-icon>
            </div>
            <div class="stat-info">
              <div class="stat-value">{{ card.value }}</div>
              <div class="stat-label">{{ card.label }}</div>
            </div>
          </div>
        </el-card>
      </el-col>
    </el-row>

    <el-card class="section-card">
      <template #header>
        <div class="card-header">
          <span>
            <el-icon><Tickets /></el-icon> 记录
            <span v-if="runs.length >= limit" class="muted small">
              · 只显示最近 {{ limit }} 条
            </span>
          </span>
        </div>
      </template>

      <el-table
        v-loading="loading"
        :data="filtered"
        row-key="run_id"
        stripe
        max-height="calc(100vh - 260px)"
        @row-click="openRun"
      >
        <el-table-column label="流程" min-width="150">
          <template #default="{ row }">
            <span class="mono strong">{{ flowNameOf(row) }}</span>
            <el-tag size="small" effect="plain" style="margin-left: 6px">
              rev {{ row.flow_revision }}
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column label="状态" width="110">
          <template #default="{ row }">
            <el-tag :type="statusTagType(row.status)" effect="plain" size="small">
              {{ statusText(row.status) }}
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column label="耗时" width="100" align="right">
          <template #default="{ row }">{{ elapsedOf(row) }}</template>
        </el-table-column>
        <el-table-column label="开始时间" width="180">
          <template #default="{ row }">
            <el-tooltip :content="formatTime(row.started_at)" placement="top">
              <span>{{ fromNow(row.started_at, now) }}</span>
            </el-tooltip>
          </template>
        </el-table-column>
        <el-table-column label="错误" min-width="280" show-overflow-tooltip>
          <template #default="{ row }">
            <span v-if="row.error" class="error-text">{{ row.error }}</span>
            <span v-else class="muted">—</span>
          </template>
        </el-table-column>
        <el-table-column label="调用链" width="130">
          <template #default="{ row }">
            <el-button link type="primary" class="mono" @click.stop="openTrace(row)">
              {{ shortTrace(row.trace_id) }}
            </el-button>
          </template>
        </el-table-column>
        <template #empty>
          <el-empty :description="loading ? '加载中…' : '还没有执行记录'" />
        </template>
      </el-table>
    </el-card>
  </div>
</template>

<script setup>
import { computed, onMounted, ref } from "vue";
import { useRouter } from "vue-router";
import { ElMessage } from "element-plus";
import { listFlows, listRuns, HubError } from "@/api/hub";
import { formatTime, fromNow, statusTagType, statusText } from "../shared/format";

const LIMIT = 100;

const router = useRouter();

const loading = ref(false);
const hubOffline = ref(false);
const hubOfflineMessage = ref("");
const runs = ref([]);
const flows = ref([]);
const flowFilter = ref("");
const statusFilter = ref("");
const now = ref(Date.now());
const limit = ref(LIMIT);

/** `/runs` 只返回 flow_id，名字要自己映射——flow 数量少，拉一次够用 */
const flowNames = computed(() => {
  const map = {};
  for (const flow of flows.value) map[flow.id] = flow.name;
  return map;
});

const filtered = computed(() => {
  if (!statusFilter.value) return runs.value;
  return runs.value.filter((r) => r.status === statusFilter.value);
});

const statCards = computed(() => {
  const count = (status) => runs.value.filter((r) => r.status === status).length;
  return [
    {
      label: "总执行",
      value: runs.value.length,
      icon: "Tickets",
      bg: "var(--el-color-primary-light-9)",
      color: "var(--el-color-primary)",
    },
    {
      label: "成功",
      value: count("succeeded"),
      icon: "CircleCheck",
      bg: "var(--el-color-success-light-9)",
      color: "var(--el-color-success)",
    },
    {
      label: "失败",
      value: count("failed"),
      icon: "CircleClose",
      bg: "var(--el-color-danger-light-9)",
      color: "var(--el-color-danger)",
    },
    {
      label: "被拒",
      value: count("rejected"),
      icon: "WarningFilled",
      bg: "var(--el-color-warning-light-9)",
      color: "var(--el-color-warning)",
    },
  ];
});

function flowNameOf(row) {
  return flowNames.value[row.flow_id] || `#${row.flow_id}`;
}

/**
 * 耗时用起止时间算而不是取某个 duration 字段——`runs` 表里没有这一列。
 * 没跑完（finished_at 为空）时不给数：显示一个「到目前为止」的耗时
 * 会让人以为它已经结束了。
 */
function elapsedOf(row) {
  if (!row.finished_at) return "—";
  const start = new Date(row.started_at).getTime();
  const end = new Date(row.finished_at).getTime();
  if (Number.isNaN(start) || Number.isNaN(end)) return "—";
  const ms = end - start;
  return ms < 1000 ? `${ms} ms` : `${(ms / 1000).toFixed(2)} s`;
}

/** trace id 是 32 位十六进制，列里放不下——显后 8 位，完整值在 tooltip 与详情页 */
function shortTrace(traceId) {
  return traceId ? `…${traceId.slice(-8)}` : "—";
}

function openRun(row) {
  router.push(`/hub/runs/${encodeURIComponent(row.run_id)}`);
}

function openTrace(row) {
  router.push(`/hub/traces/${encodeURIComponent(row.trace_id)}`);
}

async function load() {
  loading.value = true;
  try {
    // flow 列表用来把 flow_id 翻成名字；它拉失败不该让整个页面空掉
    const [runRows, flowRows] = await Promise.all([
      listRuns({ flow: flowFilter.value || undefined, limit: LIMIT }),
      listFlows().catch(() => []),
    ]);
    runs.value = runRows;
    flows.value = flowRows;
    now.value = Date.now();
    hubOffline.value = false;
  } catch (error) {
    if (error instanceof HubError && error.isNetwork) {
      hubOffline.value = true;
      hubOfflineMessage.value = error.message;
      runs.value = [];
    } else {
      ElMessage.error(error.message || "加载执行记录失败");
    }
  } finally {
    loading.value = false;
  }
}

onMounted(load);
</script>

<style scoped>
.hub-runs {
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

.header-title {
  display: flex;
  align-items: center;
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

.stat-content {
  display: flex;
  align-items: center;
  gap: 14px;
}

.stat-icon {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 52px;
  height: 52px;
  border-radius: 10px;
  flex-shrink: 0;
}

.stat-value {
  font-size: 24px;
  font-weight: 600;
  line-height: 1.2;
}

.stat-label {
  margin-top: 2px;
  font-size: 13px;
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

:deep(.el-table__row) {
  cursor: pointer;
}
</style>
