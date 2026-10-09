import { fileURLToPath, URL } from "node:url";

import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";

// 中台 HTTP 面地址。本地默认 8092（见仓库 README「快速开始」）；
// 指向远程环境时用 VITE_HUB_TARGET=http://<host>:<port> pnpm dev。
const hubTarget = process.env.VITE_HUB_TARGET || "http://127.0.0.1:8092";

// https://vite.dev/config/
export default defineConfig({
  base: "./",
  plugins: [vue()],
  resolve: {
    alias: [{ find: "@", replacement: fileURLToPath(new URL("./src", import.meta.url)) }],
  },
  define: {
    __VUE_PROD_HYDRATION_MISMATCH_DETAILS__: "false",
  },
  server: {
    host: "127.0.0.1",
    port: 5180,
    strictPort: false,
    proxy: {
      // 控制台 API 面。前缀剥离必须与部署侧 nginx 的
      // `location /hub-api/ { proxy_pass http://127.0.0.1:8092/; }` 一致——
      // 两边行为不一致的话，本地跑通的路径到生产就 404。
      "/hub-api": {
        target: hubTarget,
        changeOrigin: true,
        secure: false,
        rewrite: (path) => path.replace(/^\/hub-api/, ""),
      },
      // MCP 面。部署侧它是独立的一段（`location = /mcp`，**不剥前缀**——
      // MCP 客户端配置里的 endpoint 写的就是 /mcp），本地代理复刻同一行为。
      // 缺了这段，「插件目录」页的 MCP 服务状态探测在 dev 下永远不可达。
      "/mcp": {
        target: hubTarget,
        changeOrigin: true,
        secure: false,
      },
    },
  },
});
