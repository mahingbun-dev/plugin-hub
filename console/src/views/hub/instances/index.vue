<template>
  <div class="hub-instances">
    <el-card class="title-card">
      <div class="page-header">
        <div>
          <div class="header-title">
            <el-icon :size="28" style="margin-right: 12px"><Connection /></el-icon>
            <span>实例健康</span>
          </div>
          <div class="header-subtitle">
            已注册的插件实例与它们的心跳。掉线的实例会被中台巡检摘除，因此这里看不到的即已摘除。
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

    <el-row :gutter="16" style="margin-bottom: 16px">
      <el-col :span="8">
        <el-card shadow="hover" class="stat-card">
          <div class="stat-content">
            <div class="stat-icon" style="background: var(--el-color-primary-light-9); color: var(--el-color-primary)">
              <el-icon :size="28"><Connection /></el-icon>
            </div>
            <div class="stat-info">
              <div class="stat-value">{{ instances.length }}</div>
              <div class="stat-label">已注册实例</div>
            </div>
          </div>
        </el-card>
      </el-col>
      <el-col :span="8">
        <el-card shadow="hover" class="stat-card">
          <div class="stat-content">
            <div class="stat-icon" style="background: var(--el-color-success-light-9); color: var(--el-color-success)">
              <el-icon :size="28"><Grid /></el-icon>
            </div>
            <div class="stat-info">
              <div class="stat-value">{{ pluginCount }}</div>
              <div class="stat-label">涉及插件</div>
            </div>
          </div>
        </el-card>
      </el-col>
      <el-col :span="8">
        <el-card shadow="hover" class="stat-card">
          <div class="stat-content">
            <div class="stat-icon" style="background: var(--el-color-info-light-9); color: var(--el-color-info)">
              <el-icon :size="28"><Files /></el-icon>
            </div>
            <div class="stat-info">
              <div class="stat-value">{{ versionCount }}</div>
              <div class="stat-label">涉及版本</div>
            </div>
          </div>
        </el-card>
      </el-col>
    </el-row>

    <el-card class="section-card">
      <template #header>
        <div class="card-header">
          <span><el-icon><Connection /></el-icon> 实例清单</span>
          <div class="header-actions">
            <el-select
              v-model="pluginFilter"
              placeholder="全部插件"
              clearable
              style="width: 200px"
            >
              <el-option v-for="name in pluginNames" :key="name" :label="name" :value="name" />
            </el-select>
            <span v-if="autoRefresh" class="muted refresh-hint">
              每 {{ REFRESH_MS / 1000 }} 秒自动刷新
            </span>
          </div>
        </div>
      </template>

      <el-table v-loading="loading" :data="filtered" row-key="instance_id" stripe max-height="calc(100vh - 260px)">
        <el-table-column prop="plugin_name" label="插件" min-width="150">
          <template #default="{ row }">
            <span class="mono strong">{{ row.plugin_name }}</span>
          </template>
        </el-table-column>
        <el-table-column prop="version" label="版本" width="100">
          <template #default="{ row }">
            <span class="mono">{{ row.version }}</span>
          </template>
        </el-table-column>
        <el-table-column prop="instance_id" label="实例" min-width="200">
          <template #default="{ row }">
            <span class="mono">{{ row.instance_id }}</span>
          </template>
        </el-table-column>
        <el-table-column label="自报地址" min-width="220">
          <template #default="{ row }">
            <el-tooltip
              content="插件注册时上报的可达地址。中台调用这个实例时连的就是它——同机插件与跨机插件在这一列上一眼可辨。"
              placement="top"
            >
              <span class="mono">{{ row.advertise_addr }}</span>
            </el-tooltip>
          </template>
        </el-table-column>
        <el-table-column label="来源 IP" width="140">
          <template #default="{ row }">
            <span v-if="row.source_ip" class="mono">{{ row.source_ip }}</span>
            <span v-else class="muted">—</span>
          </template>
        </el-table-column>
        <el-table-column label="最后心跳" width="200">
          <template #default="{ row }">
            <el-tooltip :content="formatTime(row.last_heartbeat_at)" placement="top">
              <span class="heartbeat">{{ fromNow(row.last_heartbeat_at, now) }}</span>
            </el-tooltip>
          </template>
        </el-table-column>
        <el-table-column label="注册时间" width="180">
          <template #default="{ row }">
            <span class="muted">{{ formatTime(row.registered_at) }}</span>
          </template>
        </el-table-column>
        <template #empty>
          <el-empty
            :description="
              loading
                ? '加载中…'
                : '当前没有实例注册。插件进程启动后会主动注册到这里。'
            "
          />
        </template>
      </el-table>
    </el-card>
  </div>
</template>

<script setup>
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { ElMessage } from "element-plus";
import { listInstances, HubError } from "@/api/hub";
import { formatTime, fromNow } from "../shared/format";

/** 心跳是秒级的，但这是给人看的页面——10 秒够看出「它还在」而不至于一直在跳 */
const REFRESH_MS = 10000;

const loading = ref(false);
const hubOffline = ref(false);
const hubOfflineMessage = ref("");
const instances = ref([]);
const pluginFilter = ref("");
const autoRefresh = ref(true);
/** 相对时间的基准。一次刷新取一次，避免同屏各行基准不同 */
const now = ref(Date.now());

let timer = null;

const pluginNames = computed(() => [
  ...new Set(instances.value.map((i) => i.plugin_name)),
]);
const pluginCount = computed(() => pluginNames.value.length);
const versionCount = computed(
  () => new Set(instances.value.map((i) => `${i.plugin_name}@${i.version}`)).size,
);

const filtered = computed(() => {
  if (!pluginFilter.value) return instances.value;
  return instances.value.filter((i) => i.plugin_name === pluginFilter.value);
});

async function load() {
  loading.value = true;
  try {
    instances.value = await listInstances();
    now.value = Date.now();
    hubOffline.value = false;
  } catch (error) {
    if (error instanceof HubError && error.isNetwork) {
      hubOffline.value = true;
      hubOfflineMessage.value = error.message;
      instances.value = [];
    } else {
      ElMessage.error(error.message || "加载实例列表失败");
    }
  } finally {
    loading.value = false;
  }
}

function startTimer() {
  stopTimer();
  if (autoRefresh.value) {
    timer = setInterval(load, REFRESH_MS);
  }
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

// 离开页面必须停掉轮询——否则切走后还在持续打中台
onBeforeUnmount(stopTimer);
</script>

<style scoped>
.hub-instances {
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

.refresh-hint {
  font-size: 12px;
}

.heartbeat {
  color: var(--el-color-success);
}
</style>
