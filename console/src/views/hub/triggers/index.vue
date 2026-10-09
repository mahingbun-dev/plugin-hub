<template>
  <div class="hub-triggers">
    <el-card class="title-card">
      <div class="page-header">
        <div>
          <div class="header-title">
            <el-icon :size="28" style="margin-right: 12px"><Timer /></el-icon>
            <span>触发器</span>
          </div>
          <div class="header-subtitle">
            让一条已发布的编排多一个被触发的入口：定时（cron）或订阅外部 Redis Stream。
            登记触发器不改变编排本身。
          </div>
        </div>
        <div class="header-actions">
          <el-button type="primary" @click="openCreate">
            <el-icon><Plus /></el-icon> 新建触发器
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

    <!-- 中台还是旧版：这个接口对它来说不存在。要说清楚，
         否则空表会被读成「一条触发器都没有」 -->
    <el-alert
      v-else-if="apiMissing"
      type="warning"
      :closable="false"
      show-icon
      style="margin-bottom: 16px"
      title="当前中台没有触发器接口"
      description="连上的中台返回 404，说明它还是这个面之前编译的版本。重新部署中台即可。"
    />

    <el-card class="section-card">
      <template #header>
        <div class="card-header">
          <span><el-icon><Timer /></el-icon> 触发器清单</span>
          <span class="muted small">
            停用的排在最前——问「它为什么没跑」，第一个答案往往是「它被关了」
          </span>
        </div>
      </template>

      <el-table v-loading="loading" :data="triggers" row-key="id" stripe max-height="calc(100vh - 260px)">
        <el-table-column label="流程" min-width="140">
          <template #default="{ row }">
            <el-button link type="primary" class="mono" @click="openFlow(row)">
              {{ row.flow_name }}
            </el-button>
          </template>
        </el-table-column>
        <el-table-column label="类型" width="100">
          <template #default="{ row }">
            <el-tag :type="row.kind === 'cron' ? 'primary' : 'success'" effect="plain" size="small">
              {{ row.kind === "cron" ? "定时" : "消息订阅" }}
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column prop="name" label="名称" width="140">
          <template #default="{ row }">
            <span class="mono">{{ row.name }}</span>
          </template>
        </el-table-column>
        <el-table-column label="配置" min-width="220">
          <template #default="{ row }">
            <span class="mono">{{ configText(row) }}</span>
          </template>
        </el-table-column>
        <el-table-column label="状态" width="90">
          <template #default="{ row }">
            <el-switch
              :model-value="row.enabled"
              :loading="togglingId === row.id"
              @change="(value) => toggle(row, value)"
            />
          </template>
        </el-table-column>
        <el-table-column label="最近触发" width="170">
          <template #default="{ row }">
            <template v-if="row.last_fired_at">
              <el-tooltip :content="formatTime(row.last_fired_at)" placement="top">
                <span>{{ fromNow(row.last_fired_at, now) }}</span>
              </el-tooltip>
              <div class="muted small">共 {{ row.fired_count }} 次</div>
            </template>
            <span v-else class="muted">还没触发过</span>
          </template>
        </el-table-column>
        <el-table-column label="最近错误" min-width="240">
          <template #default="{ row }">
            <el-tooltip v-if="row.last_error" :content="row.last_error" placement="top">
              <span class="error-text clamp">{{ row.last_error }}</span>
            </el-tooltip>
            <span v-else class="muted">—</span>
          </template>
        </el-table-column>
        <el-table-column label="操作" width="130" align="center">
          <template #default="{ row }">
            <el-button link type="primary" @click="openEdit(row)">编辑</el-button>
            <el-button link type="danger" @click="remove(row)">删除</el-button>
          </template>
        </el-table-column>
        <template #empty>
          <el-empty
            :description="loading ? '加载中…' : '还没有触发器。编排只能被手动或经 agent 触发。'"
          />
        </template>
      </el-table>
    </el-card>

    <el-dialog
      v-model="editorVisible"
      :title="editing ? '编辑触发器' : '新建触发器'"
      width="600px"
      @closed="resetForm"
    >
      <el-form :model="form" label-width="90px">
        <el-form-item label="流程" required>
          <!-- 编辑时不能换流程：触发器按 (流程, 类型, 名称) 唯一，
               改流程等于在另一条编排上新建一条，那不如删了重建 -->
          <el-select v-model="form.flow" placeholder="选择流程" :disabled="!!editing" style="width: 100%">
            <el-option v-for="flow in flows" :key="flow.name" :label="flow.name" :value="flow.name" />
          </el-select>
          <div v-if="editing" class="muted small" style="margin-top: 4px">
            流程不可改：触发器按「流程 + 类型 + 名称」唯一，换流程等于新建一条。
          </div>
        </el-form-item>

        <el-form-item label="类型" required>
          <el-radio-group v-model="form.kind" :disabled="!!editing">
            <el-radio-button value="cron">定时</el-radio-button>
            <el-radio-button value="mq">消息订阅</el-radio-button>
          </el-radio-group>
        </el-form-item>

        <el-form-item label="名称">
          <el-input v-model="form.name" placeholder="default" />
          <div class="muted small" style="margin-top: 4px">
            同一个流程下同类型可以有多个触发器，用名称区分。重名即覆盖。
          </div>
        </el-form-item>

        <template v-if="form.kind === 'cron'">
          <el-form-item label="表达式" required>
            <el-input v-model="form.expr" placeholder="0 2 * * *" class="mono-input" />
          </el-form-item>
          <el-form-item label="常用">
            <div class="preset-list">
              <el-tag
                v-for="preset in CRON_PRESETS"
                :key="preset.expr"
                class="preset"
                :type="form.expr === preset.expr ? 'primary' : 'info'"
                effect="plain"
                @click="form.expr = preset.expr"
              >
                {{ preset.label }}
              </el-tag>
            </div>
          </el-form-item>
          <el-form-item label=" ">
            <span class="muted small">
              {{ CRON_FORMAT_HELP }}
            </span>
          </el-form-item>
        </template>

        <template v-else>
          <el-form-item label="Stream" required>
            <el-input v-model="form.stream" placeholder="wms:orders" class="mono-input" />
          </el-form-item>
          <el-form-item label=" ">
            <span class="muted small">
              中台会以独立的消费组订阅这条 Stream。消息体的 <code>d</code> 字段必须是 JSON，
              它会被当作这次触发的业务载荷。
            </span>
          </el-form-item>
        </template>
      </el-form>

      <template #footer>
        <el-button @click="editorVisible = false">取消</el-button>
        <el-button type="primary" :loading="saving" @click="save">保存</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<script setup>
import { onMounted, ref } from "vue";
import { useRouter } from "vue-router";
import { ElMessage, ElMessageBox } from "element-plus";
import {
  deleteTrigger,
  listFlows,
  listTriggers,
  saveTrigger,
  setTriggerEnabled,
  HubError,
} from "@/api/hub";
import { formatTime, fromNow } from "../shared/format";

/**
 * cron 表达式是 6 字段（带秒）——中台用的是 `cron` crate 的解析器，
 * 与 Linux crontab 的 5 字段不同。这里把差别摆在明处，
 * 否则「为什么 0 2 * * * 不生效」会变成一个要读源码才能回答的问题。
 */
const CRON_FORMAT_HELP =
  "6 个字段：秒 分 时 日 月 周。与 Linux crontab 的 5 字段不同——它前面多一个「秒」。";
const CRON_PRESETS = [
  { label: "每分钟", expr: "0 * * * * *" },
  { label: "每 5 分钟", expr: "0 */5 * * * *" },
  { label: "每小时", expr: "0 0 * * * *" },
  { label: "每天 02:00", expr: "0 0 2 * * *" },
  { label: "每天 08:30", expr: "0 30 8 * * *" },
  { label: "每周一 09:00", expr: "0 0 9 * * 1" },
];

const router = useRouter();

const loading = ref(false);
const hubOffline = ref(false);
const hubOfflineMessage = ref("");
const apiMissing = ref(false);
const triggers = ref([]);
const flows = ref([]);
const now = ref(Date.now());
const togglingId = ref(null);

const editorVisible = ref(false);
const saving = ref(false);
const editing = ref(null);
const form = ref({ flow: "", kind: "cron", name: "", expr: "", stream: "" });

/** 配置摘要是按类型取不同字段的——直接 JSON.stringify 出来对人不友好 */
function configText(row) {
  const config = row.config || {};
  if (row.kind === "cron") return config.expr || "(缺 expr)";
  if (row.kind === "mq") return config.stream || "(缺 stream)";
  return JSON.stringify(config);
}

function openFlow(row) {
  router.push(`/hub/flows/${encodeURIComponent(row.flow_name)}`);
}

function openCreate() {
  editing.value = null;
  form.value = { flow: flows.value[0]?.name || "", kind: "cron", name: "", expr: "", stream: "" };
  editorVisible.value = true;
}

function openEdit(row) {
  editing.value = row;
  form.value = {
    flow: row.flow_name,
    kind: row.kind,
    name: row.name,
    expr: row.config?.expr || "",
    stream: row.config?.stream || "",
  };
  editorVisible.value = true;
}

function resetForm() {
  editing.value = null;
  form.value = { flow: "", kind: "cron", name: "", expr: "", stream: "" };
}

async function save() {
  if (!form.value.flow) {
    ElMessage.warning("请选择流程");
    return;
  }

  const config =
    form.value.kind === "cron"
      ? { expr: form.value.expr.trim() }
      : { stream: form.value.stream.trim() };

  // 本地先挡一道：中台也会拒（400），但那要等一个来回才看到
  if (!config.expr && form.value.kind === "cron") {
    ElMessage.warning("请填入 cron 表达式");
    return;
  }
  if (!config.stream && form.value.kind === "mq") {
    ElMessage.warning("请填入要订阅的 Stream 名");
    return;
  }

  saving.value = true;
  try {
    await saveTrigger(form.value.flow, {
      kind: form.value.kind,
      name: form.value.name.trim() || "default",
      config,
    });
    ElMessage.success(editing.value ? "已更新" : "已登记");
    editorVisible.value = false;
    await load();
  } catch (error) {
    ElMessage.error(error.message || "保存失败");
  } finally {
    saving.value = false;
  }
}

async function toggle(row, enabled) {
  togglingId.value = row.id;
  try {
    await setTriggerEnabled(row.id, enabled);
    row.enabled = enabled;
    ElMessage.success(enabled ? "已启用" : "已停用");
  } catch (error) {
    ElMessage.error(error.message || "操作失败");
    // 失败时重新拉一次：开关已经被用户拨过去了，不回滚会显示一个不存在的状态
    await load();
  } finally {
    togglingId.value = null;
  }
}

async function remove(row) {
  try {
    await ElMessageBox.confirm(
      `删除触发器「${row.name}」（${row.flow_name}）？这条编排将不再被它触发。`,
      "确认删除",
      { confirmButtonText: "删除", cancelButtonText: "取消", type: "warning" },
    );
  } catch {
    return;
  }

  try {
    await deleteTrigger(row.id);
    ElMessage.success("已删除");
    await load();
  } catch (error) {
    ElMessage.error(error.message || "删除失败");
  }
}

async function load() {
  loading.value = true;
  try {
    const [rows, flowRows] = await Promise.all([
      listTriggers(),
      listFlows().catch(() => []),
    ]);
    triggers.value = rows;
    flows.value = flowRows;
    now.value = Date.now();
    hubOffline.value = false;
    apiMissing.value = false;
  } catch (error) {
    if (error instanceof HubError && error.isNetwork) {
      hubOffline.value = true;
      hubOfflineMessage.value = error.message;
      triggers.value = [];
    } else if (error instanceof HubError && error.isNotFound) {
      // 路由不存在同样回 404——与「这个资源不存在」区分开要靠 api.js 里的状态码兜底
      apiMissing.value = true;
      triggers.value = [];
    } else {
      ElMessage.error(error.message || "加载触发器失败");
    }
  } finally {
    loading.value = false;
  }
}

onMounted(load);
</script>

<style scoped>
.hub-triggers {
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

.clamp {
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
}

.preset-list {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}

.preset {
  cursor: pointer;
}

:deep(.mono-input input) {
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
}
</style>
