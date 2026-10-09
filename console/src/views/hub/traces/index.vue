<template>
  <div class="hub-traces">
    <el-card class="title-card">
      <div class="page-header">
        <div>
          <div class="header-title">
            <el-icon :size="28" style="margin-right: 12px"><DataLine /></el-icon>
            <span>调用链</span>
          </div>
          <div class="header-subtitle">
            按 trace 聚合的最近调用。一次执行会产生若干个 span——入口一个、每个节点一个。
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

    <el-card class="section-card">
      <template #header>
        <div class="card-header">
          <span><el-icon><DataLine /></el-icon> 最近的调用链</span>
          <span class="muted small">只显示最近 {{ LIMIT }} 条</span>
        </div>
      </template>

      <el-table
        v-loading="loading"
        :data="traces"
        row-key="trace_id"
        stripe
        max-height="calc(100vh - 260px)"
        @row-click="openTrace"
      >
        <el-table-column label="trace" min-width="200">
          <template #default="{ row }">
            <span class="mono">{{ row.trace_id }}</span>
          </template>
        </el-table-column>
        <el-table-column prop="span_count" label="span 数" width="100" align="center" />
        <el-table-column label="异常 span" width="110" align="center">
          <template #default="{ row }">
            <el-tag v-if="row.error_count > 0" type="danger" effect="plain" size="small">
              {{ row.error_count }}
            </el-tag>
            <span v-else class="muted">0</span>
          </template>
        </el-table-column>
        <el-table-column label="耗时" width="120" align="right">
          <template #default="{ row }">
            <!-- 显示**墙钟**时长（首尾差），不是接口给的 span 耗时之和：
                 同层并发时几个 span 的时间是重叠的，加起来会大于真实耗时，
                 拿那个数字读「这次跑了多久」会读到偏大的值。 -->
            <el-tooltip
              :content="`墙钟 ${wallClock(row)}；各 span 耗时之和 ${formatDuration(row.total_duration_ms)}`"
              placement="top"
            >
              <span>{{ wallClock(row) }}</span>
            </el-tooltip>
          </template>
        </el-table-column>
        <el-table-column label="开始时间" width="180">
          <template #default="{ row }">
            <el-tooltip :content="formatTime(row.started_at)" placement="top">
              <span>{{ fromNow(row.started_at, now) }}</span>
            </el-tooltip>
          </template>
        </el-table-column>
        <el-table-column label="操作" width="100" align="center">
          <template #default="{ row }">
            <el-button link type="primary" @click.stop="openTrace(row)">查看</el-button>
          </template>
        </el-table-column>
        <template #empty>
          <el-empty
            :description="loading ? '加载中…' : '还没有调用链。触发一次 flow 就会有。'"
          />
        </template>
      </el-table>
    </el-card>
  </div>
</template>

<script setup>
import { onMounted, ref } from "vue";
import { useRouter } from "vue-router";
import { ElMessage } from "element-plus";
import { listTraces, HubError } from "@/api/hub";
import { elapsedBetween, formatDuration, formatTime, fromNow } from "../shared/format";

const LIMIT = 50;

const router = useRouter();

const loading = ref(false);
const hubOffline = ref(false);
const hubOfflineMessage = ref("");
const traces = ref([]);
const now = ref(Date.now());

function openTrace(row) {
  router.push(`/hub/traces/${encodeURIComponent(row.trace_id)}`);
}

/** 首尾差：这条链从第一个 span 开始到最后一个 span 结束，真实过了多久 */
function wallClock(row) {
  return elapsedBetween(row.started_at, row.finished_at);
}

async function load() {
  loading.value = true;
  try {
    traces.value = await listTraces({ limit: LIMIT });
    now.value = Date.now();
    hubOffline.value = false;
  } catch (error) {
    if (error instanceof HubError && error.isNetwork) {
      hubOffline.value = true;
      hubOfflineMessage.value = error.message;
      traces.value = [];
    } else {
      ElMessage.error(error.message || "加载调用链失败");
    }
  } finally {
    loading.value = false;
  }
}

onMounted(load);
</script>

<style scoped>
.hub-traces {
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

.card-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
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

:deep(.el-table__row) {
  cursor: pointer;
}
</style>
