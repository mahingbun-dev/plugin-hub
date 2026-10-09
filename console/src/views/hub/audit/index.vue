<template>
  <div class="hub-plugin-audit">
    <el-card class="title-card">
      <div class="page-header">
        <div>
          <div class="header-title">
            <el-icon :size="28" style="margin-right: 12px"><DocumentChecked /></el-icon>
            <span>插件审计</span>
          </div>
          <div class="header-subtitle">
            谁在什么时候调了什么插件、结果与耗时。数据随调用链保留期走（默认 7 天）。
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
          <span><el-icon><Search /></el-icon> 过滤</span>
        </div>
      </template>
      <el-form inline @submit.prevent>
        <el-form-item label="插件">
          <el-input v-model="filters.plugin" placeholder="如 sql-executor" clearable style="width: 180px" @keyup.enter="load" @clear="load" />
        </el-form-item>
        <el-form-item label="调用者">
          <el-input v-model="filters.caller" placeholder="平台用户名 / anonymous" clearable style="width: 180px" @keyup.enter="load" @clear="load" />
        </el-form-item>
        <el-form-item label="状态">
          <el-select v-model="filters.status" clearable placeholder="全部" style="width: 140px" @change="load">
            <el-option label="成功 (ok)" value="ok" />
            <el-option label="已拒绝 (rejected)" value="rejected" />
            <el-option label="故障 (error)" value="error" />
          </el-select>
        </el-form-item>
        <el-form-item>
          <el-button type="primary" :loading="loading" @click="load">
            <el-icon><Search /></el-icon> 查询
          </el-button>
        </el-form-item>
      </el-form>
    </el-card>

    <el-card class="section-card">
      <template #header>
        <div class="card-header">
          <span><el-icon><DocumentChecked /></el-icon> 调用记录</span>
          <span class="muted small">共 {{ total }} 条，每页 {{ PAGE_SIZE }} 条</span>
        </div>
      </template>

      <el-table v-loading="loading" :data="rows" row-key="trace_id" stripe max-height="calc(100vh - 260px)">
        <el-table-column label="时间" width="180">
          <template #default="{ row }">
            <span class="muted">{{ formatTime(row.started_at) }}</span>
          </template>
        </el-table-column>
        <el-table-column label="插件" min-width="140">
          <template #default="{ row }">
            <span class="mono">{{ row.plugin }}</span>
          </template>
        </el-table-column>
        <el-table-column label="工具" min-width="120">
          <template #default="{ row }">
            <span class="mono">{{ row.tool || '—' }}</span>
          </template>
        </el-table-column>
        <el-table-column label="调用者" min-width="130">
          <template #default="{ row }">
            <span class="mono">{{ row.caller }}</span>
          </template>
        </el-table-column>
        <el-table-column label="结果" width="110" align="center">
          <template #default="{ row }">
            <el-tag :type="statusTag(row.status)" effect="plain" size="small">
              {{ statusLabel(row.status) }}
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column label="耗时" width="100" align="right">
          <template #default="{ row }">{{ row.duration_ms }} ms</template>
        </el-table-column>
        <el-table-column label="详情" min-width="220">
          <template #default="{ row }">
            <span v-if="row.error" class="error-text">{{ row.error }}</span>
            <span v-else-if="issueSummary(row)" class="muted">{{ issueSummary(row) }}</span>
            <span v-else class="muted">—</span>
          </template>
        </el-table-column>
        <el-table-column label="trace" min-width="200">
          <template #default="{ row }">
            <el-link type="primary" :underline="false" @click="openTrace(row)">
              <span class="mono">{{ row.trace_id }}</span>
            </el-link>
          </template>
        </el-table-column>
      </el-table>

      <div class="pager">
        <el-pagination
          layout="prev, pager, next"
          :total="total"
          :page-size="PAGE_SIZE"
          :current-page="page + 1"
          @current-change="switchPage"
        />
      </div>
    </el-card>
  </div>
</template>

<script setup>
import { ElMessage } from "element-plus";
import { computed, onMounted, reactive, ref } from "vue";
import { useRouter } from "vue-router";
import { DocumentChecked, Refresh, Search } from "@element-plus/icons-vue";

import { HubError, listPluginAudit } from "@/api/hub";

const PAGE_SIZE = 20;
const router = useRouter();

const loading = ref(false);
const hubOffline = ref(false);
const hubOfflineMessage = ref("");
const rows = ref([]);
const total = ref(0);
const page = ref(0);
const filters = reactive({ plugin: "", caller: "", status: "" });

const effectiveFilters = computed(() => ({
  plugin: filters.plugin.trim() || undefined,
  caller: filters.caller.trim() || undefined,
  status: filters.status || undefined,
}));

async function load() {
  loading.value = true;
  try {
    // 拦截器已解包到响应体：拿到的就是 { total, rows }
    const payload = await listPluginAudit({
      ...effectiveFilters.value,
      limit: PAGE_SIZE,
      offset: page.value * PAGE_SIZE,
    });
    rows.value = payload.rows ?? [];
    total.value = payload.total ?? 0;
    hubOffline.value = false;
  } catch (error) {
    if (error instanceof HubError && error.isNetwork) {
      hubOffline.value = true;
      hubOfflineMessage.value = error.message;
      rows.value = [];
      total.value = 0;
    } else {
      ElMessage.error(error.message || "加载插件审计失败");
    }
  } finally {
    loading.value = false;
  }
}

function switchPage(next) {
  page.value = next - 1;
  load();
}

function openTrace(row) {
  router.push(`/traces/${encodeURIComponent(row.trace_id)}`);
}

function statusLabel(status) {
  return { ok: "成功", rejected: "已拒绝", error: "故障" }[status] ?? status;
}

function statusTag(status) {
  return { ok: "success", rejected: "warning", error: "danger" }[status] ?? "info";
}

/** 拒绝时把校验问题数亮出来，不用点开详情才知道为什么被拒。 */
function issueSummary(row) {
  const issues = row.attributes?.issues;
  if (!Array.isArray(issues) || issues.length === 0) return "";
  const first = issues[0]?.message ?? "";
  const more = issues.length > 1 ? ` 等 ${issues.length} 条` : "";
  return `${first}${more}`;
}

function formatTime(value) {
  if (!value) return "";
  return new Date(value).toLocaleString();
}

onMounted(load);
</script>

<style scoped>
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
  margin-top: 4px;
  font-size: 13px;
  color: var(--el-text-color-secondary);
}
.card-header {
  display: flex;
  align-items: center;
  gap: 8px;
}
.mono {
  font-family: monospace;
  font-size: 12px;
}
.muted {
  color: var(--el-text-color-secondary);
  font-size: 12px;
}
.small {
  font-size: 12px;
}
.error-text {
  color: var(--el-color-danger);
  font-size: 12px;
}
.pager {
  display: flex;
  justify-content: flex-end;
  margin-top: 12px;
}
</style>
