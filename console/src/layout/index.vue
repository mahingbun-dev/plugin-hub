<template>
  <el-container class="console-layout">
    <el-aside width="228px" class="console-aside">
      <div class="console-brand">
        <span class="brand-mark">
          <svg viewBox="0 0 16 16" width="15" height="15" fill="none">
            <circle cx="8" cy="3.2" r="1.9" fill="#fff" />
            <circle cx="3.2" cy="11.6" r="1.9" fill="#fff" />
            <circle cx="12.8" cy="11.6" r="1.9" fill="#fff" />
            <path d="M8 3.2 3.2 11.6M8 3.2l4.8 8.4M3.2 11.6h9.6" stroke="#fff" stroke-width="1.3" />
          </svg>
        </span>
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
      <div class="console-foot">hub 链接万物</div>
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
  display: flex;
  flex-direction: column;
  overflow: hidden;
  border-right: 1px solid var(--border-subtle);
  background: rgba(255, 255, 255, 0.015);
}
.console-brand {
  height: 56px;
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 0 18px;
  border-bottom: 1px solid var(--border-subtle);
  flex-shrink: 0;
}
.brand-mark {
  width: 28px;
  height: 28px;
  border-radius: 8px;
  display: flex;
  align-items: center;
  justify-content: center;
  background: var(--brand-gradient);
  box-shadow: 0 2px 10px -2px rgba(77, 107, 254, 0.55);
  flex-shrink: 0;
}
.brand-name {
  font-size: 15px;
  font-weight: 800;
  letter-spacing: -0.01em;
  color: var(--text-primary);
}
.brand-sub {
  font-size: 11px;
  font-weight: 500;
  color: var(--text-muted);
  border: 1px solid var(--border-default);
  border-radius: 999px;
  padding: 1px 8px;
}
.console-menu {
  border-right: none;
  flex: 1;
  overflow-y: auto;
  padding: 8px 0;
}
.console-foot {
  padding: 14px 18px;
  font-size: 11px;
  letter-spacing: 0.22em;
  color: var(--text-muted);
  border-top: 1px solid var(--border-subtle);
  opacity: 0.75;
}
.console-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  border-bottom: 1px solid var(--border-subtle);
  background: color-mix(in srgb, var(--bg-page) 72%, transparent);
  backdrop-filter: blur(10px);
}
.header-title {
  font-size: 15px;
  font-weight: 700;
  color: var(--text-primary);
}
.header-actions {
  display: flex;
  align-items: center;
  gap: 12px;
}
.health-pill {
  display: inline-flex;
  align-items: center;
  gap: 7px;
  font-size: 12px;
  font-weight: 600;
  padding: 4px 12px;
  border-radius: 999px;
  border: 1px solid var(--border-default);
  color: var(--text-muted);
  cursor: default;
  transition: border-color 0.2s ease;
}
.health-dot {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background: var(--text-muted);
}
.health-pill.online {
  border-color: color-mix(in srgb, var(--color-success) 38%, transparent);
  color: var(--color-success);
}
.health-pill.online .health-dot {
  background: var(--color-success);
  box-shadow: 0 0 8px color-mix(in srgb, var(--color-success) 80%, transparent);
}
.health-pill.offline {
  border-color: color-mix(in srgb, var(--color-danger) 38%, transparent);
  color: var(--color-danger);
}
.health-pill.offline .health-dot {
  background: var(--color-danger);
}
.console-main {
  overflow: auto;
  background: transparent;
}
</style>
