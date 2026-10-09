<template>
  <div class="hub-onboarding">
    <!-- 标题 -->
    <el-card class="title-card">
      <div class="page-header">
        <div>
          <div class="header-title">
            <el-icon :size="28" style="margin-right: 12px"><Download /></el-icon>
            <span>插件开发接入</span>
          </div>
          <div class="header-subtitle">
            下载一份可运行的插件工程：含源码骨架、接入指南与契约一致性测试，不用先拉中台仓库
          </div>
        </div>
        <div class="header-actions">
          <el-tag v-if="hubOffline" type="danger" effect="dark" size="large">
            中台不可达
          </el-tag>
          <el-button :loading="loading" @click="loadAll">
            <el-icon><Refresh /></el-icon> 刷新
          </el-button>
        </div>
      </div>
    </el-card>

    <!-- 中台不可达时给出可行动的说明，而不是一个空列表 -->
    <el-alert
      v-if="hubOffline"
      type="error"
      :closable="false"
      show-icon
      style="margin-bottom: 16px"
      title="连不上控制中台"
      :description="hubOfflineMessage"
    />

    <!-- 中台没配对外地址时如实说出来。
         **不说「工程里是占位文字」**——那要看模板有没有引用 @@hub_addr@@。
         早先这里写的是「目前还没有模板引用它」，那已经过期了：五门模板的 AGENTS.md
         都引用它（README 里四门有、Go 那份没有）。但页面仍然只报它知道的事实：
         配置状态。要判断「工程里到底是不是占位文字」，得知道每门模板引没引用——
         那是中台侧的知识，前端不该猜。 -->
    <el-alert
      v-if="!hubOffline && !anyAddrConfigured"
      type="warning"
      :closable="false"
      show-icon
      style="margin-bottom: 16px"
      title="中台未配置对外地址"
      description="下载到的工程里，中台地址需要你自己确认（问中台维护方，或看部署文档）。中台没配 HUB_PLUGIN_PUBLIC_ADDR，所以它不知道自己对外的插件面地址，也就没法替你填。"
    />

    <!-- 语言卡片 -->
    <el-row :gutter="16">
      <el-col v-for="tpl in templates" :key="tpl.lang" :xs="24" :sm="12" :lg="8">
        <el-card shadow="hover" class="tpl-card">
          <div class="tpl-head">
            <span class="tpl-name">{{ tpl.display_name }}</span>
            <el-tag size="small" type="info" effect="plain">{{ tpl.lang }}</el-tag>
          </div>

          <div class="tpl-desc">{{ tpl.description }}</div>

          <div class="tpl-meta">
            <div class="meta-row">
              <span class="meta-label">模板构建于</span>
              <span class="meta-value">{{ formatTime(tpl.built_at) }}</span>
            </div>
            <div class="meta-row">
              <span class="meta-label">适配中台</span>
              <span class="meta-value">{{ tpl.hub_version }}</span>
            </div>
            <div class="meta-row">
              <span class="meta-label">包含</span>
              <span class="meta-value">
                {{ tpl.file_count }} 个文件 · 约 {{ formatSize(tpl.size_bytes) }}
              </span>
            </div>
          </div>

          <div class="tpl-actions">
            <el-button type="primary" @click="openDownload(tpl)">
              <el-icon><Download /></el-icon> 下载工程
            </el-button>
            <!--
              只有 Go 有脚手架 CLI，所以这个按钮<strong>只在 go 上出现</strong>。
              给别的语言显示一条 Go 的命令是假的——而假命令比没有命令更费时间：
              照着敲会得到 command not found，然后开始怀疑文档是不是过期了。
            -->
            <el-button v-if="tpl.lang === 'go'" @click="copyCommand">
              <el-icon><DocumentCopy /></el-icon> 复制命令
            </el-button>
          </div>

          <div class="tpl-hint">
            已有中台仓库检出、且要写 Go 插件？也可以直接在中台仓库的 <code>sdk/go</code>
            下跑 <code>go run ./cmd/hub-plugin new &lt;插件名&gt;</code>——
            <strong>两条路生成的是同一批文件</strong>，用哪条都一样。
          </div>
        </el-card>
      </el-col>
    </el-row>

    <el-empty
      v-if="!loading && !hubOffline && templates.length === 0"
      description="中台没有提供任何语言的模板"
    />

    <!-- 下载参数 -->
    <el-dialog v-model="dlgVisible" title="下载插件工程" width="520px">
      <el-form label-width="90px" @submit.prevent>
        <el-form-item label="插件名" required>
          <el-input
            v-model="form.name"
            placeholder="order-reader"
            maxlength="64"
            show-word-limit
            @input="onNameInput"
          />
          <div class="field-hint">
            只允许字母、数字、连字符与下划线，以字母数字开头——<strong>与中台注册时的规则一致</strong>，
            在这里挡下来，省得下载完再被中台拒一次。
          </div>
          <div v-if="nameTouched && !nameValid" class="field-error">
            这个名字中台会拒绝：{{ nameError }}
          </div>
        </el-form-item>

        <el-form-item label="包名" :required="pkgRequired">
          <el-input
            v-model="form.package"
            :placeholder="pkgPlaceholder"
            @input="pkgTouched = true"
          />
          <div v-if="pkgRequired" class="field-hint">
            <strong>{{ dlg?.display_name }} 的包名是<em>命名空间</em></strong>，不能带
            连字符——比如 <code>OrderReader</code>。留空会拿插件名当包名，而插件名带
            <code>-</code> 时那份工程<strong>编译不过</strong>，所以这里必须填。
          </div>
          <div v-else class="field-hint">留空就用插件名。</div>
          <div v-if="pkgRequired && pkgTouched && !packageValid" class="field-error">
            这门语言的包名不能为空
          </div>
        </el-form-item>
      </el-form>

      <!-- 中台的 400 留在弹窗里显示，而不是弹个 toast 就把弹窗关掉：
           「这个输入框要改」的提示得和输入框待在一起，用户才能边看边改。
           中台那句原文比前端能编的任何话都准（它带着「要什么形状 + 正确例子」）。 -->
      <el-alert
        v-if="dlgError"
        type="error"
        :closable="false"
        show-icon
        class="dlg-error"
        :title="dlgError"
      />

      <template #footer>
        <el-button @click="dlgVisible = false">取消</el-button>
        <el-button
          type="primary"
          :loading="downloading"
          :disabled="!formValid"
          @click="doDownload"
        >
          下载
        </el-button>
      </template>
    </el-dialog>
  </div>
</template>

<script setup>
import { computed, onMounted, reactive, ref } from "vue";
import { ElMessage } from "element-plus";
import {
  downloadPluginTemplate,
  listPluginTemplates,
  HubError,
} from "@/api/hub";

const loading = ref(false);
const hubOffline = ref(false);
const hubOfflineMessage = ref("");
const templates = ref([]);

const dlgVisible = ref(false);
const dlg = ref(null);
const downloading = ref(false);
const nameTouched = ref(false);
const pkgTouched = ref(false);
/** 中台回的 400 原文。留在弹窗里显示，见模板里那段说明。 */
const dlgError = ref("");
const form = reactive({ name: "", package: "" });

// 中台没配对外地址时，所有模板都报 false——取第一个即可，它们来自同一个配置
const anyAddrConfigured = computed(() =>
  templates.value.length === 0 ? true : !!templates.value[0].plugin_addr_configured,
);

/**
 * 插件名规则。**与中台的 `is_valid_plugin_name` 一致**（字母数字开头，
 * 其后只允许字母数字与 `-_`，最长 64）。
 *
 * 这里挡一道是为了少一次往返，不是为了替中台做判断——**判据仍是中台那条**：
 * 后端用的是同一个函数，所以这里放行的名字后端一定也放行。
 * 注意它比 `hub-plugin new` 宽松（那个只允许小写字母开头）。
 */
const NAME_RE = /^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$/;

const nameValid = computed(() => NAME_RE.test(form.name.trim()));

const nameError = computed(() => {
  const n = form.name.trim();
  if (!n) return "不能为空";
  if (n.length > 64) return "最长 64 个字符";
  if (!/^[A-Za-z0-9]/.test(n)) return "要以字母或数字开头";
  return "只允许字母、数字、连字符与下划线";
});

/**
 * 哪几门语言**必须**显式给包名（留空会拿插件名当包名，而那门语言不收）。
 *
 * 只有 C#：它的包名是**命名空间**，而插件名允许连字符（`order-reader`），
 * `namespace order-reader;` 直接是语法错。另外四门把插件名当包名都合法——Go 是
 * module 路径、Rust 是 crate 名、Node 是包名、Python 是分发名，四者都收 `-`。
 *
 * ⚠️ **这是前端唯一一处知道「某门语言的包名规则」的地方，而它本不该知道**：判据的
 * 事实来源在中台的 `hub_templates::validate_package`（按语言各一套，且都是实测出来的）。
 * 这里的作用只有一个——别让**默认路径**直接撞墙。真要更准，得让列表接口带一个字段：
 *
 *   TODO(中台): `TemplateInfo` 加 `package_required: bool` 与 `package_default: string`，
 *               这里改成读接口，加第六门语言时就不用回来改前端了。
 *
 * 它兜不住的部分由中台的 400 兜住，页面把原文显示在弹窗里（见 `doDownload`）。
 */
const PACKAGE_REQUIRED_LANGS = new Set(["csharp"]);

const pkgRequired = computed(() => PACKAGE_REQUIRED_LANGS.has(dlg.value?.lang));

/** 包名非空即算「填了」。**不在前端复刻各语言的正则**——那是中台的判据。 */
const packageValid = computed(() => !pkgRequired.value || form.package.trim() !== "");

const formValid = computed(() => nameValid.value && packageValid.value);

/**
 * 按插件名推一个包名建议值。
 *
 * 只做一件事：把 `order-reader` / `order_reader` 变成 `OrderReader`（PascalCase），
 * 也就是 C# 命名空间的样子。**它不是校验，只是省得人手敲**；用户改了就以用户为准。
 */
function suggestPackage(name) {
  const parts = name.trim().split(/[-_]+/).filter(Boolean);
  if (parts.length === 0) return "";
  return parts.map((p) => p.charAt(0).toUpperCase() + p.slice(1)).join("");
}

const pkgPlaceholder = computed(
  () => suggestPackage(form.name) || dlg.value?.package_hint || "",
);

/**
 * 插件名变了：需要包名的那门语言，替用户把它填上（**只填没被手动改过的**）。
 *
 * 判据用 `pkgTouched` 而不是「当前是否为空」：用户把建议值删掉、打算自己填时，
 * 这里不该再塞回去——那就成了一个删不掉的默认值。
 */
function onNameInput() {
  nameTouched.value = true;
  dlgError.value = "";
  if (pkgRequired.value && !pkgTouched.value) {
    form.package = suggestPackage(form.name);
  }
}

function formatTime(value) {
  if (!value) return "—";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return String(value);
  const pad = (n) => String(n).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ` +
    `${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

/** 字节数说成人能估量的样子。包大小是近似值，所以用「约」而不是精确到个位。 */
function formatSize(bytes) {
  if (!bytes) return "—";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

async function loadAll() {
  loading.value = true;
  hubOffline.value = false;
  try {
    templates.value = await listPluginTemplates();
  } catch (err) {
    if (err instanceof HubError && err.isNetwork) {
      hubOffline.value = true;
      hubOfflineMessage.value = err.message;
      templates.value = [];
    } else if (err instanceof HubError && err.isNotFound) {
      // 中台版本比控制台旧时会出现：接口还没有。说清楚，别让人以为是模板没了
      ElMessage.error("这个中台还没有「插件模板」接口，升级中台后再试");
      templates.value = [];
    } else {
      ElMessage.error(err?.message || "加载模板列表失败");
      templates.value = [];
    }
  } finally {
    loading.value = false;
  }
}

function openDownload(tpl) {
  dlg.value = tpl;
  form.name = "";
  form.package = "";
  nameTouched.value = false;
  pkgTouched.value = false;
  dlgError.value = "";
  dlgVisible.value = true;
}

async function doDownload() {
  if (!nameValid.value) {
    nameTouched.value = true;
    return;
  }
  if (!packageValid.value) {
    pkgTouched.value = true;
    return;
  }

  downloading.value = true;
  dlgError.value = "";
  try {
    const { blob, filename } = await downloadPluginTemplate(dlg.value.lang, {
      name: form.name.trim(),
      package: form.package.trim(),
    });
    saveBlob(blob, filename);
    ElMessage.success(`已下载 ${filename}`);
    dlgVisible.value = false;
  } catch (err) {
    // 400 = 这次输入不合这门语言的口径（中台按语言各有一套判据，同一句话在五门里
    // 代表五种东西）。**留在弹窗里显示**：这是「你这个输入框要改」，而弹窗一关提示
    // 就跟着没了，用户得重开一遍才能改。中台那句原文带着「要什么形状 + 正确例子」，
    // 比前端能编的任何话都准，原样展示。
    if (err instanceof HubError && err.status === 400) {
      dlgError.value = err.message;
      return;
    }
    ElMessage.error(err?.message || "下载失败");
  } finally {
    downloading.value = false;
  }
}

/** 触发浏览器下载。名字用中台给的那个（见 api/hub.js 的说明）。 */
function saveBlob(blob, filename) {
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  a.remove();
  URL.revokeObjectURL(url);
}

async function copyCommand(tpl) {
  const cmd = `go run ./cmd/hub-plugin new order-reader`;
  try {
    await navigator.clipboard.writeText(cmd);
    ElMessage.success("已复制——在中台仓库的 sdk/go 目录里执行");
  } catch {
    // 剪贴板在非 https 或没授权时会失败。别静默，把命令打出来让人自己复制
    ElMessage.info(cmd);
  }
}

onMounted(loadAll);
</script>

<style scoped>
.hub-onboarding {
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

.tpl-card {
  margin-bottom: 16px;
  /* 卡片高度不固定：描述长短不一，固定高度会截断文案或留下大片空白 */
}

.tpl-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 8px;
}

.tpl-name {
  font-size: 17px;
  font-weight: 600;
}

.tpl-desc {
  min-height: 40px;
  font-size: 13px;
  line-height: 1.6;
  color: var(--el-text-color-regular);
}

.tpl-meta {
  margin: 12px 0;
  padding: 10px 12px;
  border-radius: 8px;
  background: var(--el-fill-color-light);
}

.meta-row {
  display: flex;
  justify-content: space-between;
  gap: 10px;
  font-size: 12px;
  line-height: 1.9;
}

.meta-label {
  color: var(--el-text-color-secondary);
  /* 标签不压缩，让值那边换行——反过来的话「模板构建于」会被切成两行 */
  flex-shrink: 0;
}

.meta-value {
  color: var(--el-text-color-primary);
  text-align: right;
  word-break: break-all;
}

.tpl-actions {
  display: flex;
  gap: 8px;
}

.tpl-hint {
  margin-top: 10px;
  font-size: 12px;
  line-height: 1.6;
  color: var(--el-text-color-secondary);
}

.tpl-hint code {
  padding: 1px 5px;
  border-radius: 4px;
  background: var(--el-fill-color);
  font-size: 11px;
  /* 不让代码在连字符处断行：浏览器默认会在 `hub-plugin` 的 `-` 处折行，
     于是命令被切成两半（`.../cmd/hub-` + `plugin new ...`），读起来像两条命令。
     代码片段总共没几个字符，nowrap 不会撑破卡片。 */
  white-space: nowrap;
}

.field-hint {
  margin-top: 4px;
  font-size: 12px;
  line-height: 1.6;
  color: var(--el-text-color-secondary);
}

.field-error {
  margin-top: 4px;
  font-size: 12px;
  color: var(--el-color-danger);
}

/* 中台的 400 原文：贴在表单下方，与输入框待在一起 */
.dlg-error {
  margin-top: 4px;
}
</style>
