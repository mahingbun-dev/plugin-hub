<template>
  <div class="hub-plugins">
    <!-- 标题 -->
    <el-card class="title-card">
      <div class="page-header">
        <div>
          <div class="header-title">
            <el-icon :size="28" style="margin-right: 12px"><Grid /></el-icon>
            <span>插件目录</span>
          </div>
          <div class="header-subtitle">
            已注册到控制中台的插件、版本与它们声明的能力
          </div>
        </div>
        <div class="header-actions">
          <el-tag v-if="hubOffline" type="danger" effect="dark" size="large">
            中台不可达
          </el-tag>
          <!-- 要写插件的人是从这里开始的：他先来看「已有什么插件」，
               然后问「我怎么写一个」。入口放这儿比放一级菜单更贴近那一下 -->
          <el-button @click="router.push('/onboarding')">
            <el-icon><Download /></el-icon> 开发插件
          </el-button>
          <el-button :loading="loading" @click="refreshAll">
            <el-icon><Refresh /></el-icon> 刷新
          </el-button>
        </div>
      </div>
    </el-card>

    <!-- 中台不可达时给出可行动的说明，而不是一个空表格 -->
    <el-alert
      v-if="hubOffline"
      type="error"
      :closable="false"
      show-icon
      style="margin-bottom: 16px"
      title="连不上控制中台"
      :description="hubOfflineMessage"
    />

    <!-- 概览。只有三个指标：它们都能从 `/admin/plugins` 一次拿到。
         工具数与契约数**不在这里显示**——列表接口不带这两个字段，
         放在这儿就只剩两个选择：显示一个恒为 0 的假数，或者为它多发 N 次详情请求。
         前者会让人误以为插件没声明工具，后者是拿首屏延迟换一个装饰性的数字。
         它们在插件详情里按版本列出来，那里才是要回答这个问题的地方。 -->
    <el-row :gutter="16" style="margin-bottom: 16px">
      <el-col :span="8">
        <el-card shadow="hover" class="stat-card">
          <div class="stat-content">
            <div class="stat-icon" style="background: var(--el-color-primary-light-9); color: var(--el-color-primary)">
              <el-icon :size="28"><Grid /></el-icon>
            </div>
            <div class="stat-info">
              <div class="stat-value">{{ plugins.length }}</div>
              <div class="stat-label">插件</div>
            </div>
          </div>
        </el-card>
      </el-col>
      <el-col :span="8">
        <el-card shadow="hover" class="stat-card">
          <div class="stat-content">
            <div class="stat-icon" style="background: var(--el-color-success-light-9); color: var(--el-color-success)">
              <el-icon :size="28"><Files /></el-icon>
            </div>
            <div class="stat-info">
              <div class="stat-value">{{ totalVersions }}</div>
              <div class="stat-label">版本</div>
            </div>
          </div>
        </el-card>
      </el-col>
      <el-col :span="8">
        <el-card shadow="hover" class="stat-card">
          <div class="stat-content">
            <div class="stat-icon" style="background: var(--el-color-warning-light-9); color: var(--el-color-warning)">
              <el-icon :size="28"><Connection /></el-icon>
            </div>
            <div class="stat-info">
              <div class="stat-value">{{ onlineInstances }}</div>
              <div class="stat-label">在线实例</div>
            </div>
          </div>
        </el-card>
      </el-col>
    </el-row>

    <!-- 服务状态与接入。两个面都从浏览器按接入方的真实路径各探一遍：
         HTTP 走 /hub-api/health，MCP 走完整握手到 tools/list。不是问中台
         「你活着吗」——两面同进程，后端自报恒为真；而 /mcp 的 Host 白名单、
         nginx 转发段缺失这类问题只有经对外入口访问才现形，控制台与 8081
         同源，这里的探测恰好就是外部 agent 的视角。 -->
    <el-row :gutter="16" style="margin-bottom: 16px">
      <el-col :span="12">
        <el-card shadow="hover" class="service-card">
          <template #header>
            <div class="card-header">
              <span class="service-title">
                <el-icon><MagicStick /></el-icon> MCP 服务
              </span>
              <span class="service-state">
                <el-tag v-if="!mcpProbe" type="info" effect="plain">探测中…</el-tag>
                <template v-else>
                  <el-tag v-if="mcpProbe.ok" type="success" effect="plain">在线</el-tag>
                  <el-tag
                    v-else-if="mcpProbe.errorKind === 'forbidden'"
                    type="warning"
                    effect="plain"
                  >
                    服务在线 · 需权限
                  </el-tag>
                  <el-tag
                    v-else-if="mcpProbe.errorKind === 'host_rejected'"
                    type="warning"
                    effect="plain"
                  >
                    Host 校验拒绝
                  </el-tag>
                  <el-tag
                    v-else-if="mcpProbe.errorKind === 'network'"
                    type="danger"
                    effect="plain"
                  >
                    不可达
                  </el-tag>
                  <el-tag v-else type="danger" effect="plain">异常</el-tag>
                </template>
                <span v-if="mcpProbe" class="muted">{{ formatDuration(mcpProbe.latencyMs) }}</span>
              </span>
            </div>
          </template>

          <div class="endpoint-row">
            <span class="mono endpoint">{{ mcpAccessEndpoint }}</span>
            <el-button link type="primary" @click="copyText(mcpAccessEndpoint)">
              <el-icon><DocumentCopy /></el-icon> 复制
            </el-button>
          </div>
          <!-- 配置了对外端点时，探测地址（浏览器推导）退居参考——agent 真正
               要连的是上面那个，两者在「接入方解析不了域名」时不是同一个 -->
          <div v-if="mcpAccessEndpoint !== mcpEndpoint" class="service-meta">
            <span class="muted">本页探测地址：{{ mcpEndpoint }}</span>
          </div>
          <div class="service-meta">
            <template v-if="mcpProbe?.ok">
              <span class="strong">{{ mcpProbe.toolCount ?? "—" }} 个工具</span>
              <span class="muted">·</span>
              <span>Streamable HTTP</span>
              <span v-if="mcpProbe.protocolVersion" class="muted">
                · 协议 {{ mcpProbe.protocolVersion }}
              </span>
            </template>
            <span v-else-if="mcpProbe" class="probe-hint">{{ mcpHint }}</span>
          </div>

          <el-collapse class="howto-collapse">
            <el-collapse-item name="mcp">
              <template #title><span class="howto-title">接入方式</span></template>
              <pre class="code-block">{{ mcpConfigExample }}</pre>
              <div class="copy-row">
                <el-button link type="primary" @click="copyText(mcpConfigExample)">
                  <el-icon><DocumentCopy /></el-icon> 复制配置
                </el-button>
              </div>
              <ul class="howto-notes">
                <li>传输为 Streamable HTTP，客户端直连上面的端点即可，无需本地起进程</li>
                <li>
                  插件的工具以 <span class="mono">插件名__工具名</span>
                  自动聚合进工具面，注册成功后 agent 重新拉一次 tools/list 就能看见并调用
                </li>
                <li>
                  调用业务能力用 <span class="mono">invoke_plugin</span>
                  或聚合工具，载荷结构见「插件开发接入」页
                </li>
              </ul>
            </el-collapse-item>
          </el-collapse>
        </el-card>
      </el-col>

      <el-col :span="12">
        <el-card shadow="hover" class="service-card">
          <template #header>
            <div class="card-header">
              <span class="service-title">
                <el-icon><Monitor /></el-icon> HTTP 服务
              </span>
              <span class="service-state">
                <el-tag v-if="!httpProbe" type="info" effect="plain">探测中…</el-tag>
                <template v-else>
                  <el-tag v-if="httpProbe.ok" type="success" effect="plain">在线</el-tag>
                  <el-tag v-else type="danger" effect="plain">
                    {{ httpProbe.errorKind === "network" ? "不可达" : "异常" }}
                  </el-tag>
                </template>
                <span v-if="httpProbe" class="muted">
                  {{ formatDuration(httpProbe.latencyMs) }}
                </span>
              </span>
            </div>
          </template>

          <div class="endpoint-row">
            <span class="mono endpoint">{{ httpBase }}</span>
            <el-button link type="primary" @click="copyText(httpBase)">
              <el-icon><DocumentCopy /></el-icon> 复制
            </el-button>
          </div>
          <div class="service-meta">
            <template v-if="httpProbe?.ok">
              <span class="strong">v{{ httpProbe.version || "?" }}</span>
              <span class="muted">·</span>
              <span>已运行 {{ formatUptime(httpProbe.uptimeSeconds) }}</span>
            </template>
            <span v-else-if="httpProbe" class="probe-hint">{{ httpHint }}</span>
          </div>

          <el-collapse class="howto-collapse">
            <el-collapse-item name="http">
              <template #title><span class="howto-title">接入方式</span></template>
              <pre class="code-block">{{ httpIngressExample }}</pre>
              <div class="copy-row">
                <el-button link type="primary" @click="copyText(httpIngressExample)">
                  <el-icon><DocumentCopy /></el-icon> 复制命令
                </el-button>
              </div>
              <ul class="howto-notes">
                <li>
                  业务系统直接 POST 基址下的 <span class="mono">/ingress/&#123;插件名&#125;</span>，
                  载荷为 JSON 对象；超过 4MB 时响应改给 <span class="mono">payload_ref</span> 引用通道
                </li>
                <li>
                  响应码：<span class="mono">200</span> 成功 ·
                  <span class="mono">404</span> 插件未注册 ·
                  <span class="mono">422</span> 校验拒绝（带 issues） ·
                  <span class="mono">429</span> 背压/并发到顶 ·
                  <span class="mono">502</span> 插件不可达或超时
                </li>
              </ul>
            </el-collapse-item>
          </el-collapse>
        </el-card>
      </el-col>
    </el-row>

    <!-- 注册拒绝。只在中台真有留痕时出现：被拒的插件在实例表里没有行，
         列表上「在线实例 0」是唯一线索——而拒绝原因（典型如改了契约没升版本号
         的 VERSION_CONFLICT）只有中台留了痕才查得到。旧版中台没有这个接口，
         404 按没数据处理，不打扰。 -->
    <el-card v-if="rejections.length" class="section-card" style="margin-bottom: 16px">
      <template #header>
        <div class="card-header">
          <span>
            <el-icon style="color: var(--el-color-danger)"><WarningFilled /></el-icon>
            注册拒绝
            <el-tag type="danger" effect="plain" size="small" style="margin-left: 8px">
              {{ rejections.length }}
            </el-tag>
          </span>
          <span class="muted">插件在重试注册但被中台拒绝——容器活着，问题在这里</span>
        </div>
      </template>
      <el-table
        :data="rejections"
        size="small"
        stripe
        max-height="calc(100vh - 260px)"
        @row-click="(row) => openDetail({ name: row.plugin_name })"
      >
        <el-table-column prop="plugin_name" label="插件" min-width="140">
          <template #default="{ row }">
            <span class="mono strong">{{ row.plugin_name }}</span>
          </template>
        </el-table-column>
        <el-table-column prop="code_name" label="拒绝码" width="180">
          <template #default="{ row }">
            <el-tag :type="rejectionTagType(row.code_name)" effect="plain" size="small" class="mono">
              {{ row.code_name }}
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column prop="message" label="原因" min-width="260" show-overflow-tooltip />
        <el-table-column prop="detail" label="该怎么办" min-width="280" show-overflow-tooltip />
        <el-table-column prop="version" label="尝试版本" width="100" align="center">
          <template #default="{ row }">
            <span class="mono">{{ row.version || "—" }}</span>
          </template>
        </el-table-column>
        <el-table-column prop="count" label="已重试" width="90" align="center">
          <template #default="{ row }">
            <el-tag :type="row.count > 10 ? 'danger' : 'warning'" effect="plain" size="small">
              {{ row.count }} 次
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column label="最近一次" width="170">
          <template #default="{ row }">{{ formatTime(row.last_seen_at) }}</template>
        </el-table-column>
        <template #empty>
          <el-empty description="最近没有注册被拒" />
        </template>
      </el-table>
    </el-card>

    <!-- 插件列表 -->
    <el-card class="section-card">
      <template #header>
        <div class="card-header">
          <span><el-icon><Grid /></el-icon> 插件清单</span>
          <el-input
            v-model="keyword"
            placeholder="按名称 / 描述 / 负责人过滤"
            clearable
            style="width: 280px"
          >
            <template #prefix><el-icon><Search /></el-icon></template>
          </el-input>
        </div>
      </template>

      <el-table
        v-loading="loading"
        :data="filteredPlugins"
        row-key="name"
        stripe
        max-height="calc(100vh - 260px)"
        @row-click="openDetail"
      >
        <el-table-column prop="name" label="插件" min-width="180">
          <template #default="{ row }">
            <span class="mono strong">{{ row.name }}</span>
          </template>
        </el-table-column>
        <el-table-column prop="description" label="描述" min-width="240" show-overflow-tooltip>
          <template #default="{ row }">
            <span v-if="row.description">{{ row.description }}</span>
            <span v-else class="muted">—</span>
          </template>
        </el-table-column>
        <el-table-column prop="owner" label="负责人" width="140">
          <template #default="{ row }">
            <span v-if="row.owner">{{ row.owner }}</span>
            <span v-else class="muted">—</span>
          </template>
        </el-table-column>
        <el-table-column prop="version_count" label="版本" width="90" align="center" />
        <el-table-column label="在线实例" width="110" align="center">
          <template #default="{ row }">
            <el-tag v-if="row.instance_count > 0" type="success" effect="plain">
              {{ row.instance_count }}
            </el-tag>
            <el-tag v-else type="info" effect="plain">0</el-tag>
          </template>
        </el-table-column>
        <el-table-column label="操作" width="100" align="center">
          <template #default="{ row }">
            <el-button link type="primary" @click.stop="openDetail(row)">详情</el-button>
          </template>
        </el-table-column>
        <template #empty>
          <el-empty
            :description="loading ? '加载中…' : '还没有插件注册到中台'"
          />
        </template>
      </el-table>
    </el-card>

    <!-- 插件详情 -->
    <el-drawer
      v-model="detailVisible"
      :title="detail ? `插件 ${detail.name}` : '插件详情'"
      size="62%"
      destroy-on-close
    >
      <div v-loading="detailLoading" class="detail-body">
        <template v-if="detail">
          <el-descriptions :column="2" border size="small" style="margin-bottom: 16px">
            <el-descriptions-item label="描述" :span="2">
              {{ detail.description || '—' }}
            </el-descriptions-item>
            <el-descriptions-item label="负责人">{{ detail.owner || '—' }}</el-descriptions-item>
            <el-descriptions-item label="插件 ID">{{ detail.id }}</el-descriptions-item>
            <el-descriptions-item label="创建时间" :span="2">
              {{ formatTime(detail.created_at) }}
            </el-descriptions-item>
            <el-descriptions-item label="已登记版本（恢复操作）" :span="2">
              <div class="version-actions">
                <span v-for="v in detail.versions" :key="v.version" class="version-action">
                  <el-tag effect="plain" class="mono">{{ v.version }}</el-tag>
                  <el-button link type="danger" size="small" @click="removeVersion(v.version)">
                    删除
                  </el-button>
                </span>
                <span v-if="!detail.versions.length" class="muted">没有已登记的版本</span>
              </div>
            </el-descriptions-item>
          </el-descriptions>

          <el-tabs v-model="detailTab">
            <!-- 契约 -->
            <el-tab-pane :label="`契约 (${contractCount})`" name="contracts">
              <el-table :data="contractRows" size="small" stripe max-height="400">
                <el-table-column prop="fq_name" label="消息全限定名" min-width="280">
                  <template #default="{ row }">
                    <el-button link type="primary" class="mono" @click="showUsage(row.fq_name)">
                      {{ row.fq_name }}
                    </el-button>
                  </template>
                </el-table-column>
                <el-table-column prop="direction" label="方向" width="110">
                  <template #default="{ row }">
                    <el-tag :type="row.direction === 'produces' ? 'success' : 'warning'" effect="plain" size="small">
                      {{ row.direction === 'produces' ? '生产' : '消费' }}
                    </el-tag>
                  </template>
                </el-table-column>
                <el-table-column prop="version" label="版本" width="100" />
                <el-table-column label="状态" width="110" align="center">
                  <template #default="{ row }">
                    <el-tag v-if="row.serving" type="success" effect="plain" size="small">MCP 下发中</el-tag>
                    <el-tag v-else-if="row.online" type="primary" effect="plain" size="small">在线</el-tag>
                    <el-tag v-else-if="row.online === false" type="info" effect="plain" size="small">已下线</el-tag>
                    <span v-else class="muted">—</span>
                  </template>
                </el-table-column>
                <template #empty>
                  <el-empty description="该插件没有声明消息契约（只用 Struct 承载 JSON 的插件是正常的）" />
                </template>
              </el-table>
            </el-tab-pane>

            <!-- MCP 工具。已下线版本的工具不在工具面上，不计入计数，折叠收录 -->
            <el-tab-pane :label="`MCP 工具 (${toolCount})`" name="tools">
              <el-table :data="activeToolRows" size="small" stripe max-height="400">
                <el-table-column prop="name" label="工具名" min-width="160">
                  <template #default="{ row }">
                    <span class="mono">{{ row.name }}</span>
                  </template>
                </el-table-column>
                <el-table-column prop="description" label="描述" min-width="240" show-overflow-tooltip />
                <el-table-column prop="version" label="版本" width="100" />
                <el-table-column label="状态" width="110" align="center">
                  <template #default="{ row }">
                    <el-tag v-if="row.serving" type="success" effect="plain" size="small">MCP 下发中</el-tag>
                    <el-tag v-else-if="row.online" type="primary" effect="plain" size="small">在线</el-tag>
                    <span v-else class="muted">—</span>
                  </template>
                </el-table-column>
                <el-table-column label="需审批" width="90" align="center">
                  <template #default="{ row }">
                    <el-tag v-if="row.requires_approval" type="warning" effect="plain" size="small">是</el-tag>
                    <span v-else class="muted">否</span>
                  </template>
                </el-table-column>
                <template #empty>
                  <el-empty description="该插件没有声明 MCP 工具" />
                </template>
              </el-table>

              <el-collapse v-if="offlineToolRows.length" class="offline-block">
                <el-collapse-item :title="`已下线版本的工具 (${offlineToolRows.length})`" name="offline">
                  <el-table :data="offlineToolRows" size="small" stripe max-height="400">
                    <el-table-column prop="name" label="工具名" min-width="160">
                      <template #default="{ row }">
                        <span class="mono">{{ row.name }}</span>
                      </template>
                    </el-table-column>
                    <el-table-column prop="description" label="描述" min-width="240" show-overflow-tooltip />
                    <el-table-column prop="version" label="版本" width="100" />
                    <el-table-column label="需审批" width="90" align="center">
                      <template #default="{ row }">
                        <el-tag v-if="row.requires_approval" type="warning" effect="plain" size="small">是</el-tag>
                        <span v-else class="muted">否</span>
                      </template>
                    </el-table-column>
                  </el-table>
                </el-collapse-item>
              </el-collapse>
            </el-tab-pane>

            <!-- 实例 -->
            <el-tab-pane :label="`实例 (${instanceRows.length})`" name="instances">
              <el-table :data="instanceRows" size="small" stripe max-height="400">
                <el-table-column prop="instance_id" label="实例" min-width="200">
                  <template #default="{ row }">
                    <span class="mono">{{ row.instance_id }}</span>
                  </template>
                </el-table-column>
                <el-table-column prop="version" label="版本" width="100" />
                <el-table-column prop="advertise_addr" label="自报地址" min-width="200">
                  <template #default="{ row }">
                    <span class="mono">{{ row.advertise_addr }}</span>
                  </template>
                </el-table-column>
                <el-table-column prop="status" label="状态" width="100">
                  <template #default="{ row }">
                    <!-- 中台给的是 `healthy`，判据不是 `online`——
                         原来那个三元表达式让它原样透出英文枚举，还落到灰色 info 上。
                         `shared/format` 里已经有这套映射，用它。 -->
                    <el-tag :type="statusTagType(row.status)" effect="plain" size="small">
                      {{ statusText(row.status) }}
                    </el-tag>
                  </template>
                </el-table-column>
                <el-table-column label="最后心跳" width="180">
                  <template #default="{ row }">{{ formatTime(row.last_heartbeat_at) }}</template>
                </el-table-column>
                <template #empty>
                  <el-empty description="该插件当前没有在线实例" />
                </template>
              </el-table>

              <!-- 该插件自己的注册拒绝。实例空 + 这里也有记录 = 「为什么上不了线」
                   直接有答案，不用去翻插件容器的日志 -->
              <template v-if="detailRejections.length">
                <el-divider content-position="left">
                  <span style="color: var(--el-color-danger)">注册被拒记录</span>
                </el-divider>
                <el-table :data="detailRejections" size="small" stripe max-height="400">
                  <el-table-column prop="code_name" label="拒绝码" width="180">
                    <template #default="{ row }">
                      <el-tag
                        :type="rejectionTagType(row.code_name)"
                        effect="plain"
                        size="small"
                        class="mono"
                      >
                        {{ row.code_name }}
                      </el-tag>
                    </template>
                  </el-table-column>
                  <el-table-column prop="message" label="原因" min-width="220" show-overflow-tooltip />
                  <el-table-column prop="detail" label="该怎么办" min-width="260" show-overflow-tooltip />
                  <el-table-column prop="count" label="已重试" width="90" align="center">
                    <template #default="{ row }">{{ row.count }} 次</template>
                  </el-table-column>
                  <el-table-column label="最近一次" width="170">
                    <template #default="{ row }">{{ formatTime(row.last_seen_at) }}</template>
                  </el-table-column>
                </el-table>
              </template>
            </el-tab-pane>

            <!-- HTTP 调用。中台不存载荷的字段级 schema（校验器在插件自己那里），
                 所以这里给的是信封级契约：端点、信封字段、响应码——业务字段怎么传
                 要看插件自己的文档，页面不编造 -->
            <el-tab-pane label="HTTP 调用" name="http">
              <div class="invoke-row">
                <span class="invoke-method mono">POST</span>
                <span class="mono invoke-url">{{ httpInvokeEndpoint }}</span>
                <el-button link type="primary" @click="copyText(httpInvokeEndpoint)">
                  <el-icon><DocumentCopy /></el-icon> 复制端点
                </el-button>
              </div>

              <pre class="code-block">{{ httpInvokeExample }}</pre>
              <div class="copy-row">
                <el-button link type="primary" @click="copyText(httpInvokeExample)">
                  <el-icon><DocumentCopy /></el-icon> 复制命令
                </el-button>
              </div>

              <el-divider content-position="left">请求体字段</el-divider>
              <el-table :data="httpRequestFields" size="small" stripe max-height="400">
                <el-table-column prop="field" label="字段" width="130">
                  <template #default="{ row }">
                    <span class="mono">{{ row.field }}</span>
                  </template>
                </el-table-column>
                <el-table-column label="必填" width="70" align="center">
                  <template #default="{ row }">
                    <el-tag v-if="row.required" type="danger" effect="plain" size="small">必填</el-tag>
                    <span v-else class="muted">可选</span>
                  </template>
                </el-table-column>
                <el-table-column prop="note" label="说明" min-width="320" />
              </el-table>

              <el-divider content-position="left">响应码</el-divider>
              <el-table :data="httpResponseCodes" size="small" stripe max-height="400">
                <el-table-column prop="code" label="状态码" width="110">
                  <template #default="{ row }">
                    <el-tag :type="row.type" effect="plain" size="small" class="mono">
                      {{ row.code }}
                    </el-tag>
                  </template>
                </el-table-column>
                <el-table-column prop="note" label="含义与处置" min-width="320" />
              </el-table>

              <ul class="howto-notes">
                <li>
                  鉴权：需要登录 Cookie（<span class="mono">hub:invoke</span> 权限位），从控制台
                  同源发起的调用自动携带浏览器登录态；未配置鉴权插件（HUB_AUTH_PLUGIN）时匿名可用
                </li>
                <li>
                  载荷的字段结构由插件自己的校验器把关，中台不存 schema——传什么字段看插件文档
                  或问负责人（{{ detail.owner || "—" }}）
                </li>
                <li>
                  响应体含 <span class="mono">message_id</span>、<span class="mono">trace_id</span>/
                  <span class="mono">traceparent</span>、<span class="mono">instance_id</span>、
                  <span class="mono">elapsed_ms</span>；插件返回 Struct 时载荷在
                  <span class="mono">payload</span>，返回业务类型时给
                  <span class="mono">payload_type_url</span> + <span class="mono">payload_base64</span>，
                  超 4MB 时改给 <span class="mono">payload_ref</span> 引用通道地址
                </li>
              </ul>
            </el-tab-pane>
          </el-tabs>
        </template>
      </div>
    </el-drawer>

    <!-- 契约影响面 -->
    <el-dialog v-model="usageVisible" :title="`契约影响面 · ${usage?.fq_name || ''}`" width="620px">
      <div v-loading="usageLoading">
        <template v-if="usage">
          <el-divider content-position="left">生产该消息的插件</el-divider>
          <div v-if="usage.producers.length" class="ref-list">
            <el-tag v-for="p in usage.producers" :key="`p-${p.plugin}-${p.version}`" class="ref-tag">
              {{ p.plugin }} <span class="muted">@{{ p.version }}</span>
            </el-tag>
          </div>
          <el-empty v-else description="没有插件声明生产该消息" :image-size="60" />

          <el-divider content-position="left">消费该消息的插件</el-divider>
          <div v-if="usage.consumers.length" class="ref-list">
            <el-tag v-for="c in usage.consumers" :key="`c-${c.plugin}-${c.version}`" type="warning" class="ref-tag">
              {{ c.plugin }} <span class="muted">@{{ c.version }}</span>
            </el-tag>
          </div>
          <el-empty v-else description="没有插件声明消费该消息" :image-size="60" />

          <el-alert
            type="info"
            :closable="false"
            show-icon
            style="margin-top: 12px"
            title="改这个契约之前先看这里"
            description="上下游任一端的版本改动都要过字段级兼容检查：删字段、改类型、改字段编号会被拒绝注册。"
          />
        </template>
      </div>
    </el-dialog>
  </div>
</template>

<script setup>
import { computed, onMounted, ref } from "vue";
import { useRouter } from "vue-router";
import { ElMessage, ElMessageBox } from "element-plus";
import {
  deletePluginVersion,
  getMessageUsage,
  getHealth,
  getPlugin,
  getPublicEndpoints,
  listPlugins,
  listRegisterRejections,
  probeMcpService,
  HubError,
} from "@/api/hub";
import { statusTagType, statusText, formatDuration } from "../shared/format";

const loading = ref(false);
const hubOffline = ref(false);
const hubOfflineMessage = ref("");

const router = useRouter();

const plugins = ref([]);
const keyword = ref("");

/** 注册拒绝留痕。只在有数据时展示；旧版中台没有该接口（404），按空处理 */
const rejections = ref([]);

const detailVisible = ref(false);
const detailLoading = ref(false);
const detail = ref(null);
const detailTab = ref("contracts");

const usageVisible = ref(false);
const usageLoading = ref(false);
const usage = ref(null);

// ---------------------------------------------------------------- 服务状态与接入

// 端点从当前页面的源推导：生产上控制台与对外端口同源，推导出的就是接入方要用的
// 真实地址；本地 dev 由 vite 代理把 /mcp 与 /hub-api 转到中台，行为与 nginx 一致
const mcpEndpoint = `${window.location.origin}/mcp`;
const httpBase = `${window.location.origin}/hub-api`;

/** 中台下发的对外接入端点（GET /endpoints）；null/未配置时退回浏览器推导 */
const publicEndpoints = ref(null);
/** MCP 面探测结果；null = 探测中 */
const mcpProbe = ref(null);
/** HTTP 面探测结果；null = 探测中 */
const httpProbe = ref(null);

/** 接入方真正要用的 MCP 端点。agent 所在的机器不一定解析得了对外域名，
 * 所以接入地址以中台配置为准（IP 形态），浏览器推导的只做本页探测 */
const mcpAccessEndpoint = computed(() => publicEndpoints.value?.mcp || mcpEndpoint);

const mcpConfigExample = computed(() =>
  JSON.stringify(
    { mcpServers: { "plugin-hub": { url: mcpAccessEndpoint.value } } },
    null,
    2,
  ),
);

const httpIngressExample = computed(
  () =>
    `curl -X POST ${httpBase}/ingress/<插件名> \\\n` +
    `  -H "Content-Type: application/json" \\\n` +
    `  -d '{"payload": {"order_id": "SO-001"}, "timeout_ms": 30000}'`,
);

/** 详情里该插件专属的 HTTP 调用端点。与顶部 HTTP 卡同一个基址：
 * 生产同源即真实地址，本地 dev 由 vite 代理转发，行为与 nginx 一致 */
const httpInvokeEndpoint = computed(() => `${httpBase}/ingress/${detail.value?.name ?? ""}`);

const httpInvokeExample = computed(
  () =>
    `curl -X POST ${httpInvokeEndpoint.value} \\\n` +
    `  -H "Content-Type: application/json" \\\n` +
    `  -d '{"payload": {"字段": "值"}, "timeout_ms": 30000}'`,
);

/** 请求体字段说明。载荷的字段级结构中台不存——校验器在插件自己那里，页面不编造 */
const httpRequestFields = computed(() => [
  { field: "payload", required: true, note: "业务载荷，必须是 JSON 对象；校验由插件自己的校验器执行" },
  {
    field: "version",
    required: false,
    note:
      "目标插件版本，缺省路由到最新登记版本" +
      (detail.value && detail.value.versions.length
        ? `（当前已登记：${detail.value.versions.map((v) => v.version).join("、")}）`
        : ""),
  },
  { field: "message_id", required: false, note: "幂等键，重试时传同一个值由下游去重；缺省由中台生成 ULID" },
  { field: "meta", required: false, note: "附加到信封 meta 的键值对，随调用透传给插件" },
  {
    field: "timeout_ms",
    required: false,
    note: "超时预算（毫秒），默认 30000，上限 300000；更长的处理应编排成异步 flow",
  },
]);

/** 响应码与处置。error.rs 刻意区分「数据不合法」（422，调用方的问题）与
 * 「插件不可用」（502/503，下游的问题）——照实分开写，不合并成笼统的失败 */
const httpResponseCodes = [
  { code: "200", type: "success", note: "成功，返回执行结果" },
  { code: "400", type: "danger", note: "请求不合法：载荷不是 JSON 对象，或 timeout_ms 非法" },
  { code: "401 / 403", type: "danger", note: "未登录，或没有 hub:invoke 权限位" },
  { code: "404", type: "info", note: "插件未注册（或没有已登记版本）" },
  { code: "422", type: "warning", note: "校验拒绝，响应带 issues 逐条指出问题" },
  { code: "429", type: "warning", note: "背压：总线堆积到顶，稍后重试" },
  { code: "502", type: "danger", note: "插件不可达或调用超时" },
  { code: "503", type: "danger", note: "熔断：该实例暂时不可用" },
];

/** MCP 探测失败时给一句可行动的提示——每个失败态对应不同的处置 */
const mcpHint = computed(() => {
  const probe = mcpProbe.value;
  if (!probe) return "";
  switch (probe.errorKind) {
    case "host_rejected":
      return "MCP 面在线，但防 DNS rebinding 的 Host 白名单拒绝了当前域名——在中台 .env 的 HUB_MCP_ALLOWED_HOSTS 里加上它";
    case "forbidden":
      return "MCP 面在线，但当前账号没有 hub:invoke 权限位（管理面鉴权已启用）";
    case "bad_shape":
      return "端点有响应但不是 MCP 握手结果——部署上多半缺少 location = /mcp 的转发段";
    case "http_error":
      return `MCP 端点返回了非预期响应（HTTP ${probe.httpStatus}）`;
    default:
      return "连不上 MCP 端点——中台未启动，或部署上缺少 /mcp 的转发";
  }
});

const httpHint = computed(() => {
  const probe = httpProbe.value;
  if (!probe) return "";
  return probe.errorKind === "network"
    ? "连不上 HTTP 面——中台未启动，或 /hub-api 的转发未配"
    : `HTTP 面返回了非预期响应（HTTP ${probe.httpStatus}）`;
});

/** HTTP 面探测：GET /health 是探活入口，不依赖数据库，也不需要凭证 */
async function probeHttp() {
  const started = performance.now();
  try {
    const health = await getHealth();
    httpProbe.value = {
      ok: true,
      version: health?.version,
      uptimeSeconds: health?.uptime_seconds,
      latencyMs: Math.round(performance.now() - started),
    };
  } catch (error) {
    httpProbe.value = {
      ok: false,
      errorKind: error instanceof HubError && error.isNetwork ? "network" : "http_error",
      httpStatus: error instanceof HubError ? error.status : undefined,
      latencyMs: Math.round(performance.now() - started),
    };
  }
}

/** 对外接入端点是部署属性，从 /endpoints 拿。旧版中台没有这个接口（404）或
 * 请求失败都按「未配置」处理——这只是个显示增强，不该让状态卡跟着报错 */
async function loadEndpoints() {
  try {
    publicEndpoints.value = await getPublicEndpoints();
  } catch {
    publicEndpoints.value = null;
  }
}

/** 两个面各探各的（MCP 走 /mcp 完整握手，HTTP 走 /health），互不牵连 */
async function probeServices() {
  const [mcp] = await Promise.all([probeMcpService(), probeHttp(), loadEndpoints()]);
  mcpProbe.value = mcp;
}

async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    ElMessage.success("已复制");
  } catch {
    ElMessage.error("复制失败，请手动选择复制");
  }
}

/** 秒 → 「X 天 X 小时」这类人能直接读的时长 */
function formatUptime(seconds) {
  if (seconds === null || seconds === undefined) return "—";
  const days = Math.floor(seconds / 86400);
  const hours = Math.floor((seconds % 86400) / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  if (days > 0) return `${days} 天 ${hours} 小时`;
  if (hours > 0) return `${hours} 小时 ${minutes} 分`;
  if (minutes > 0) return `${minutes} 分钟`;
  return `${seconds} 秒`;
}

const filteredPlugins = computed(() => {
  const kw = keyword.value.trim().toLowerCase();
  if (!kw) return plugins.value;
  return plugins.value.filter((p) =>
    [p.name, p.description, p.owner]
      .filter(Boolean)
      .some((field) => String(field).toLowerCase().includes(kw)),
  );
});

const totalVersions = computed(() =>
  plugins.value.reduce((sum, p) => sum + (p.version_count || 0), 0),
);
const onlineInstances = computed(() =>
  plugins.value.reduce((sum, p) => sum + (p.instance_count || 0), 0),
);

const contractRows = computed(() => {
  if (!detail.value) return [];
  return detail.value.versions.flatMap((v) =>
    v.contracts.map((c) => ({ ...c, version: v.version, online: v.online, serving: v.serving })),
  );
});
const contractCount = computed(() => contractRows.value.length);

const toolRows = computed(() => {
  if (!detail.value) return [];
  return detail.value.versions.flatMap((v) =>
    v.tools.map((t) => ({ ...t, version: v.version, online: v.online, serving: v.serving })),
  );
});

/** 在工具面上的工具（未下线的版本）。已下线版本的声明不算数，不进计数 */
const activeToolRows = computed(() => toolRows.value.filter((r) => r.online !== false));
const toolCount = computed(() => activeToolRows.value.length);

/** 已下线版本的工具：保留登记信息可查，但默认折叠，不与在面工具混排 */
const offlineToolRows = computed(() => toolRows.value.filter((r) => r.online === false));

const instanceRows = computed(() => {
  if (!detail.value) return [];
  return detail.value.versions.flatMap((v) =>
    v.instances.map((i) => ({ ...i, version: v.version })),
  );
});

/** 当前详情插件的注册拒绝记录，只给实例 tab 用 */
const detailRejections = computed(() => {
  if (!detail.value) return [];
  return rejections.value.filter((r) => r.plugin_name === detail.value.name);
});

/** 拒绝码 → 标签颜色。契约类拒绝（改了东西没换版号）最常见也最难排查，
 * 给 danger；不可达更像网络问题给 warning；internal 是中台自己的事 */
function rejectionTagType(codeName) {
  if (["VERSION_CONFLICT", "BREAKING_CHANGE", "MANIFEST_INVALID", "DESCRIPTOR_INVALID"].includes(codeName))
    return "danger";
  if (["UNREACHABLE", "INSTANCE_CONFLICT", "TOOL_CONFLICT"].includes(codeName)) return "warning";
  if (codeName === "INTERNAL") return "danger";
  return "info";
}

function formatTime(value) {
  if (!value) return "—";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return String(value);
  const pad = (n) => String(n).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(
    date.getHours(),
  )}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
}

async function loadAll() {
  loading.value = true;
  hubOffline.value = false;
  try {
    plugins.value = await listPlugins();
  } catch (error) {
    // 中台不可达与「中台说没有插件」是两回事：前者要指路，后者是正常空态。
    if (error instanceof HubError && error.isNetwork) {
      hubOffline.value = true;
      hubOfflineMessage.value = error.message;
      plugins.value = [];
    } else {
      ElMessage.error(error.message || "加载插件列表失败");
    }
  } finally {
    loading.value = false;
  }
}

/** 注册拒绝留痕。旧版中台没有这个接口（404），静默当空——
 * 它是排障增强，不该让整个页面跟着旧中台报错 */
async function loadRejections() {
  try {
    rejections.value = await listRegisterRejections({ limit: 50 });
  } catch {
    rejections.value = [];
  }
}

/** 删除一个已登记版本。恢复操作：注册被 VERSION_CONFLICT / BREAKING_CHANGE
 * 拒死时，删掉旧登记让插件重新注册。级联删契约、工具与实例——**不可逆**，
 * 所以弹窗把后果说全，而不是只问一句「确定吗」。 */
async function removeVersion(version) {
  try {
    await ElMessageBox.confirm(
      `删除插件「${detail.value?.name}」的版本 ${version}？` +
        "它的契约、MCP 工具与实例登记将一并删除，工具面立即下线。" +
        "仅用于注册被 VERSION_CONFLICT / BREAKING_CHANGE 拒死后的恢复——" +
        "删除后插件需要重新注册才能上线，操作不可逆。",
      "删除已登记版本",
      { confirmButtonText: "删除", cancelButtonText: "取消", type: "warning" },
    );
  } catch {
    return;
  }
  try {
    await deletePluginVersion(detail.value?.name, version);
    ElMessage.success(`版本 ${version} 已删除，等待插件重新注册`);
    detailVisible.value = false;
    await Promise.all([loadAll(), loadRejections()]);
  } catch (error) {
    ElMessage.error(error.message || "删除版本失败");
  }
}

async function openDetail(row) {
  detailVisible.value = true;
  detailLoading.value = true;
  detail.value = null;
  detailTab.value = "contracts";
  try {
    detail.value = await getPlugin(row.name);
  } catch (error) {
    ElMessage.error(error.message || `加载插件 ${row.name} 详情失败`);
    detailVisible.value = false;
  } finally {
    detailLoading.value = false;
  }
}

async function showUsage(fqName) {
  usageVisible.value = true;
  usageLoading.value = true;
  usage.value = null;
  try {
    usage.value = await getMessageUsage(fqName);
  } catch (error) {
    ElMessage.error(error.message || "查询契约影响面失败");
    usageVisible.value = false;
  } finally {
    usageLoading.value = false;
  }
}

onMounted(() => {
  loadAll();
  loadRejections();
  probeServices();
});

/** 刷新按钮：插件列表、拒绝留痕与服务状态一起重探——状态卡不是只看一次的摆设 */
function refreshAll() {
  loadAll();
  loadRejections();
  probeServices();
}
</script>

<style scoped>
.hub-plugins {
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

.stat-card {
  cursor: default;
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

.stat-info {
  min-width: 0;
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

/* ---------- 服务状态与接入 ---------- */

.service-title {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-weight: 600;
}

.service-state {
  display: inline-flex;
  align-items: center;
  gap: 8px;
}

.endpoint-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
}

.endpoint {
  font-size: 13px;
  word-break: break-all;
}

.service-meta {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 6px;
  margin-top: 6px;
  font-size: 13px;
}

.probe-hint {
  color: var(--el-text-color-secondary);
  line-height: 1.6;
}

.howto-title {
  font-size: 13px;
  color: var(--el-text-color-regular);
}

.code-block {
  margin: 8px 0 4px;
  padding: 10px 12px;
  background: var(--el-fill-color-light);
  border-radius: 6px;
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  font-size: 12px;
  line-height: 1.6;
  /* 配置与命令都不长，折行比横向滚动条好用 */
  white-space: pre-wrap;
  word-break: break-all;
}

.copy-row {
  display: flex;
  justify-content: flex-end;
}

.howto-notes {
  margin: 8px 0 0;
  padding-left: 18px;
  font-size: 12px;
  color: var(--el-text-color-secondary);
  line-height: 1.9;
}

.detail-body {
  min-height: 200px;
}

.offline-block {
  margin-top: 12px;
}

.offline-block :deep(.el-collapse-item__header) {
  color: var(--el-text-color-secondary);
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

.ref-list {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
}

.ref-tag {
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
}

/* 表格行可点击进详情——给个手型，否则用户不会知道 */
:deep(.el-table__row) {
  cursor: pointer;
}

/* 版本删除：tag + danger 链接横排，多个版本换行排开 */
.version-actions {
  display: flex;
  flex-wrap: wrap;
  gap: 12px;
}

.version-action {
  display: inline-flex;
  align-items: center;
  gap: 4px;
}

/* ---------- 详情 · HTTP 调用 ---------- */

.invoke-row {
  display: flex;
  align-items: center;
  gap: 8px;
}

.invoke-method {
  color: var(--el-color-success);
  font-weight: 600;
}

.invoke-url {
  flex: 1;
  min-width: 0;
  font-size: 13px;
  word-break: break-all;
}
</style>
