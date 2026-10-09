import { createRouter, createWebHistory } from 'vue-router'
import Layout from '@/layout/index.vue'

// 与原工程 hub 控制台同构的子路由（去掉主工程的登录守卫与角色过滤：
// 中台管理面默认无内置鉴权，见 src/api/hub.js 顶部说明；配置了
// HUB_AUTH_PLUGIN 的部署由中台按权限位拦截，控制台按 401/403 提示）。
const routes = [
  {
    path: '/',
    component: Layout,
    redirect: '/plugins',
    children: [
      {
        path: 'plugins',
        name: 'HubPluginCatalog',
        component: () => import('@/views/hub/plugins/index.vue'),
        meta: { title: '插件目录' },
      },
      {
        path: 'onboarding',
        name: 'HubPluginOnboarding',
        component: () => import('@/views/hub/onboarding/index.vue'),
        meta: { title: '插件开发接入' },
      },
      {
        path: 'instances',
        name: 'HubInstanceHealth',
        component: () => import('@/views/hub/instances/index.vue'),
        meta: { title: '实例健康' },
      },
      {
        path: 'flows',
        name: 'HubFlowList',
        component: () => import('@/views/hub/flows/index.vue'),
        meta: { title: '编排流程' },
      },
      {
        path: 'flows/:name',
        name: 'HubFlowDetail',
        component: () => import('@/views/hub/flows/detail.vue'),
        meta: { title: '流程拓扑' },
      },
      {
        // 编排编辑器。flow 不存在时同一路由就是「新建」——名字在 URL 里，
        // 保存时中台会把它建出来，不需要为「新建」单独开一条路由与一套状态
        path: 'flows/:name/edit',
        name: 'HubFlowEditor',
        component: () => import('@/views/hub/flows/editor.vue'),
        meta: { title: '编排编辑器' },
      },
      {
        path: 'runs',
        name: 'HubRunList',
        component: () => import('@/views/hub/runs/index.vue'),
        meta: { title: '执行记录' },
      },
      {
        path: 'runs/:runId',
        name: 'HubRunDetail',
        component: () => import('@/views/hub/runs/detail.vue'),
        meta: { title: '执行详情' },
      },
      {
        path: 'traces',
        name: 'HubTraceList',
        component: () => import('@/views/hub/traces/index.vue'),
        meta: { title: '调用链' },
      },
      {
        path: 'traces/:traceId',
        name: 'HubTraceDetail',
        component: () => import('@/views/hub/traces/detail.vue'),
        meta: { title: '调用链详情' },
      },
      {
        path: 'audit',
        name: 'HubPluginAudit',
        component: () => import('@/views/hub/audit/index.vue'),
        meta: { title: '插件审计' },
      },
      {
        path: 'dead-letters',
        name: 'HubDeadLetters',
        component: () => import('@/views/hub/dead-letters/index.vue'),
        meta: { title: '死信' },
      },
      {
        path: 'triggers',
        name: 'HubTriggers',
        component: () => import('@/views/hub/triggers/index.vue'),
        meta: { title: '触发器' },
      },
      {
        path: 'governance',
        name: 'HubGovernance',
        component: () => import('@/views/hub/governance/index.vue'),
        meta: { title: '治理' },
      },
      {
        path: 'upgrades',
        name: 'HubUpgrades',
        component: () => import('@/views/hub/upgrades/index.vue'),
        meta: { title: '版本升级' },
      },
    ],
  },
  { path: '/:pathMatch(.*)*', redirect: '/plugins' },
]

const router = createRouter({
  // history 模式：URL 不要带 /#/（带 # 的路径不会被识别）
  history: createWebHistory(),
  routes,
  scrollBehavior: () => ({ top: 0 }),
})

router.afterEach((to) => {
  document.title = to.meta.title ? `${to.meta.title} · plugin-hub 控制台` : 'plugin-hub 控制台'
})

export default router
