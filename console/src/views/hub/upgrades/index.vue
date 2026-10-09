<template>
  <div class="hub-upgrades">
    <el-card class="title-card">
      <div class="page-header">
        <div>
          <div class="header-title">
            <el-icon :size="28" style="margin-right: 12px"><Upload /></el-icon>
            <span>版本升级</span>
          </div>
          <div class="header-subtitle">
            找出编排里锁在旧版本上的节点。灰度升级就是「先起新版本副本、再把这里的约束抬高」——
            这一页负责把需要抬高的地方列出来。
          </div>
        </div>
        <el-button :loading="loading" @click="load">
          <el-icon><Refresh /></el-icon> 重新扫描
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

    <el-row :gutter="16" style="margin-bottom: 16px">
      <el-col :span="8">
        <el-card shadow="hover" class="stat-card">
          <div class="stat-content">
            <div class="stat-icon" style="background: var(--el-color-primary-light-9); color: var(--el-color-primary)">
              <el-icon :size="28"><Share /></el-icon>
            </div>
            <div class="stat-info">
              <div class="stat-value">{{ scannedFlows }}</div>
              <div class="stat-label">已扫描流程</div>
            </div>
          </div>
        </el-card>
      </el-col>
      <el-col :span="8">
        <el-card shadow="hover" class="stat-card">
          <div class="stat-content">
            <div class="stat-icon" style="background: var(--el-color-warning-light-9); color: var(--el-color-warning)">
              <el-icon :size="28"><Upload /></el-icon>
            </div>
            <div class="stat-info">
              <div class="stat-value">{{ upgradable.length }}</div>
              <div class="stat-label">可抬高的节点</div>
            </div>
          </div>
        </el-card>
      </el-col>
      <el-col :span="8">
        <el-card shadow="hover" class="stat-card">
          <div class="stat-content">
            <div class="stat-icon" style="background: var(--el-color-info-light-9); color: var(--el-color-info)">
              <el-icon :size="28"><Lock /></el-icon>
            </div>
            <div class="stat-info">
              <div class="stat-value">{{ floating.length }}</div>
              <div class="stat-label">跟随最新版本</div>
            </div>
          </div>
        </el-card>
      </el-col>
    </el-row>

    <el-card class="section-card">
      <template #header>
        <div class="card-header">
          <span><el-icon><Upload /></el-icon> 可抬高的版本约束</span>
          <span class="muted small">
            按「已发布版本优先」排序——那才是正在跑生产的那一份
          </span>
        </div>
      </template>

      <el-table v-loading="loading" :data="upgradable" row-key="key" stripe max-height="400">
        <el-table-column label="流程" min-width="150">
          <template #default="{ row }">
            <el-button link type="primary" class="mono" @click="openFlow(row)">
              {{ row.flow }}
            </el-button>
          </template>
        </el-table-column>
        <el-table-column label="修订" width="140">
          <template #default="{ row }">
            <el-tag :type="row.isDraft ? 'warning' : 'success'" effect="plain" size="small">
              rev {{ row.revision }}{{ row.isDraft ? " · 草稿" : " · 已发布" }}
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column label="节点" width="130">
          <template #default="{ row }">
            <span class="mono">{{ row.nodeId }}</span>
          </template>
        </el-table-column>
        <el-table-column label="插件" min-width="140">
          <template #default="{ row }">
            <span class="mono">{{ row.plugin }}</span>
          </template>
        </el-table-column>
        <el-table-column label="锁定版本" width="120">
          <template #default="{ row }">
            <span class="mono">{{ row.version }}</span>
          </template>
        </el-table-column>
        <el-table-column label="最新版本" width="160">
          <template #default="{ row }">
            <span class="mono strong">{{ row.latest }}</span>
            <el-tag v-if="row.behind > 1" type="warning" effect="plain" size="small" style="margin-left: 6px">
              落后 {{ row.behind }} 个版本
            </el-tag>
          </template>
        </el-table-column>
        <template #empty>
          <el-empty
            :description="
              loading
                ? '扫描中…'
                : '没有锁在旧版本上的节点——要么都已抬到最新，要么编排里根本没锁版本'
            "
          />
        </template>
      </el-table>

      <el-alert
        type="info"
        :closable="false"
        show-icon
        style="margin-top: 14px"
        title="抬高版本约束要做什么"
        description="把节点的 version 改成最新版本，保存为草稿，再发布。发布前中台会重新校验契约——新版本如果改了消息定义，这里会被拦下，那时候该做的是先让下游插件跟上。"
      />
    </el-card>

    <el-card class="section-card" style="margin-top: 16px">
      <template #header>
        <div class="card-header">
          <span><el-icon><Lock /></el-icon> 跟随最新版本的节点</span>
          <span class="muted small">
            它们不写 version，永远用插件的最新版本——升级时不必动它们，代价是缺一个稳定的回退点
          </span>
        </div>
      </template>
      <el-table :data="floating" size="small" stripe max-height="400">
        <el-table-column label="流程" min-width="150">
          <template #default="{ row }">
            <span class="mono">{{ row.flow }}</span>
          </template>
        </el-table-column>
        <el-table-column label="修订" width="140">
          <template #default="{ row }">
            <el-tag type="info" effect="plain" size="small">rev {{ row.revision }}</el-tag>
          </template>
        </el-table-column>
        <el-table-column label="节点" width="140">
          <template #default="{ row }">
            <span class="mono">{{ row.nodeId }}</span>
          </template>
        </el-table-column>
        <el-table-column label="插件" min-width="160">
          <template #default="{ row }">
            <span class="mono">{{ row.plugin }}</span>
          </template>
        </el-table-column>
        <template #empty>
          <el-empty description="没有跟随最新版本的节点" :image-size="60" />
        </template>
      </el-table>
    </el-card>
  </div>
</template>

<script setup>
import { computed, onMounted, ref } from "vue";
import { useRouter } from "vue-router";
import { ElMessage } from "element-plus";
import { getFlow, getPlugin, listFlows, listPlugins, HubError } from "@/api/hub";

const router = useRouter();

const loading = ref(false);
const hubOffline = ref(false);
const hubOfflineMessage = ref("");
const upgradable = ref([]);
const floating = ref([]);
const scannedFlows = ref(0);

/**
 * 比较两个语义化版本，返回 a 相对 b 的先后。
 *
 * 只按数字段比，不处理 `-rc.1` 这类预发布后缀：中台的版本约束本身就是字符串匹配，
 * 这里做得比它更"聪明"反而会出现「这一页说要升级、中台却认为约束没变」的分歧。
 */
function compareVersion(a, b) {
  const pa = String(a).split(".").map((n) => parseInt(n, 10) || 0);
  const pb = String(b).split(".").map((n) => parseInt(n, 10) || 0);
  const len = Math.max(pa.length, pb.length);
  for (let i = 0; i < len; i += 1) {
    const diff = (pa[i] || 0) - (pb[i] || 0);
    if (diff !== 0) return diff;
  }
  return 0;
}

function openFlow(row) {
  router.push(`/flows/${encodeURIComponent(row.flow)}`);
}

/** 从一个修订的 definition 里取出所有节点的版本约束 */
function nodesOf(definition) {
  return definition?.nodes || [];
}

async function load() {
  loading.value = true;
  try {
    const [flows, plugins] = await Promise.all([listFlows(), listPlugins()]);

    // 每个插件的最新版本。列表接口不直接给，得逐个拉详情——
    // 插件数量级是几十，可以接受；真到几百个时该由中台提供一个「最新版本」字段
    const details = await Promise.all(
      plugins.map((p) => getPlugin(p.name).catch(() => null)),
    );
    const latest = new Map();
    for (const detail of details) {
      if (!detail) continue;
      const versions = (detail.versions || []).map((v) => v.version);
      if (!versions.length) continue;
      const newest = versions.reduce((acc, v) => (compareVersion(v, acc) > 0 ? v : acc), versions[0]);
      latest.set(detail.name, { newest, all: versions });
    }

    // 每条 flow 都要拉详情才有 definition——同样是一次 N+1，
    // 换来的是「把需要动的地方一次列全」
    const flowDetails = await Promise.all(
      flows.map((f) => getFlow(f.name).catch(() => null)),
    );

    const pending = [];
    const floatingNodes = [];
    let scanned = 0;

    for (const detail of flowDetails) {
      if (!detail) continue;
      scanned += 1;

      // 草稿与已发布都扫：草稿是「即将生效的那一份」，漏掉它会让升级清单看着不全
      const revisions = [
        detail.draft && { ...detail.draft, isDraft: true },
        detail.published && { ...detail.published, isDraft: false },
      ].filter(Boolean);

      for (const revision of revisions) {
        for (const node of nodesOf(revision.definition)) {
          const info = latest.get(node.plugin);
          if (!info) continue;

          if (!node.version) {
            floatingNodes.push({
              key: `${detail.name}-${revision.revision}-${node.id}`,
              flow: detail.name,
              revision: revision.revision,
              nodeId: node.id,
              plugin: node.plugin,
            });
            continue;
          }

          const behind = info.all.filter((v) => compareVersion(v, node.version) > 0).length;
          if (behind > 0) {
            pending.push({
              key: `${detail.name}-${revision.revision}-${node.id}`,
              flow: detail.name,
              revision: revision.revision,
              isDraft: revision.isDraft,
              nodeId: node.id,
              plugin: node.plugin,
              version: node.version,
              latest: info.newest,
              behind,
            });
          }
        }
      }
    }

    // 已发布的排前面：那是正在跑生产的那一份，也是最该先动的
    pending.sort((a, b) => {
      if (a.isDraft !== b.isDraft) return a.isDraft ? 1 : -1;
      return b.behind - a.behind;
    });

    upgradable.value = pending;
    floating.value = floatingNodes;
    scannedFlows.value = scanned;
    hubOffline.value = false;
  } catch (error) {
    if (error instanceof HubError && error.isNetwork) {
      hubOffline.value = true;
      hubOfflineMessage.value = error.message;
      upgradable.value = [];
      floating.value = [];
    } else {
      ElMessage.error(error.message || "扫描失败");
    }
  } finally {
    loading.value = false;
  }
}

onMounted(load);
</script>

<style scoped>
.hub-upgrades {
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
  max-width: 780px;
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
</style>
