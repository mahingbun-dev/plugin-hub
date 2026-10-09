<template>
  <div class="hub-governance">
    <el-card class="title-card">
      <div class="page-header">
        <div>
          <div class="header-title">
            <el-icon :size="28" style="margin-right: 12px"><Odometer /></el-icon>
            <span>治理</span>
          </div>
          <div class="header-subtitle">
            中台侧的实例级治理：并发闸门、熔断、背压。这些数字都在中台进程的内存里，
            反映的是「此刻」，不落库。
          </div>
        </div>
        <div class="header-actions">
          <el-switch
            v-model="autoRefresh"
            inline-prompt
            active-text="自动"
            inactive-text="手动"
            style="margin-right: 4px"
          />
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

    <!-- 中台还是旧版：这个接口对它来说不存在。要说清楚，而不是显示一张空表 -->
    <el-alert
      v-else-if="apiMissing"
      type="warning"
      :closable="false"
      show-icon
      style="margin-bottom: 16px"
      title="当前中台没有治理接口"
      description="连上的中台返回 404，说明它还是这个面之前编译的版本。重新部署中台即可。"
    />

    <template v-if="data">
      <!-- 配置。摆出来是因为「这些数字从哪来」是看这份面板的第一个问题 -->
      <el-row :gutter="16" style="margin-bottom: 16px">
        <el-col :span="6">
          <el-card shadow="hover" class="stat-card">
            <div class="stat-label">每实例并发上限</div>
            <div class="stat-value">{{ data.config.max_concurrency }}</div>
            <div class="stat-hint">同时最多几个在途调用</div>
          </el-card>
        </el-col>
        <el-col :span="6">
          <el-card shadow="hover" class="stat-card">
            <div class="stat-label">排队等待上限</div>
            <div class="stat-value">{{ data.config.queue_timeout_ms }} <span class="unit">ms</span></div>
            <div class="stat-hint">等不到名额就背压失败，不无限排队</div>
          </el-card>
        </el-col>
        <el-col :span="6">
          <el-card shadow="hover" class="stat-card">
            <div class="stat-label">熔断阈值</div>
            <div class="stat-value">{{ data.config.failure_threshold }} <span class="unit">次</span></div>
            <div class="stat-hint">连续失败达到就跳闸</div>
          </el-card>
        </el-col>
        <el-col :span="6">
          <el-card shadow="hover" class="stat-card">
            <div class="stat-label">冷却时长</div>
            <div class="stat-value">{{ data.config.open_cooldown_secs }} <span class="unit">s</span></div>
            <div class="stat-hint">之后放一个探测请求过去</div>
          </el-card>
        </el-col>
      </el-row>

      <el-card class="section-card">
        <template #header>
          <div class="card-header">
            <span><el-icon><Odometer /></el-icon> 实例状态</span>
            <span v-if="autoRefresh" class="muted small">每 {{ REFRESH_MS / 1000 }} 秒自动刷新</span>
          </div>
        </template>

        <el-alert
          type="info"
          :closable="false"
          show-icon
          style="margin-bottom: 14px"
          title="这里的实例与「实例健康」页的口径不同"
          description="治理表是按「被调用过」建项的：刚注册还没被调用过的实例不会出现在这里，而已下线但还留着失败计数的实例会（那正是要保留它的原因——否则它刚攒下的失败计数会凭空归零）。要判断谁在线，看「实例健康」页。"
        />

        <el-table :data="data.instances" row-key="instance_id" stripe max-height="calc(100vh - 260px)">
          <el-table-column label="实例" min-width="220">
            <template #default="{ row }">
              <span class="mono">{{ row.instance_id }}</span>
            </template>
          </el-table-column>
          <el-table-column label="并发占用" min-width="200">
            <template #default="{ row }">
              <div class="gauge">
                <div class="gauge-track">
                  <div
                    class="gauge-fill"
                    :class="{ saturated: row.available === 0 }"
                    :style="{ width: `${(row.in_flight / Math.max(row.max_concurrency, 1)) * 100}%` }"
                  />
                </div>
                <span class="gauge-text mono">{{ row.in_flight }} / {{ row.max_concurrency }}</span>
              </div>
            </template>
          </el-table-column>
          <el-table-column label="熔断" width="130">
            <template #default="{ row }">
              <el-tooltip :content="breakerHint(row)" placement="top">
                <el-tag :type="breakerTagType(row.breaker)" effect="plain" size="small">
                  {{ breakerText(row.breaker) }}
                </el-tag>
              </el-tooltip>
            </template>
          </el-table-column>
          <el-table-column label="连续失败" width="110" align="center">
            <template #default="{ row }">
              <span :class="{ 'error-text': row.consecutive_failures > 0 }">
                {{ row.consecutive_failures }} / {{ data.config.failure_threshold }}
              </span>
            </template>
          </el-table-column>
          <el-table-column label="累计放行" width="120" align="right">
            <template #default="{ row }">
              <span class="mono">{{ row.admitted }}</span>
            </template>
          </el-table-column>
          <template #empty>
            <el-empty description="还没有实例被调用过——治理表是按被调用建立的" />
          </template>
        </el-table>
      </el-card>
    </template>
  </div>
</template>

<script setup>
import { onBeforeUnmount, onMounted, ref, watch } from "vue";
import { ElMessage } from "element-plus";
import { getGovernance, HubError } from "@/api/hub";

const REFRESH_MS = 5000;

const loading = ref(false);
const hubOffline = ref(false);
const hubOfflineMessage = ref("");
const apiMissing = ref(false);
const data = ref(null);
const autoRefresh = ref(true);

let timer = null;

const BREAKER_TEXT = { closed: "正常", open: "跳闸中", probing: "半开探测" };

function breakerText(breaker) {
  return BREAKER_TEXT[breaker] || breaker;
}

function breakerTagType(breaker) {
  if (breaker === "closed") return "success";
  if (breaker === "open") return "danger";
  return "warning";
}

function breakerHint(row) {
  if (row.breaker === "open") {
    const seconds = row.open_for_ms != null ? (row.open_for_ms / 1000).toFixed(1) : "?";
    return `跳闸中，${seconds} 秒后放一个探测请求过去。这段时间对它的调用一律被拒（503）。`;
  }
  if (row.breaker === "probing") {
    return "半开：已经放了一个探测请求过去，等它回报。只有这个探测成功才会关掉熔断。";
  }
  return "正常放行。连续失败达到阈值会跳闸。";
}

async function load() {
  loading.value = true;
  try {
    data.value = await getGovernance();
    apiMissing.value = false;
    hubOffline.value = false;
  } catch (error) {
    if (error instanceof HubError && error.isNetwork) {
      hubOffline.value = true;
      hubOfflineMessage.value = error.message;
      data.value = null;
    } else if (error instanceof HubError && error.isNotFound) {
      apiMissing.value = true;
      data.value = null;
    } else {
      ElMessage.error(error.message || "加载治理状态失败");
    }
  } finally {
    loading.value = false;
  }
}

function startTimer() {
  stopTimer();
  if (autoRefresh.value) timer = setInterval(load, REFRESH_MS);
}

function stopTimer() {
  if (timer) {
    clearInterval(timer);
    timer = null;
  }
}

watch(autoRefresh, startTimer);

onMounted(() => {
  load();
  startTimer();
});

// 离开页面必须停掉轮询——这是 5 秒一次的高频请求
onBeforeUnmount(stopTimer);
</script>

<style scoped>
.hub-governance {
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
  max-width: 760px;
}

.header-actions {
  display: flex;
  align-items: center;
  gap: 12px;
}

.stat-card {
  min-height: 116px;
}

.stat-label {
  font-size: 13px;
  color: var(--el-text-color-secondary);
}

.stat-value {
  margin-top: 8px;
  font-size: 26px;
  font-weight: 600;
  line-height: 1.1;
}

.unit {
  font-size: 14px;
  font-weight: 400;
  color: var(--el-text-color-secondary);
}

.stat-hint {
  margin-top: 8px;
  font-size: 12px;
  color: var(--el-text-color-secondary);
  line-height: 1.4;
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

.error-text {
  color: var(--el-color-danger);
}

.gauge {
  display: flex;
  align-items: center;
  gap: 8px;
}

.gauge-track {
  flex: 1;
  height: 10px;
  background: var(--el-fill-color-light);
  border-radius: 3px;
  overflow: hidden;
  min-width: 80px;
}

.gauge-fill {
  height: 100%;
  background: var(--el-color-primary);
  border-radius: 3px;
  transition: width 0.3s;
}

/* 名额用尽 = 下一个调用会背压失败，值得一眼看出来 */
.gauge-fill.saturated {
  background: var(--el-color-warning);
}

.gauge-text {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  flex-shrink: 0;
}
</style>
