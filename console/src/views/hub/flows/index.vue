<template>
  <div class="hub-flows">
    <el-card class="title-card">
      <div class="page-header">
        <div>
          <div class="header-title">
            <el-icon :size="28" style="margin-right: 12px"><Share /></el-icon>
            <span>编排流程</span>
          </div>
          <div class="header-subtitle">
            已定义的 flow 与它们的发布状态。点进去看拓扑与历史修订。
          </div>
        </div>
        <div class="header-actions">
          <el-button type="primary" @click="newFlowVisible = true">
            <el-icon><Plus /></el-icon> 新建流程
          </el-button>
          <el-button :loading="loading" @click="load">
            <el-icon><Refresh /></el-icon> 刷新
          </el-button>
        </div>
      </div>
    </el-card>

    <el-dialog v-model="newFlowVisible" title="新建流程" width="460px" @closed="resetNewFlow">
      <el-form label-width="80px">
        <el-form-item label="名称" required>
          <el-input
            v-model="newFlowName"
            placeholder="order-intake"
            class="mono-input"
            @keyup.enter="createFlow"
          />
        </el-form-item>
        <el-form-item label=" ">
          <span class="muted small">
            只允许字母、数字、下划线与中划线，且以字母或数字开头——它会进 URL 与 MCP 工具名。
          </span>
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="newFlowVisible = false">取消</el-button>
        <el-button type="primary" @click="createFlow">创建</el-button>
      </template>
    </el-dialog>

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
              <el-icon :size="28"><Share /></el-icon>
            </div>
            <div class="stat-info">
              <div class="stat-value">{{ flows.length }}</div>
              <div class="stat-label">流程</div>
            </div>
          </div>
        </el-card>
      </el-col>
      <el-col :span="8">
        <el-card shadow="hover" class="stat-card">
          <div class="stat-content">
            <div class="stat-icon" style="background: var(--el-color-success-light-9); color: var(--el-color-success)">
              <el-icon :size="28"><CircleCheck /></el-icon>
            </div>
            <div class="stat-info">
              <div class="stat-value">{{ publishedCount }}</div>
              <div class="stat-label">已发布</div>
            </div>
          </div>
        </el-card>
      </el-col>
      <el-col :span="8">
        <el-card shadow="hover" class="stat-card">
          <div class="stat-content">
            <div class="stat-icon" style="background: var(--el-color-warning-light-9); color: var(--el-color-warning)">
              <el-icon :size="28"><EditPen /></el-icon>
            </div>
            <div class="stat-info">
              <div class="stat-value">{{ draftCount }}</div>
              <div class="stat-label">有未发布草稿</div>
            </div>
          </div>
        </el-card>
      </el-col>
    </el-row>

    <el-card class="section-card">
      <template #header>
        <div class="card-header">
          <span><el-icon><Share /></el-icon> 流程清单</span>
          <el-input
            v-model="keyword"
            placeholder="按名称 / 描述过滤"
            clearable
            style="width: 260px"
          >
            <template #prefix><el-icon><Search /></el-icon></template>
          </el-input>
        </div>
      </template>

      <el-table
        v-loading="loading"
        :data="filtered"
        row-key="name"
        stripe
        max-height="calc(100vh - 260px)"
        @row-click="openFlow"
      >
        <el-table-column prop="name" label="名称" min-width="180">
          <template #default="{ row }">
            <span class="mono strong">{{ row.name }}</span>
          </template>
        </el-table-column>
        <el-table-column prop="description" label="描述" min-width="260" show-overflow-tooltip>
          <template #default="{ row }">
            <span v-if="row.description">{{ row.description }}</span>
            <span v-else class="muted">—</span>
          </template>
        </el-table-column>
        <el-table-column label="当前发布" width="130" align="center">
          <template #default="{ row }">
            <el-tag v-if="row.published_revision > 0" type="success" effect="plain">
              rev {{ row.published_revision }}
            </el-tag>
            <el-tag v-else type="info" effect="plain">未发布</el-tag>
          </template>
        </el-table-column>
        <el-table-column prop="revision_count" label="修订数" width="90" align="center" />
        <el-table-column label="草稿" width="110" align="center">
          <template #default="{ row }">
            <el-tag v-if="row.has_draft" type="warning" effect="plain" size="small">
              有草稿
            </el-tag>
            <span v-else class="muted">—</span>
          </template>
        </el-table-column>
        <el-table-column label="操作" width="150" align="center">
          <template #default="{ row }">
            <el-button link type="primary" @click.stop="openFlow(row)">查看</el-button>
            <el-button link type="primary" @click.stop="editFlow(row)">编辑</el-button>
          </template>
        </el-table-column>
        <template #empty>
          <el-empty
            :description="loading ? '加载中…' : '还没有定义任何 flow'"
          />
        </template>
      </el-table>
    </el-card>
  </div>
</template>

<script setup>
import { computed, onMounted, ref } from "vue";
import { useRouter } from "vue-router";
import { ElMessage } from "element-plus";
import { listFlows, HubError } from "@/api/hub";
import { NAME_HINT, isValidFlowName } from "../shared/name";

const router = useRouter();

const loading = ref(false);
const hubOffline = ref(false);
const hubOfflineMessage = ref("");
const flows = ref([]);
const keyword = ref("");

const newFlowVisible = ref(false);
const newFlowName = ref("");

const publishedCount = computed(
  () => flows.value.filter((f) => f.published_revision > 0).length,
);
const draftCount = computed(() => flows.value.filter((f) => f.has_draft).length);

const filtered = computed(() => {
  const kw = keyword.value.trim().toLowerCase();
  if (!kw) return flows.value;
  return flows.value.filter((f) =>
    [f.name, f.description]
      .filter(Boolean)
      .some((field) => String(field).toLowerCase().includes(kw)),
  );
});

async function load() {
  loading.value = true;
  try {
    flows.value = await listFlows();
    hubOffline.value = false;
  } catch (error) {
    if (error instanceof HubError && error.isNetwork) {
      hubOffline.value = true;
      hubOfflineMessage.value = error.message;
      flows.value = [];
    } else {
      ElMessage.error(error.message || "加载流程列表失败");
    }
  } finally {
    loading.value = false;
  }
}

function openFlow(row) {
  router.push(`/hub/flows/${encodeURIComponent(row.name)}`);
}

function editFlow(row) {
  router.push(`/hub/flows/${encodeURIComponent(row.name)}/edit`);
}

function resetNewFlow() {
  newFlowName.value = "";
}

/**
 * 新建只是把名字带进编辑器——**不在这里先建一条空 flow**：
 * 建了却不编辑会留下一条没有任何节点的空编排，那对谁都没用，
 * 而「保存时才真正落库」让放弃编辑这件事不需要任何清理。
 */
function createFlow() {
  const name = newFlowName.value.trim();
  if (!name) {
    ElMessage.warning("请填流程名");
    return;
  }
  if (!isValidFlowName(name)) {
    ElMessage.error(NAME_HINT);
    return;
  }
  if (flows.value.some((f) => f.name === name)) {
    ElMessage.error(`流程 ${name} 已存在，请换个名字或直接编辑它`);
    return;
  }

  newFlowVisible.value = false;
  router.push(`/hub/flows/${encodeURIComponent(name)}/edit`);
}

onMounted(load);
</script>

<style scoped>
.hub-flows {
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

:deep(.el-table__row) {
  cursor: pointer;
}

:deep(.mono-input input) {
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
}
</style>
