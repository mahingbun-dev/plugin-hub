import { createApp } from 'vue'
import ElementPlus from 'element-plus'
import 'element-plus/dist/index.css'
import 'element-plus/theme-chalk/dark/css-vars.css'
import * as ElementPlusIconsVue from '@element-plus/icons-vue'
import router from './router'
import App from './App.vue'
import './styles/index.scss'
import './styles/element-dark.scss'
import { useTheme } from './composables/useTheme'

// 在 Vue app 创建前初始化主题，防止 FOUC 闪烁
const { initTheme } = useTheme()
initTheme()

const app = createApp(App)

// 全局注册所有图标：路由 meta 与页面模板里按名字直接使用（如 <Grid />）
for (const [key, component] of Object.entries(ElementPlusIconsVue)) {
  app.component(key, component)
}

app.use(ElementPlus)
app.use(router)

app.config.errorHandler = (err) => {
  console.error('Vue Error:', err)
}

app.mount('#app')
