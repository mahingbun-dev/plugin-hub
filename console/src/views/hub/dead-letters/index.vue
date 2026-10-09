<template>
  <div class="hub-dead-letters">
    <el-card class="title-card">
      <div class="page-header">
        <div>
          <div class="header-title">
            <el-icon :size="28" style="margin-right: 12px"><WarningFilled /></el-icon>
            <span>死信</span>
          </div>
          <div class="header-subtitle">
            重投耗尽的异步消息。它们对应节点的执行需要人来看一眼——要么补数据重放，要么按业务放弃。
          </div>
        </div>
        <div class="header-actions">
          <el-radio-group v-model="pendingOnly" @change="load">
            <el-radio-button :value="true">待处理</el-radio-button>
            <el-radio-button :value="false">全部</el-radio-button>
          </el-radio-group>
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

    <el-card class="section-card">
      <template #header>
        <div class="card-header">
          <span>
            <el-icon><WarningFilled /></el-icon>
            {{ pendingOnly ? "待处理的死信" : "全部死信" }}
          </span>
          <span class="muted small">{{ deadLetters.length }} 条</span>
        </div>
      </template>

      <el-table v-loading="loading" :data="deadLetters" row-key="id" stripe max-height="calc(100vh - 260px)">
        <el-table-column label="流程 / 节点" min-width="200">
          <template #default="{ row }">
            <div>
              <span class="mono strong">{{ row.flow_name || "—" }}</span>
            </div>
            <div class="muted small mono">{{ row.node_id || "—" }}</div>
          </template>
        </el-table-column>
        <el-table-column label="错误" min-width="320">
          <template #default="{ row }">
            <el-tooltip :content="row.error" placement="top" :show-after="300">
              <span class="error-text clamp">{{ row.error }}</span>
            </el-tooltip>
          </template>
        </el-table-column>
        <el-table-column prop="attempts" label="投递次数" width="100" align="center" />
        <el-table-column label="首次出现" width="170">
          <template #default="{ row }">
            <el-tooltip :content="formatTime(row.first_seen_at)" placement="top">
              <span>{{ fromNow(row.first_seen_at, now) }}</span>
            </el-tooltip>
          </template>
        </el-table-column>
        <el-table-column label="重放状态" width="170">
          <template #default="{ row }">
            <template v-if="row.replayed_run_id">
              <el-button link type="success" @click="openRun(row.replayed_run_id)">
                已重放 · {{ shortId(row.replayed_run_id) }}
              </el-button>
            </template>
            <el-tag v-else type="warning" effect="plain" size="small">待处理</el-tag>
          </template>
        </el-table-column>
        <el-table-column label="操作" width="140" align="center">
          <template #default="{ row }">
            <el-button link type="primary" @click="openDetail(row)">详情</el-button>
            <el-button link type="primary" @click="openReplay(row)">重放</el-button>
          </template>
        </el-table-column>
        <template #empty>
          <el-empty
            :description="
              loading ? '加载中…' : pendingOnly ? '没有待处理的死信' : '还没有死信'
            "
          />
        </template>
      </el-table>
    </el-card>

    <!-- 详情 -->
    <el-dialog v-model="detailVisible" title="死信详情" width="680px">
      <el-descriptions v-if="current" :column="2" border size="small">
        <el-descriptions-item label="流程">{{ current.flow_name || "—" }}</el-descriptions-item>
        <el-descriptions-item label="节点">{{ current.node_id || "—" }}</el-descriptions-item>
        <el-descriptions-item label="Stream">
          <span class="mono">{{ current.stream }}</span>
        </el-descriptions-item>
        <el-descriptions-item label="消息 id">
          <span class="mono">{{ current.stream_id }}</span>
        </el-descriptions-item>
        <el-descriptions-item label="原执行">
          <el-button
            v-if="current.run_id"
            link
            type="primary"
            class="mono"
            @click="openRun(current.run_id)"
          >
            {{ current.run_id }}
          </el-button>
          <span v-else class="muted">—</span>
        </el-descriptions-item>
        <el-descriptions-item label="投递次数">{{ current.attempts }}</el-descriptions-item>
        <el-descriptions-item label="首次出现">
          {{ formatTime(current.first_seen_at) }}
        </el-descriptions-item>
        <el-descriptions-item label="最后尝试">
          {{ formatTime(current.last_attempt_at) }}
        </el-descriptions-item>
        <el-descriptions-item v-if="current.replayed_run_id" label="重放为" :span="2">
          <el-button link type="success" class="mono" @click="openRun(current.replayed_run_id)">
            {{ current.replayed_run_id }}
          </el-button>
        </el-descriptions-item>
        <el-descriptions-item label="错误" :span="2">
          <span class="error-text">{{ current.error }}</span>
        </el-descriptions-item>
        <el-descriptions-item label="载荷摘要" :span="2">
          <pre class="payload-summary">{{ JSON.stringify(current.payload_summary, null, 2) }}</pre>
        </el-descriptions-item>
      </el-descriptions>
    </el-dialog>

    <!-- 重放 -->
    <el-dialog v-model="replayVisible" title="重放死信" width="620px" @closed="resetReplay">
      <el-alert
        type="warning"
        :closable="false"
        show-icon
        style="margin-bottom: 14px"
        title="载荷要由你提供"
        description="死信里只存了载荷摘要（类型 / 字节数 / meta 键），没有全量报文——这是刻意的：把每次失败的报文都留一份，Redis 与库都扛不住。所以重放不是「一键重发」，你得把这份数据填回来。"
      />

      <el-descriptions v-if="current" :column="1" border size="small" style="margin-bottom: 14px">
        <el-descriptions-item label="载荷摘要">
          <pre class="payload-summary">{{ JSON.stringify(current.payload_summary, null, 2) }}</pre>
        </el-descriptions-item>
      </el-descriptions>

      <el-form label-width="90px">
        <el-form-item label="载荷" required>
          <el-input
            v-model="replayForm.payload"
            type="textarea"
            :rows="7"
            placeholder='必须是 JSON 对象，例如 {"orderId": "SO-001"}'
          />
        </el-form-item>
        <el-form-item label="meta">
          <el-input
            v-model="replayForm.meta"
            type="textarea"
            :rows="2"
            placeholder='可选的 JSON 对象，例如 {"source": "manual-replay"}'
          />
        </el-form-item>
        <el-form-item label="超时">
          <el-input-number v-model="replayForm.timeoutMs" :min="1" :max="300000" :step="1000" />
          <span class="muted small" style="margin-left: 8px">毫秒</span>
        </el-form-item>
      </el-form>

      <template #footer>
        <el-button @click="replayVisible = false">取消</el-button>
        <el-button type="primary" :loading="replaying" @click="doReplay">重放</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<script setup>
import { onMounted, ref } from "vue";
import { useRouter } from "vue-router";
import { ElMessage, ElMessageBox } from "element-plus";
import { listDeadLetters, replayDeadLetter, HubError } from "@/api/hub";
import { formatTime, fromNow } from "../shared/format";

const router = useRouter();

const loading = ref(false);
const hubOffline = ref(false);
const hubOfflineMessage = ref("");
const deadLetters = ref([]);
const pendingOnly = ref(true);
const now = ref(Date.now());

const detailVisible = ref(false);
const replayVisible = ref(false);
const replaying = ref(false);
const current = ref(null);
const replayForm = ref({ payload: "", meta: "", timeoutMs: 30000 });

function shortId(id) {
  return id ? `…${id.slice(-8)}` : "—";
}

function openRun(runId) {
  router.push(`/hub/runs/${encodeURIComponent(runId)}`);
}

function openDetail(row) {
  current.value = row;
  detailVisible.value = true;
}

function openReplay(row) {
  current.value = row;
  replayForm.value = { payload: "", meta: "", timeoutMs: 30000 };
  replayVisible.value = true;
}

function resetReplay() {
  replayForm.value = { payload: "", meta: "", timeoutMs: 30000 };
}

/** 解析用户填的 JSON，给出「哪一项填错了」而不是笼统的「格式错误」 */
function parseJson(text, label) {
  const trimmed = (text || "").trim();
  if (!trimmed) return null;
  try {
    return JSON.parse(trimmed);
  } catch (error) {
    throw new Error(`${label} 不是合法 JSON：${error.message}`);
  }
}

async function doReplay() {
  const payloadText = replayForm.value.payload.trim();
  if (!payloadText) {
    ElMessage.warning("请填入要重放的载荷");
    return;
  }

  let payload;
  let meta;
  try {
    payload = parseJson(payloadText, "载荷");
    meta = parseJson(replayForm.value.meta, "meta");
  } catch (error) {
    ElMessage.error(error.message);
    return;
  }

  if (payload === null || typeof payload !== "object" || Array.isArray(payload)) {
    ElMessage.error("载荷必须是 JSON 对象");
    return;
  }

  // 重放会真的产生一次新执行、真的打插件——按「有副作用的动作」对待
  try {
    await ElMessageBox.confirm(
      `确认重放这封死信？它会触发一次新的执行（流程 ${current.value?.flow_name || "—"}）。`,
      "确认重放",
      { confirmButtonText: "重放", cancelButtonText: "取消", type: "warning" },
    );
  } catch {
    return;
  }

  replaying.value = true;
  try {
    const result = await replayDeadLetter(current.value.id, {
      payload,
      meta: meta || undefined,
      timeoutMs: replayForm.value.timeoutMs,
    });
    replayVisible.value = false;
    ElMessage.success(`已重放，新的执行 ${result.run_id}`);
    await load();
    openRun(result.run_id);
  } catch (error) {
    ElMessage.error(error.message || "重放失败");
  } finally {
    replaying.value = false;
  }
}

async function load() {
  loading.value = true;
  try {
    deadLetters.value = await listDeadLetters({
      pending: pendingOnly.value,
      limit: 100,
    });
    now.value = Date.now();
    hubOffline.value = false;
  } catch (error) {
    if (error instanceof HubError && error.isNetwork) {
      hubOffline.value = true;
      hubOfflineMessage.value = error.message;
      deadLetters.value = [];
    } else {
      ElMessage.error(error.message || "加载死信失败");
    }
  } finally {
    loading.value = false;
  }
}

onMounted(load);
</script>

<style scoped>
.hub-dead-letters {
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
  max-width: 720px;
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

/* 错误可能很长，最多显示两行——完整内容在 tooltip 与详情里 */
.clamp {
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
}

.payload-summary {
  margin: 0;
  font-size: 12px;
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  white-space: pre-wrap;
  word-break: break-all;
}
</style>
