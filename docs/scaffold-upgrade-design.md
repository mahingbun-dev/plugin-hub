# 脚手架 M6 升级 + 各语言升级补丁包 —— 实施设计（已定案）

> 本文档是全部实施任务的唯一规格来源。实现中如发现规格与现实冲突，以现有代码真实约束为准并在产出 notes 里记录偏差。
> 代码风格：中文注释、解释「为什么」；与所在文件现有注释密度一致；不顺手重构无关代码；**不执行 git commit**（主会话统一提交）。
> 铁律：不引入任何新外部 crate（保 Cargo.lock 不变，免 vendor 快照重传）。

## 1. 背景与目标

M6（PluginGateway 互调/发现、五门 SDK GatewayClient/publish/invoke_plugin）已上线，但五门脚手架模板仍停留在 M6 之前——新生成的插件工程不知道有互调能力，存量插件更没有低成本升级路径。本期交付：

1. **五门脚手架模板升级**：新生成的插件工程自带互调/发现示例与升级指引；
2. **每语言升级补丁包**：给存量插件项目幂等注入 M6 示例文件 + README 升级节 + 检查清单；经 CLI（`hub-scaffold upgrade`）与 HTTP（`GET /plugin-templates/{lang}/upgrade-package`）两条通道下发。

## 2. 决策基线

1. 补丁包形态：**幂等升级脚本 + 不撞名新文件 + README marker 追加**，不用 unified diff（存量项目必然已漂移，diff 不可靠）。
2. 新增文件刻意避开存量项目撞名面（README.md/Dockerfile/plugin.* 等一律不改内容，README 用 marker 追加）。
3. 示例文件是「休眠演示」：编译通过、不接线到插件主流程，不改变任何现有行为。
4. manifest 默认**不**声明 invokes（declared 策略下声明会改变语义），只在 manifest 构造处加注释示例。
5. Cargo.lock 必须不变：嵌入复用 `include_bytes!`，打包复用 `zip` crate（Stored）。

## 3. 模板升级（五门新项目）

每门模板加两个东西（go 模板目录在 `sdk/go/cmd/hub-plugin/templates/`，其余在 `sdk/<lang>/templates/plugin/`）：

### 3.1 新文件 gateway_example（各门落点固定）

| 语言 | 文件 | 说明 |
|---|---|---|
| go | `gateway_example.go.tmpl` | `package main`，导出函数（Go 对未使用函数不报错） |
| python | `gateway_example.py.tmpl` | 模块级演示函数 |
| node | `src/gateway-example.ts.tmpl` | **先查 tsconfig.tmpl 的 noUnusedLocals/noUnusedParameters**：开着就用 `export` 修饰规避 |
| rust | `examples/gateway_demo.rs.tmpl` | 放 cargo examples（不进 crate 编译，规避 smoke 的「rust 不允许编译警告」） |
| csharp | `GatewayExample.cs.tmpl` | public 类，无警告问题 |

示例内容（休眠、可编译、按真实 SDK 签名，禁止编造 API）：
- **发现**：构造/取得 GatewayClient 后 DescribeMessage（如 `google.protobuf.Struct`）与 ListPlugins 的调用示例；
- **同步互调**：`invoke_plugin(目标, payload, current_envelope=当前信封)`——注释讲清：传当前信封=trace 贯通+防环链自动带上（原样复制不追加自己，中台负责追加 caller）；REJECTED/ERROR 的类型化异常处理；
- **异步发布**：`publish(target, envelope)` 触发下游 flow；
- **manifest 声明**：manifest 构造处加注释示例（`// invokes: ["目标插件名"] —— declared 策略下声明才会放行调用`）。

各门真实 API（照抄，不得改名）：
- **Go**：`GatewayClient` 由骨架经 `GatewayAware` 接口 `SetGateway(*hubkit.GatewayClient)` 注入（run.go:352）；`c.InvokePlugin(ctx, "目标", hubkit.InvokeOptions{Version: "", Timeout: 30*time.Second, CurrentEnvelope: env, PayloadJSON: map[string]any{...}})` 返回 `*hubv1.Envelope`；`hubkit.NewULID()`/`hubkit.NewEnvelope()`；错误类型 `*hubkit.InvokeRejected`/`*hubkit.InvokeFailed`（errors.As）。
- **Python**：插件实现 `set_gateway`（`hubkit/plugin.py` 的注入约定），`gw.invoke_plugin("目标", {"k":"v"}, current_envelope=env, timeout_ms=30_000)`；`hubkit.InvokeRejected`/`hubkit.InvokeFailed`；`client.state.publish("flow名", env)` 返回 `PublishReceipt`；`hubkit.new_ulid()`。
- **Node**：`new GatewayClient(client, onDenied, callTimeoutMs)` + `setToken(token)`；`gw.invokePlugin("目标", { payloadJSON: {...}, currentEnvelope: env, timeoutMs: 30000 })`；`newEnvelope()`/`newULID()`。
- **Rust**：`Plugin` trait `set_gateway(&self, gateway: Arc<hubkit::GatewayClient>)`；`gw.invoke_plugin("目标", serde_json::json!({...}), InvokeOptions{ version: None, timeout_ms: Some(30_000), current_envelope: Some(&env) })` 返回 `Result<Envelope, GatewayError>`。
- **C#**：实现 `IGatewayAware.SetGateway(GatewayClient)`；`await gateway.InvokePluginAsync("目标", new InvokeOptions{...})`；`Envelopes.NewEnvelope()`/`Envelopes.NewUlid()`；异常 `InvokeFailureException` 及子类。

### 3.2 README.md.tmpl 追加小节

五门 README 末尾加「## 插件互调与发现（M6）」小节：三五行讲清 GatewayClient/发现三件套/publish 入口、invokes 声明、指向 `docs/plugin-onboarding.md` 与 `examples/ping-chain`。**同时定义 marker**（升级脚本与模板统一用）：

```
<!-- m6-intercall-start -->
…小节内容…
<!-- m6-intercall-end -->
```

模板里的 README 也带 marker（保证「新项目」与「升级后项目」形状一致）。

### 3.3 同步断言清单（漏了 CI 必挂）

- `scripts/smoke-template.sh` 的 `expect_files`（:145-157）每门加各自新文件；
- go `sdk/go/cmd/hub-plugin/main_test.go` 的 `Test生成完整的插件工程` 文件清单；
- python `sdk/python/tests/test_scaffold.py` 清单断言；
- node `sdk/node/test/scaffold.test.ts` 清单断言；
- rust：若有产物清单断言同步（`sdk/rust` 内 grep）；csharp 无本地脚手架测试（smoke 覆盖）。

## 4. 升级补丁包（存量项目）

### 4.1 素材目录与渲染

每门新增素材目录 `sdk/<lang>/templates/upgrade/`（**go 在 `sdk/go/cmd/hub-plugin/templates/upgrade/`**），三件套（`.tmpl` 结尾、走 @@ 渲染）：

| 文件 | 内容 |
|---|---|
| `gateway_example.<ext>.tmpl` | 与 §3.1 同源同内容（升级后项目与新生成项目形状一致） |
| `upgrade.sh.tmpl` | 幂等升级脚本（见 §4.3） |
| `UPGRADE.md.tmpl` | 升级说明 + 检查清单（用 @@name@@ 指代项目） |

zip 内部结构：顶层 `{name}/`（与模板 zip 一致），内含上述三件；`download_filename` = `plugin-hub-{lang}-upgrade-{name}.zip`。

### 4.2 hub-templates 机制扩展

- `build.rs`：新增 `UPGRADE_SOURCES` 扫描——对每门语言扫 `templates/upgrade/`（go 特殊目录），**存在则生成 `Some(&[(路径, include_bytes!)])`，缺失生成 `None`**（允许并行开发期部分语言缺素材；最终五门必须齐）。生成代码形如：
  `pub static UPGRADE_SOURCES: &[(&str, Option<&[(&str, &[u8])]>)] = &[("go", Some(&[...])), ("rust", None), ...];`
- `lib.rs`：
  - `pub fn upgrade_files(lang_id: &str) -> Result<&'static [(&'static str, &'static [u8])], TemplatesError>`（None/未知语言 → 错误，错误信息点名哪门素材缺失）；
  - `pub fn build_upgrade_zip(lang: &Language, values: &Values) -> Result<Vec<u8>, PackError>`：素材逐文件 render 后写 `{name}/{output}`，Stored 压缩，与 `build_zip` 同款；
  - `pub fn upgrade_download_filename(lang, plugin_name) -> String`。
  - 单测：build_upgrade_zip 可解压、条目=3、无残留 @@、五门素材齐全（最终态断言，素材未齐前先写 or 逻辑）。
- `bin/hub-scaffold.rs`：新增 `upgrade` 子命令——`hub-scaffold upgrade <语言> --dir <目标项目根>`：
  - `--dir` 必须已存在（存量项目）；解 build_upgrade_zip 产物，把 `{name}/` 下文件按相对路径落盘：**已存在 → 跳过并打印 `SKIP <文件>`；新 → 写入并打印 `ADD <文件>`**；结束打印检查清单摘要（指向 UPGRADE.md）。全部 SKIP 时退出码 0，打印「已是最新，无需变更」。
  - 不改原 `new` 子命令行为。

### 4.3 upgrade.sh 脚本行为（幂等，bash）

1. 前置：`git status --porcelain` 非空时警告并要求确认（`--force` 跳过）；定位插件项目根（脚本所在目录）。
2. `ADD`：gateway_example 文件复制到项目规范位置（go/python/csharp 根、node `src/`、rust `examples/`）——不存在才复制。
3. `README`：无 `m6-intercall-start` marker 则把小节追加到 README.md 末尾（README 不存在则创建带 marker 的新文件）。
4. Go 门专属：检查 `vendor/…/hubkit/gateway.go` 是否存在，缺失则提示「需要 `go mod vendor` 同步 SDK 副本（check-go-vendor.sh 会拦 vendor 漂移）」；检查 `go.mod` 的 replace 指向。
5. Python 门专属：提示「hubkit 由镜像构建 COPY 自动升级，确认基础镜像为准」+ 本地如 hubkit 副本存在则 grep gateway.py 提示。
6. 结尾打印检查清单：构建/测试命令、manifest invokes 声明指引、配额与防环行为提醒（每插件每分钟 60 次、链深 8）。
7. 脚本自身被渲染时 @@name@@ 等已替换；**脚本内不得出现 @@ 占位符以外的模板语法**；脚本要有 `set -euo pipefail`。

### 4.4 HTTP 端点

`crates/hub-api/src/templates.rs` 新增 `pub async fn upgrade_package(State, Path<String>, Query<DownloadQuery>) -> Response`：
- 照 `download`（:114）先例：语言未知 → 404 列已知语言；插件名用 `hub_registry::validate::is_valid_plugin_name` 校验 → 400；
- values 装配与 `download` 同款（复用现有 values_for 逻辑）；
- 返回 `application/zip` + `Content-Disposition: attachment; filename="plugin-hub-{lang}-upgrade-{name}.zip"`；
- `TemplateInfo` 加 `upgrade_size_bytes: u64`（list 响应多一个字段，前端 JS 无类型约束不受影响）；
- `src/templates.rs` 单测加：upgrade_package 成功返回 zip/文件名正确/未知语言 404/名字非法 400（authz 无需动，前缀已覆盖）。

### 4.5 smoke-template.sh 扩展（升级链路的端到端回归）

在现有流程后加「升级演练」段（每门）：
1. 用该门已有渲染产物目录（新插件工程）执行 `hub-scaffold upgrade <lang> --dir <产物目录>`；
2. 断言：gateway_example 落到规范位置、README 含 `m6-intercall-start`、二次执行全 SKIP（幂等）、脚本与新文件无 `@@` 残留；
3. 受影响门的构建再过一遍（go build、python compileall、node tsc、rust cargo check——按现有门的构建命令，确保「升级后可编译」；csharp dotnet build）。
`expect_files` 同步 §3.3。

## 5. 前端按钮（anc 仓库）

- `src/api/hub.js`：`export async function downloadUpgradePackage(lang, name)` —— 照 `downloadPluginTemplate`（:648）同款 blob 处理（rawResponse/Content-Disposition/rethrowBlobError），GET `/plugin-templates/${lang}/upgrade-package?name=...`。
- `src/views/hub/onboarding/index.vue`：模板卡片/弹窗加「下载 M6 升级包」按钮（复用 saveBlob 与错误提示先例；404 时提示「这个中台还没有升级包接口，请升级中台后使用」——照 loadAll 的 404 兼容先例）。
- 不动列表结构；不改无关页面。

## 6. 验收门（确定性执行）

1. `cargo test -p hub-templates`（含新单测）；
2. `cargo test -p hub-api --lib`（templates.rs 单测；hub-api 集成测试不在本任务范围跑全量）；
3. `bash scripts/smoke-template.sh`（五门渲染+断言+构建+升级演练；本机 go/python/node/cargo/dotnet 齐）；
4. 前端 `pnpm build`；
5. `git diff --stat Cargo.lock` 必须为空（零新依赖铁律）。
