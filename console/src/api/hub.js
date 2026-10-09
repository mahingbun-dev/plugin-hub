/**
 * 控制中台（plugin-hub）的 HTTP 面封装。
 *
 * **为什么用独立的 axios 实例**——中台返回**裸 JSON + 标准 HTTP 状态码**，
 * 错误语义落在状态码上：409 发布冲突、422 校验拒绝、429 背压、503 熔断。
 * 治理面板与编排页正要靠这些区分决定怎么展示，所以不做任何包封适配。
 *
 * 中台的管理面**按设计没有中台内置鉴权**（管理面全插件化，见
 * `docs/design.md`），所以这里不带 Authorization。将来接 auth 插件时在这一处加即可。
 *
 * 路径与部署侧 nginx 的 `/hub-api/` 前缀剥离对齐：本地由 vite 代理复刻同一行为
 * （见 vite.config.js 的 proxy 配置）。
 */
import axios from "axios";
import { ElMessage } from "element-plus";

export const HUB_BASE = "/hub-api";

const client = axios.create({
  baseURL: HUB_BASE,
  // 触发是同步链，最长可到中台的 300s 上限；读接口远快于此。
  // 单次请求可以按需覆盖（见 triggerFlow）。
  timeout: 60000,
  withCredentials: true,
});

/**
 * 中台的结构化错误。
 *
 * 中台的错误体是 `{error, message, issues, plugin}`（见 hub-api 的 `error.rs`），
 * 机器可读的 `error` 与给人看的 `message` 是分开的。这里把两者都留着——
 * 页面按 `error` 分支（要不要展示问题清单、要不要提示稍后重试），
 * 按 `message` 展示正文。
 */
export class HubError extends Error {
  constructor({ status = 0, error = "network_error", message = "", issues = [], plugin = null }) {
    super(message || error);
    this.name = "HubError";
    this.status = status;
    this.error = error;
    this.issues = issues;
    this.plugin = plugin;
  }

  /** 插件校验器拒绝：issues 里是逐字段的问题清单 */
  get isValidationRejected() {
    return this.error === "validation_rejected";
  }

  /** 总线背压或实例并发到顶——都属于「稍后重试」，不是故障 */
  get isOverloaded() {
    return this.error === "overloaded";
  }

  /** 实例熔断冷却中 */
  get isCircuitOpen() {
    return this.error === "circuit_open";
  }

  /** 发布冲突：草稿被别人改过、或没有草稿可发 */
  get isConflict() {
    return this.error === "conflict";
  }

  get isNotFound() {
    return this.error === "not_found";
  }

  /**
   * 未认证：没带凭证、或凭证不被认。
   *
   * 只有中台配了 auth 插件（`HUB_AUTH_PLUGIN`）时才会出现——没配时管理面
   * 维持原样，不会有 401。
   */
  get isUnauthorized() {
    return this.error === "unauthorized";
  }

  /** 已认证但权限位不够 */
  get isForbidden() {
    return this.error === "forbidden";
  }

  /**
   * 鉴权插件不可用。
   *
   * **与「未登录」是两回事**：中台刻意不把这种情况降级成匿名放行（降级意味着
   * 一次 auth 插件抖动会让整个管理面变成无守卫），所以它会明确回 503。
   * 前端据此提示「稍后重试」，而不是把人踢去重新登录。
   */
  get isAuthUnavailable() {
    return this.error === "auth_unavailable";
  }

  /** 请求根本没到中台（代理没起、端口不通） */
  get isNetwork() {
    return this.status === 0;
  }
}

/**
 * 按 HTTP 状态码给出机器可读的错误码。
 *
 * 中台自己的错误总是带 `error` 字段（见 `error.rs` 的 `ErrorBody`），
 * 但**框架层产生的错误没有**——最典型的是「路由不存在」：
 * axum 直接回一个空体 404，不是 ApiError。
 *
 * 不兜底的话，这种 404 会落成 `http_error`，于是
 * 「这个中台版本还没有这个接口」与「这个资源不存在」就区分不开了——
 * 而控制台对这两件事要说的话完全不同（一个是「重新部署中台」，
 * 一个是「你要找的东西不在」）。
 */
function codeFromStatus(status) {
  switch (status) {
    case 400:
      return "bad_request";
    case 401:
      return "unauthorized";
    case 403:
      return "forbidden";
    case 404:
      return "not_found";
    case 409:
      return "conflict";
    case 422:
      return "validation_rejected";
    case 429:
      return "overloaded";
    case 503:
      return "unavailable";
    default:
      return "http_error";
  }
}

/** 把 axios 的错误规范成 `HubError`。 */
function normalize(error) {
  if (error instanceof HubError) return error;

  const response = error?.response;
  if (!response) {
    // 连不上：本地最常见的原因是中台没起（或 vite 的 /hub-api 代理没配）。
    // 提示里点出来，省得对着「Network Error」猜。
    return new HubError({
      status: 0,
      error: "network_error",
      message: `连不上控制中台（${HUB_BASE}）。本地开发请确认中台已在 8092 端口启动。`,
    });
  }

  const body = response.data;
  // body 可能不是对象（空体 404、HTML 错误页都会落到这里），所以先判一下类型
  const fromBody = body && typeof body === "object" ? body : {};

  return new HubError({
    status: response.status,
    error: fromBody.error || codeFromStatus(response.status),
    message: fromBody.message || `请求失败（HTTP ${response.status}）`,
    issues: fromBody.issues || [],
    plugin: fromBody.plugin ?? null,
  });
}

client.interceptors.response.use(
  (response) => {
    // 默认把响应解成 `response.data`——十几个页面因此少写一层解包。
    // 但**下载**要的是二进制 + 响应头里的文件名，解掉之后两者都拿不到，
    // 所以允许调用方显式要完整响应。
    //
    // 用显式的开关而不是「看 responseType 是不是 blob」：后者会让默认行为
    // 依赖一个隐式约定，读代码的人看不出哪些请求解包、哪些不解。
    if (response.config?.rawResponse) return response;
    return response.data;
  },
  (error) => {
    const normalized = normalize(error);
    handleAuthFailure(normalized);
    return Promise.reject(normalized);
  },
);

/**
 * 认证类错误的统一处理。
 *
 * 放在拦截器里而不是各页面各写一遍：十几个页面，靠自觉必然不一致——
 * 有的会弹错、有的会静默、有的会跳登录。
 */
let lastAuthNoticeAt = 0;

function handleAuthFailure(error) {
  if (!error.isUnauthorized && !error.isForbidden && !error.isAuthUnavailable) {
    return;
  }

  // 认证失败常常是整页并发请求一起失败（页面加载时几个接口同时打出去）。
  // 不去重的话，用户会看到一串一模一样的提示
  const now = Date.now();
  const first = now - lastAuthNoticeAt > 3000;
  if (first) lastAuthNoticeAt = now;
  if (!first) return;

  if (error.isUnauthorized) {
    ElMessage.warning("登录已过期，请重新登录");
    redirectToLogin();
    return;
  }

  if (error.isForbidden) {
    // 中台的消息里带着「缺哪个权限位、当前有哪些」，直接展示这句——
    // 它比前端能编的任何话都准
    ElMessage.error(error.message || "当前账号没有这个操作的权限");
    return;
  }

  // 鉴权服务不可用**不是登录问题**：中台刻意不把这种情况降级成匿名放行
  // （降级意味着一次 auth 插件抖动会让整个管理面变成无守卫），所以它回 503。
  // 这里提示「稍后重试」，而不是把人踢去重新登录——那解决不了问题
  ElMessage.error("鉴权服务暂时不可用，请稍后重试");
}

/**
 * 认证失败时的处理。
 *
 * 本 demo 不带登录页：只有中台配置了 auth 插件（`HUB_AUTH_PLUGIN`）才会出现
 * 401，此时提示检查配置即可——把人引去一个不存在的登录页更糟。
 */
function redirectToLogin() {
  ElMessage.info("中台启用了鉴权插件（HUB_AUTH_PLUGIN），本 demo 未接入登录流程");
}

// ------------------------------------------------------------------ 健康

/** 中台自身的存活状态。控制台用它判断「中台挂了」与「这条数据没有」的区别。 */
export function getHealth() {
  return client.get("/health");
}

/**
 * 对外接入端点（`GET /endpoints`）。
 *
 * MCP 的接入地址是**部署属性**：agent 所在的机器不一定解析得了对外域名，要用 IP——
 * 而控制台只能从浏览器地址推导出域名形态，IP 只有部署者知道，由
 * `HUB_MCP_PUBLIC_ENDPOINT` 配置、经这里下发（与 `HUB_PLUGIN_PUBLIC_ADDR` 同一个
 * 「不猜」）。`mcp` 为 `null` = 未配置；旧版中台没有这个接口时 404，调用方也按
 * 未配置处理。
 */
export function getPublicEndpoints() {
  return client.get("/endpoints");
}

// ------------------------------------------------------------------ 服务探测

/**
 * MCP 面的**真实探测**：从浏览器走一遍完整的 MCP 握手。
 *
 * 为什么不是问中台「MCP 挂载了吗」——HTTP 面与 MCP 面在同一个进程、同一个
 * 监听器上，后端自报「已挂载」恒为真，是假状态。而部署上真正会出的问题
 * （nginx 缺 `location = /mcp` 的转发段、`HUB_MCP_ALLOWED_HOSTS` 没配当前域名）
 * **只有从外部经 nginx 打 /mcp 才现形**（README 记录过 UAT 上 /mcp 全 403 的坑）。
 * 控制台与对外端口同源，从这里探测恰好等价于外部 agent 的视角。
 *
 * 探测即真实接入的四步：`initialize` → `notifications/initialized` →
 * `tools/list` → `DELETE` 关会话（不给服务端的会话表留探测垃圾）。
 * 前一步成功就足以判「在线」，tools/list 只是顺带把真实工具数带回页面。
 *
 * **用 fetch 而不是上面的 axios 实例**：探测把 401/403 当作正常结果之一
 * （管理面鉴权启用后，没有 `hub:invoke` 权限位的账号拿到的就是这个响应），
 * 不能落进 `handleAuthFailure` 的「未登录就跳登录」逻辑——那会把一次正常的
 * 状态显示变成把人踢出控制台。
 *
 * 返回 `{ok, endpoint, latencyMs, toolCount?, protocolVersion?, errorKind?, httpStatus?}`。
 * `errorKind`：`network`（请求根本没到）｜`host_rejected`（rmcp 的 Host 白名单拒绝，
 * 配 `HUB_MCP_ALLOWED_HOSTS` 可解）｜`forbidden`（服务在线但当前账号无权限位）｜
 * `bad_shape`（端点有 200 响应但不是 MCP——多半是转发段没配，落到了前端兜底路由）｜
 * `http_error`（其余非 2xx）。
 */
export async function probeMcpService() {
  const endpoint = `${window.location.origin}/mcp`;
  const started = performance.now();
  const done = (result) => ({
    endpoint,
    latencyMs: Math.round(performance.now() - started),
    ...result,
  });

  /**
   * 响应体可能是 SSE（rmcp 默认）也可能是裸 JSON，两者都兼容。
   * rmcp 的流里会先有空的 `data:` 心跳行，所以取**最后一条非空**的 data 行。
   */
  async function readMessage(response) {
    const text = await response.text();
    const dataLines = text
      .split("\n")
      .map((line) => line.trim())
      .filter((line) => line.startsWith("data:") && line.slice(5).trim());
    for (let i = dataLines.length - 1; i >= 0; i -= 1) {
      try {
        return JSON.parse(dataLines[i].slice(5).trim());
      } catch {
        /* 不是 JSON，试上一条 */
      }
    }
    return null;
  }

  try {
    const initResponse = await fetch(endpoint, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        // Streamable HTTP 规定 POST 的 Accept 必须同时带这两种类型，缺一个被拒
        Accept: "application/json, text/event-stream",
      },
      body: JSON.stringify({
        jsonrpc: "2.0",
        id: 1,
        method: "initialize",
        params: {
          protocolVersion: "2025-06-18",
          capabilities: {},
          clientInfo: { name: "hub-console", version: "1.0.0" },
        },
      }),
    });

    if (!initResponse.ok) {
      const body = await initResponse.text().catch(() => "");
      if (initResponse.status === 403 && body.includes("Host header is not allowed")) {
        return done({ ok: false, errorKind: "host_rejected", httpStatus: 403 });
      }
      if (initResponse.status === 401 || initResponse.status === 403) {
        // 服务在线（鉴权层应答了），只是这个账号没有 hub:invoke——与「服务挂了」分开显示
        return done({ ok: false, errorKind: "forbidden", httpStatus: initResponse.status });
      }
      return done({ ok: false, errorKind: "http_error", httpStatus: initResponse.status });
    }

    const sessionId = initResponse.headers.get("mcp-session-id") || "";
    const initMessage = await readMessage(initResponse);
    if (!initMessage?.result?.protocolVersion) {
      return done({ ok: false, errorKind: "bad_shape", httpStatus: 200 });
    }
    const protocolVersion = initMessage.result.protocolVersion;

    const post = (payload) =>
      fetch(endpoint, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          Accept: "application/json, text/event-stream",
          "MCP-Protocol-Version": protocolVersion,
          ...(sessionId ? { "mcp-session-id": sessionId } : {}),
        },
        body: JSON.stringify(payload),
      });

    // initialized 通知（202 无 body）与 tools/list 失败不推翻「在线」——
    // initialize 已经证明了「nginx /mcp 转发 + Host 白名单 + MCP 面」整条链是通的
    let toolCount;
    try {
      await post({ jsonrpc: "2.0", method: "notifications/initialized" });
      const listMessage = await readMessage(
        await post({ jsonrpc: "2.0", id: 2, method: "tools/list", params: {} }),
      );
      if (Array.isArray(listMessage?.result?.tools)) {
        toolCount = listMessage.result.tools.length;
      }
    } catch {
      /* 已能判在线，工具数拿不到就算了 */
    }

    // 关会话。不等结果也不让它抛——这是清理，不是探测的一部分
    if (sessionId) {
      fetch(endpoint, {
        method: "DELETE",
        headers: {
          "mcp-session-id": sessionId,
          "MCP-Protocol-Version": protocolVersion,
        },
      }).catch(() => {});
    }

    return done({ ok: true, toolCount, protocolVersion });
  } catch {
    return done({ ok: false, errorKind: "network" });
  }
}

// ------------------------------------------------------------------ 插件目录

/** 插件总览：插件 + 版本数 + 在线实例数 */
export function listPlugins() {
  return client.get("/admin/plugins");
}

/** 插件详情：逐版本给出契约、MCP 工具与实例 */
export function getPlugin(name) {
  return client.get(`/admin/plugins/${encodeURIComponent(name)}`);
}

/**
 * 最近的注册拒绝留痕：谁被拒、拒绝码、原因、已重试多少次。
 *
 * 「插件实例 0 个但容器还活着」的答案在这里——被拒的插件会无限重试注册，
 * 中台每轮都拒，实例表里根本没有这一行，拒绝原因唯一可查的地方就是留痕。
 */
export function listRegisterRejections({ plugin, limit } = {}) {
  return client.get("/admin/rejections", {
    params: { plugin, limit },
  });
}

/**
 * 删除一个已登记的版本（不可逆）。
 *
 * VERSION_CONFLICT / BREAKING_CHANGE 拒死后的恢复操作：版本连同契约、
 * 工具与实例一起级联删除，插件方修好 manifest 重新注册即可重新登记。
 */
export function deletePluginVersion(name, version) {
  return client.delete(
    `/admin/plugins/${encodeURIComponent(name)}/versions/${encodeURIComponent(version)}`,
  );
}

/** 全部在线实例（已经把插件名与版本摊平进来） */
export function listInstances() {
  return client.get("/admin/instances");
}

/** 某个消息类型被谁生产、被谁消费——改契约前的影响面分析 */
export function getMessageUsage(fqName) {
  return client.get(`/admin/messages/${encodeURIComponent(fqName)}`);
}

/**
 * 治理状态：实例级的并发占用与熔断。
 *
 * 这些数字全在**中台进程的内存里**，不落库——它们是「此刻」的事实。
 * 多实例部署时，每个中台只回答自己那一份。
 */
export function getGovernance() {
  return client.get("/admin/governance");
}

// ------------------------------------------------------------------ 编排（读）

/** flow 列表：名字、描述、当前发布版本、是否有草稿 */
export function listFlows() {
  return client.get("/flows");
}

/** 一条 flow 的全貌：草稿、已发布版本、历史修订 */
export function getFlow(name) {
  return client.get(`/flows/${encodeURIComponent(name)}`);
}

/** 执行记录列表 */
export function listRuns({ flow, limit = 100 } = {}) {
  const params = { limit };
  if (flow) params.flow = flow;
  return client.get("/runs", { params });
}

/** 一次执行的全貌：状态、耗时与每个节点的明细 */
export function getRun(runId) {
  return client.get(`/runs/${encodeURIComponent(runId)}`);
}

/** 最近的调用链（按 trace 聚合，不是按 span 铺开） */
export function listTraces({ limit = 50 } = {}) {
  return client.get("/traces", { params: { limit } });
}

/** 一条调用链的全部 span + 它涉及的 run */
export function getTrace(traceId) {
  return client.get(`/traces/${encodeURIComponent(traceId)}`);
}

/**
 * 插件调用审计：谁、何时、调了什么插件、结果、耗时。
 *
 * 归 hub:admin 的门（/admin/plugin-audit）。数据随 span 保留期（默认 7 天）。
 */
export function listPluginAudit({ plugin, caller, status, limit = 20, offset = 0 } = {}) {
  return client.get("/admin/plugin-audit", {
    params: { plugin, caller, status, limit, offset },
  });
}

// ------------------------------------------------------------------ 编排（写）

/**
 * 保存草稿。
 *
 * **保存不会被拒**——即使有阻断性问题也存得下来（编排是一步步改出来的，
 * 中途存不下来会很别扭）。返回体里的 `blocked` 指示能不能发布，
 * `issues` 是校验发现的问题（可能有警告而无错误）。
 */
export function saveDraft(name, { definition, description = "", createdBy = "" }) {
  return client.post(`/flows/${encodeURIComponent(name)}/draft`, {
    definition,
    description,
    created_by: createdBy,
  });
}

/** 发布草稿。发布前中台会重新校验，有阻断性问题则拒绝（409/400）。 */
export function publishFlow(name) {
  return client.post(`/flows/${encodeURIComponent(name)}/publish`);
}

/**
 * 给从未发布过的 flow 改名。发布过的会被拒（400）——名字是身份，
 * 调用方 URL 与触发器都按名字引用它，那条线划在「发布」上。
 */
export function renameFlow(name, newName) {
  return client.post(`/flows/${encodeURIComponent(name)}/rename`, {
    name: newName,
  });
}

/**
 * 同步触发一次 flow。
 *
 * 超时按调用方的预算给：同步链最长 300s（中台上限），比默认的 60s 放宽，
 * 否则一条慢链会在前端先超时，而中台那边其实还在正常跑——那会让人误以为执行失败。
 */
export function triggerFlow(name, { payload, messageId, meta, timeoutMs }) {
  const body = { payload };
  if (messageId) body.message_id = messageId;
  if (meta) body.meta = meta;
  if (timeoutMs) body.timeout_ms = timeoutMs;
  return client.post(`/flows/${encodeURIComponent(name)}/trigger`, body, {
    timeout: (timeoutMs || 30000) + 15000,
  });
}

/** 异步触发：立刻返回一个 run 句柄，执行由总线上的消费者负责 */
export function triggerFlowAsync(name, { payload, messageId, meta, timeoutMs } = {}) {
  const body = { payload };
  if (messageId) body.message_id = messageId;
  if (meta) body.meta = meta;
  if (timeoutMs) body.timeout_ms = timeoutMs;
  return client.post(`/flows/${encodeURIComponent(name)}/trigger-async`, body);
}

// ------------------------------------------------------------------ 死信

/** 死信列表。`pending` 为 true 时只看还没重放过的（中台默认即 true）。 */
export function listDeadLetters({ pending = true, limit = 100 } = {}) {
  return client.get("/dead-letters", { params: { pending, limit } });
}

export function getDeadLetter(id) {
  return client.get(`/dead-letters/${encodeURIComponent(id)}`);
}

/**
 * 重放一封死信。
 *
 * `payload` **必须由调用方提供**：死信表里只有摘要（类型/字节数/meta 键），
 * 没有全量报文。所以控制台的重放不能是「一键」——它得先让人把载荷填回来。
 * 这一点在界面上要如实表达，不能做成一个看起来能一键搞定、实际发出去空载荷的按钮。
 */
export function replayDeadLetter(id, { payload, meta, timeoutMs } = {}) {
  const body = { payload };
  if (meta) body.meta = meta;
  if (timeoutMs) body.timeout_ms = timeoutMs;
  return client.post(`/dead-letters/${encodeURIComponent(id)}/replay`, body);
}

// ------------------------------------------------------------------ 触发器

/**
 * 全部触发器（含已停用的）。
 *
 * **带停用的那批是刻意的**：排障时问「它为什么没跑」，第一个答案往往是
 * 「它被关掉了」——只列启用中的会把这个答案藏起来。
 */
export function listTriggers() {
  return client.get("/triggers");
}

/**
 * 登记或更新一条触发器。
 *
 * 同名视为同一条（改而不是新增），并且会把它**置回启用**——
 * 登记一份配置的意图就是「让它按这个跑」。
 */
export function saveTrigger(flow, { kind, name, config }) {
  return client.post(`/flows/${encodeURIComponent(flow)}/triggers`, {
    kind,
    name,
    config,
  });
}

export function setTriggerEnabled(id, enabled) {
  return client.post(`/triggers/${encodeURIComponent(id)}/enabled`, { enabled });
}

export function deleteTrigger(id) {
  return client.delete(`/triggers/${encodeURIComponent(id)}`);
}

// --------------------------------------------------- 插件开发接入（脚手架模板）

/**
 * 各语言的插件脚手架模板。
 *
 * 每一项都带着「这份模板是什么时候构建的」、文件数与包大小——开发者据此判断
 * 手里那份是不是过期的。**这不是装饰**：模板会腐烂，而它随中台发版，
 * 所以「构建时间」是页面上唯一能回答「我拿到的是哪一版」的东西。
 */
export function listPluginTemplates() {
  return client.get("/plugin-templates");
}

/**
 * 下载一份插件工程。
 *
 * 与别的接口有两处不同：
 *
 * 1. `responseType: "blob"` —— 要的是二进制，不是 JSON。
 * 2. `rawResponse: true` —— 文件名由中台拼在 `Content-Disposition` 里，
 *    默认那层解包会把它连着响应头一起丢掉。**不在前端自己拼一个**：
 *    中台按语言与插件名拼的名字与它自己的命名规则同源，前端再拼一遍就是等着漂移。
 */
export async function downloadPluginTemplate(lang, { name, package: pkg } = {}) {
  const params = { name };
  if (pkg) params.package = pkg;

  try {
    const response = await client.get(
      `/plugin-templates/${encodeURIComponent(lang)}/download`,
      { params, responseType: "blob", rawResponse: true },
    );

    return {
      blob: response.data,
      // 取不到就回落到一个朴素的名字，而不是让调用方拿到 undefined
      filename:
        filenameFromDisposition(response.headers?.["content-disposition"]) ||
        `${name}.zip`,
    };
  } catch (error) {
    throw await rethrowBlobError(error);
  }
}

/**
 * 从 `Content-Disposition` 里取文件名。
 *
 * 中台给的是 `attachment; filename="plugin-hub-go-plugin-order-reader.zip"`。
 * 取不到返回 null，由调用方回落——**不在这里编一个假的**。
 */
function filenameFromDisposition(header) {
  if (!header) return null;
  const match = /filename\*?=(?:UTF-8'')?"?([^";]+)"?/i.exec(header);
  if (!match) return null;
  try {
    return decodeURIComponent(match[1]);
  } catch {
    return match[1];
  }
}

/**
 * 把 blob 形式的错误体读回成结构化错误。
 *
 * 中台的错误体是 JSON（`{error, message, issues}`），而这次请求要的是 blob——
 * 于是它被包成了一个 Blob，`normalize` 读不出 `error` / `message`，只会退化成
 * 「请求失败（HTTP 400）」。那句话对调用方没有价值：真正有用的是中台说的那一句
 * （比如「插件名非法：只允许字母数字与 -_」）。这里把它读回来再抛。
 *
 * 读不出 JSON 时返回原错误——那是代理的错误页之类，不该把它盖掉。
 */
async function rethrowBlobError(error) {
  const blob = error?.response?.data;
  if (typeof Blob === "undefined" || !(blob instanceof Blob)) return error;

  try {
    const parsed = JSON.parse(await blob.text());
    return new HubError({
      status: error.status,
      error: parsed.error || error.error,
      message: parsed.message || error.message,
      issues: parsed.issues || [],
      plugin: parsed.plugin ?? null,
    });
  } catch {
    return error;
  }
}

export default client;
