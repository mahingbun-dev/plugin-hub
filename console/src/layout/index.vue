<template>
  <el-container class="console-layout">
    <el-aside width="220px" class="console-aside">
      <div class="console-brand">
        <span class="brand-dot"></span>
        <span class="brand-name">plugin-hub</span>
        <span class="brand-sub">控制台</span>
      </div>
      <el-menu
        :default-active="activeMenu"
        router
        class="console-menu"
        background-color="transparent"
      >
        <el-menu-item v-for="item in menu" :key="item.path" :index="item.path">
          <el-icon><component :is="item.icon" /></el-icon>
          <span>{{ item.title }}</span>
        </el-menu-item>
      </el-menu>
    </el-aside>

    <el-container>
      <el-header class="console-header" height="52px">
        <div class="header-title">{{ route.meta.title || 'plugin-hub 控制台' }}</div>
        <div class="header-actions">
          <el-tooltip :content="healthTooltip" placement="bottom">
            <span class="health-pill" :class="healthClass">
              <span class="health-dot"></span>{{ healthText }}
            </span>
          </el-tooltip>
          <el-button :icon="isDark ? Sunny : Moon" circle size="small" @click="toggleTheme" />
        </div>
      </el-header>
      <el-main class="console-main">
        <router-view />
      </el-main>
    </el-container>
  </el-container>
</template>

<script setup>
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import { useRoute } from 'vue-router'
import { Sunny, Moon } from '@element-plus/icons-vue'
import { getHealth } from '@/api/hub'
import { useTheme } from '@/composables/useTheme'

// 与 router 里 hub 子路由的可见项一一对应；detail/editor 为隐藏路由不进菜单
const menu = [
  { path: '/plugins', title: '插件目录', icon: 'Grid' },
  { path: '/onboarding', title: '插件开发接入', icon: 'Download' },
  { path: '/instances', title: '实例健康', icon: 'Connection' },
  { path: '/flows', title: '编排流程', icon: 'Share' },
  { path: '/runs', title: '执行记录', icon: 'Tickets' },
  { path: '/traces', title: '调用链', icon: 'DataLine' },
  { path: '/audit', title: '插件审计', icon: 'DocumentChecked' },
  { path: '/dead-letters', title: '死信', icon: 'WarningFilled' },
  { path: '/triggers', title: '触发器', icon: 'Timer' },
  { path: '/governance', title: '治理', icon: 'Odometer' },
  { path: '/upgrades', title: '版本升级', icon: 'Upload' },
]

const route = useRoute()
const { isDark, toggleTheme } = useTheme()

const activeMenu = computed(() => {
  // 详情/编辑页保持在所属列表项上（如 /flows/:name/edit 高亮「编排流程」）
  const top = route.path.split('/')[1]
  return '/' + top
})

// ── 中台健康指示 ──
// 控制台所有数据都来自中台 HTTP 面（vite 代理 /hub-api → 127.0.0.1:8092）。
// 顶栏常驻一个探测点，一眼区分「中台没起」与「页面本身的问题」。
const health = ref('unknown') // online | offline | unknown
let timer = null

async function probe() {
  try {
    await getHealth()
    health.value = 'online'
  } catch {
    health.value = 'offline'
  }
}

const healthText = computed(() =>
  health.value === 'online' ? '中台在线' : health.value === 'offline' ? '中台离线' : '探测中…',
)
const healthClass = computed(() => health.value)
const healthTooltip = computed(() =>
  health.value === 'online'
    ? 'HTTP 面可达（GET /health）'
    : '连不上中台。本地开发请先启动后端（仓库 README「快速开始」），确认 8092 端口在监听。',
)

onMounted(() => {
  probe()
  timer = setInterval(probe, 15000)
})
onBeforeUnmount(() => clearInterval(timer))
</script>

<style scoped>
.console-layout {
  height: 100%;
}
.console-aside {
  border-right: 1px solid var(--el-border-color-light);
  display: flex;
  flex-direction: column;
  overflow: hidden;
}
.console-brand {
  height: 52px;
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 0 16px;
  border-bottom: 1px solid var(--el-border-color-light);
  font-weight: 700;
  flex-shrink: 0;
}
.brand-dot {
  width: 10px;
  height: 10px;
  border-radius: 50%;
  background: var(--el-color-primary);
}
.brand-sub {
  color: var(--el-text-color-secondary);
  font-weight: 400;
  font-size: 12px;
}
.console-menu {
  border-right: none;
  flex: 1;
  overflow-y: auto;
}
.console-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  border-bottom: 1px solid var(--el-border-color-light);
}
.header-title {
  font-size: 15px;
  font-weight: 600;
}
.header-actions {
  display: flex;
  align-items: center;
  gap: 12px;
}
.health-pill {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: 12px;
  padding: 2px 10px;
  border-radius: 10px;
  border: 1px solid var(--el-border-color);
  color: var(--el-text-color-regular);
  cursor: default;
}
.health-dot {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background: var(--el-text-color-placeholder);
}
.health-pill.online .health-dot {
  background: var(--el-color-success);
}
.health-pill.offline .health-dot {
  background: var(--el-color-danger);
}
.console-main {
  overflow: auto;
  background: var(--el-bg-color-page);
}
</style>
