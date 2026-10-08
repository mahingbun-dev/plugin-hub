# anc-hub 插件 SDK（Python）

用 Python 写 anc-hub 插件的工具箱。与 [`sdk/go`](../go) 是同一套契约的两门语言实现：

| 件 | 位置 | 作用 |
|---|---|---|
| ① 服务端骨架 | `hubkit/` | 实现四个方法就有个能跑的插件：gRPC 服务、自注册、心跳、被摘除后自愈、优雅退出全由它处理 |
| ② 契约定义 | `hubkit/proto/hubv1/` | 由 `../../crates/hub-proto` 的 proto 生成并提交，**不需要装 protoc** |
| ③ 脚手架 | `scaffold.py` | `scaffold.py new <name>` 生成可运行的插件工程 |
| ④ mock 中台 | `hubkit/mockhub/` | 本地 mock，离线开发与自动化测试用 |
| ⑤ 一致性套件 | `hubkit/conformance/` | 插件接入前必须跑通的检查，跑的就是中台注册期的规则 |
| ⑥ 调试工具 | `python -m hubkit` | 直连插件调试（Python 侧的 `hubprobe`） |

> 与 Go 的一处结构差异：`conformance` 与 `mockhub` **嵌在 `hubkit` 包内**，而不是像 Go
> 那样与 `hubkit` 平级。Python 的顶层包名是全局命名空间，`import conformance` 这种名字
> 撞车的表现是「import 到别人的东西」，而 Go 的 module 路径天然带前缀。
> 于是 `@@sdk_module@@` 在 Python 侧只有一个值：`hubkit`。

## 快速开始

```bash
# 生成一个可运行的插件工程
python3 scaffold.py new order-reader --dir /tmp/order-reader

cd /tmp/order-reader
pip install -r requirements.txt   # 内含 -e ./sdk，第三方包走清华镜像
pip install pytest
python -m pytest -q               # 含契约一致性检查
```

生成的插件**零 proto 工具链依赖**：它用 `google.protobuf.Struct` 承载 JSON 载荷，
也就是「直接调用」那条路（agent 经 MCP、外部系统经 HTTP）。需要与其他插件在 flow 里
传递类型化消息时，再加自己的 `.proto`。

## 依赖与镜像源

第三方依赖只有两个：`grpcio` 与 `protobuf`。它们在 `pyproject.toml` 里声明，
装 SDK 的时候一并解析：

```bash
pip install -i https://pypi.tuna.tsinghua.edu.cn/simple -e .
```

> **版本下限是钉死的。** `hubkit/proto/hubv1/` 下的生成代码带两条运行时校验
> （protobuf 的 `ValidateProtobufRuntimeVersion` 与 grpcio 的 `GRPC_GENERATED_VERSION`），
> 装上更旧的版本会在 **import 期**直接抛错。重新生成 proto 之后要同步这两个下限。

## 最小插件

```python
import hubkit
from hubkit.proto.hubv1 import envelope_pb2, plugin_pb2


class MyPlugin(hubkit.Plugin):
    def manifest(self) -> plugin_pb2.PluginManifest:
        return plugin_pb2.PluginManifest(
            name="order-reader",
            version="1.0.0",
            consumes=[plugin_pb2.MessageContract(fq_name=hubkit.STRUCT_FQ_NAME)],
        )

    # descriptor()：只用 Struct 载荷的插件没有自己的 proto，缺省实现返回空即可

    def validate(self, ctx, env: envelope_pb2.Envelope) -> plugin_pb2.ValidateResponse:
        payload, is_json = hubkit.payload_json(env)
        if not is_json:
            return hubkit.invalid(hubkit.issue("payload", "需要 JSON 对象载荷"))
        if not isinstance(payload.get("orderId"), str):
            return hubkit.invalid(hubkit.issue("payload.orderId", "缺少必填字段 orderId"))
        return hubkit.valid()

    def handle(self, ctx, env: envelope_pb2.Envelope) -> envelope_pb2.Envelope:
        payload, _ = hubkit.payload_json(env)
        return hubkit.with_payload_json(env, {"orderId": (payload or {}).get("orderId")})


if __name__ == "__main__":
    hubkit.run(MyPlugin(), hubkit.config_from_env())
```

与 Go 侧的命名对应关系（读接入指南时对得上）：

| Go | Python |
|---|---|
| `Manifest()` / `Descriptor()` / `Validate()` / `Handle()` | `manifest()` / `descriptor()` / `validate()` / `handle()` |
| `hubkit.Run` / `hubkit.RunContext` | `hubkit.run` / `hubkit.serve` |
| `hubkit.PayloadJSON` / `WithPayloadJSON` | `hubkit.payload_json` / `hubkit.with_payload_json` |
| `hubkit.Valid` / `Invalid` | `hubkit.valid` / `hubkit.invalid` |
| `hubkit.StateAware`（接口） | `Plugin.set_state()`（缺省空实现，想用才覆写） |
| `hubkit.StateClient` / `StateEntry` | `hubkit.StateClient` / `hubkit.StateEntry` |

三处与 Go 同名但形状不同的地方：

- `validate(ctx, env)` / `handle(ctx, env)` 的 `ctx` 是 gRPC 的 `ServicerContext`
  （Go 侧是 `context.Context`）。测试里直接调这两个方法时传 `None` 即可。
- `payload_json` 返回 **`(载荷, 是否 JSON 载荷)`**，与 Go 的 `(map[string]any, bool)`
  一致。第二个值回答的是「载荷是什么类型」，不是「有没有内容」——载荷是空 JSON
  对象时得到 `({}, True)`，flow 内部传的业务类型才是 `(None, False)`。判据要用它，
  别用 `载荷 == {}`，否则「这次真的没有输入」会被当成「载荷不是 JSON，去按业务类型解」。
- `health()` 是**可选覆写**：不定义就恒报健康（骨架根本不问插件），定义了才调。
  默认不调是因为中台在**注册期**就探测它，返回 `healthy=False` 会让注册被拒——
  「启动瞬间依赖没就绪」于是变成「插件注册不上」。要用就得自己扛住这条，
  并返回 `plugin_pb2.HealthResponse`。

## 外置状态（HubState）

插件被**强制无状态**：实例内存不保证跨调用保留。要跨调用记住的东西（登录缓存、游标、
去重表……）都走中台的 HubState，别放进程内存——否则水平扩展与热切换立刻出问题。

**凭证由骨架管，插件作者不碰 token**。注册成功后 `run` 会把状态客户端交给插件：覆写
`set_state` 收下它即可（这是 Go 侧 `hubkit.StateAware` 在 Python 里的对应形态，不覆写
就等于老插件，一行不用改）。

```python
import threading

import hubkit
from hubkit.proto.hubv1 import envelope_pb2, plugin_pb2


class MyPlugin(hubkit.Plugin):
    def __init__(self):
        self._state = None
        self._lock = threading.Lock()  # 注入在注册线程上，handler 在 gRPC 工作线程上

    def manifest(self):
        return plugin_pb2.PluginManifest(name="order-reader", version="1.0.0")

    def set_state(self, state):
        """注册成功后（以及每次重新注册后）由骨架调用。"""
        with self._lock:
            self._state = state

    def handle(self, ctx, env):
        with self._lock:
            state = self._state
        if state is None:
            return env  # 还没注册上：按 fail-open 处理，别让插件在这里卡死
        state.put("orders", "SO-1", b"payload", ttl_seconds=300)
        return env
```

```python
state.get("orders", "SO-1")                  # → bytes | None（None = 键不存在）
state.put("orders", "SO-1", b"payload", ttl_seconds=0)   # 0 = 不过期
state.delete("orders", "SO-1")               # → True/False，删不存在的键不是错误
state.scan("orders", prefix="SO-", limit=100)  # → list[hubkit.StateEntry]
```

**Publish 已封装**（`state.publish(target, envelope)`）：中台侧的防环、链长上限 8、
每插件每分钟 60 次配额照常生效，并把 `subject` 无条件覆盖成调用插件的身份。被拒
（`receipt.accepted is False`）时原因在 `receipt.reason` 里——它是**返回值**而不是
异常：防环拒绝是调用方该分支处理的常规结果，不是故障。它是**异步**触发，拿不到下游
的处理结果；需要结果的同步互调用 `set_gateway` 注入的 `hubkit.GatewayClient`
（`invoke_plugin(plugin, payload, current_envelope=env)`，发现三件套也在它上面）。

### 要知道的几条

* **键名只允许 `[A-Za-z0-9_.-]`，非空，≤200 字节**（namespace / key / prefix 同一套规则）。
  客户端会**本地先拦一道**（`ValueError`，比中台那句 `INVALID_ARGUMENT` 好懂）；判定与
  中台一致，见 `hubkit.valid_state_segment`。放行 `*` 是漏洞不是功能——前缀靠字符串拼接。
* **前缀由中台拼**：中台按凭证反查插件名，键最终长成 `hub:state:{插件名}:{namespace}:{key}`。
  插件自报的 namespace 只是子空间，**不要自己再拼一遍**。
* **所有版本共用一个状态空间**（前缀里没有版本号）。升版本不会清空状态（对缓存正是要的），
  但两个版本往同一个 namespace 写就是**互相覆盖**。要按版本隔离，自己把版本写进 namespace。
* **上限**：单值 1MB、一次 Scan 最多 1000 条。超了在客户端就报错，不会产生往返。
* **TTL 的粒度是秒**，`0` 表示不过期。传不足 1 秒的值会**报错**：线协议会把它截断成 0，
  也就是永不过期——与「很快消失」正好相反，所以宁可吵一声。
* **单次调用有 2 秒上限**（`Config.state_call_timeout`，对应 Go 的 `StateCallTimeout`）。
  客户端先于中台放弃，插件才有机会走 fail-open。
* **状态调用抛 `grpc.RpcError`**（原样抛，可看 `err.code()`）；参数不合法抛 `ValueError`。
* **撞上 `UNAUTHENTICATED` 会自动重新注册换新凭证**（中台重启轮换凭证、实例被摘除后旧
  凭证失效）。这一次调用仍然是失败的——重注册是后台的补救，不该掩盖它。
* 隔离强度**不是承诺**：凭证只证明注册方自称是某个插件名。见 proto 文件头与 `docs/design.md`。

### 对着真中台验一遍

```bash
# 需要一个真中台（PG + Redis 齐全）；不设这个变量则整组跳过
HUB_INTEGRATION_ADDR=http://127.0.0.1:8093 python -m pytest tests/integration -q -s
```

它真注册一个叫 `py-state-it` 的插件，跑完主动注销，并断言注销后旧凭证立即失效。

## 环境变量

| 变量 | 必填 | 说明 |
|---|---|---|
| `HUB_ADDR` | 是 | 中台插件面地址（本地 `http://127.0.0.1:8093`） |
| `HUB_ADVERTISE_ADDR` | 是 | **中台能拨通**的本插件地址 |
| `HUB_LISTEN_ADDR` | 否 | 本插件监听地址，缺省 `:9000` |
| `HUB_INSTANCE_ID` | 否 | 实例标识，缺省「主机名-PID」 |
| `HUB_LOG_LEVEL` | 否 | `debug` / `info` / `warn` / `error` |

变量名与 Go 侧逐字一致——同一份部署清单、同一份接入指南要能套在两门语言上。

## 调试

```bash
# 直连插件（不需要中台）：验「插件自己做得对不对」
python -m hubkit health   http://127.0.0.1:9000
python -m hubkit describe http://127.0.0.1:9000
python -m hubkit validate http://127.0.0.1:9000 --payload '{"orderId":"SO-1"}'
python -m hubkit invoke   http://127.0.0.1:9000 --payload '{"orderId":"SO-1"}'
python -m hubkit conform  http://127.0.0.1:9000
```

`invoke` 走的是与中台一致的顺序（先校验、通过才进插件体），校验不通过会短路并返回退出码 1。
`conform` 把 `conformance.runtime` 的检查跑一遍并打印报告，有任何 `✗` 退出码 1。

**这些命令连的都是插件自己的地址**，证明不了中台能不能按你登记的 `HUB_ADVERTISE_ADDR`
拨到你——那是网络视角的事，只有把插件真注册进中台（L3）才答得了。

## 本地跑一个中台

```bash
# 中台仓库根目录
cargo run -p hub-mock
curl http://127.0.0.1:8092/health    # "name":"hub-mock" 才是在对 mock 验
curl http://127.0.0.1:8092/plugins   # 看在册的
```

8092 / 8093 被别的进程占着时，mock 支持换端口（记得插件的 `HUB_ADDR` 跟着改）：

```bash
HUB_MOCK_HTTP_ADDR=127.0.0.1:18092 HUB_MOCK_GRPC_ADDR=0.0.0.0:18093 cargo run -p hub-mock
```

## 开发这个 SDK 本身

```bash
# 1. 改了 proto 之后重新生成（需要 grpcio-tools，普通插件开发者不需要）
pip install grpcio-tools protobuf
./generate.sh

# 2. 测试
python -m pytest tests/ -q
```

`generate.sh` 会把 protoc 生成的 `from hub.v1 import ...` 重写成本包内的
`from hubkit.proto.hubv1 import ...`。**不改 proto 的 package**——package 是契约的一部分
（descriptor 里的消息全限定名就是 `hub.v1.Envelope`），改了它中台的兼容检查会把每个
字段都判成「包名变了」。

## 接入指南共通层

`templates/_shared/onboarding.md` 是各语言共用的「接入四道关」，渲染时被内联进
`AGENTS.md` 的 `@@onboarding@@` 处。**这里这份是 `sdk/go/cmd/hub-plugin/templates/_shared/`
的逐字节副本**——维护 SDK 时两边要一起改，否则两门语言的接入指南会开始各说各话。
中台侧（`crates/hub-templates`）统一渲染时应当只认一份（见文末「给中台打包器的一节」）。

## 给中台打包器的一节

中台侧渲染的是**同一批模板**，所以目录布局是约定而不是随便放的。Python 语言登记进
`crates/hub-templates/build.rs` 时要对齐的三件事：

**① 模板根是 `sdk/python/templates/plugin`，不是 `sdk/python/templates`。**

```
sdk/python/templates/
├── plugin/           ← LANGUAGES 指向这里，它的**内容**就是生成工程的根
│   ├── main.py.tmpl
│   ├── plugin.py.tmpl
│   └── ...
└── _shared/          ← 共通层素材，在模板根**之外**，所以不会被当成产物发出去
    └── onboarding.md
```

`_shared/` 刻意放在模板根之外：Go 那边把它放在模板目录里面、靠 `NON_PAYLOAD_DIRS`
排除，是因为 Go 的 `//go:embed` 不能向上取父目录、而 Rust 读任何路径都不受限。
Python 这边没有这个约束，放在外面就不必再维护一条排除规则——**少一条规则就少一处
可能对不上的地方**。

**② SDK 随包下发的排除项**（对比 `build.rs` 的 `SDK_SOURCES`）：

```rust
("python", "../../sdk/python", "sdk", "hubkit",
 &["tests", "templates", "scaffold.py", "__pycache__", ".pytest_cache", ".venv", "*.egg-info"])
```

⚠️ `*.egg-info` **必须排除**。`pip install -e .` 会在 SDK 根目录里建出 `hubkit.egg-info/`，
而它里面记的是**打包者机器上的路径**。带着它下发的工程装出来的元数据是错的，
表现还很难查（`pip show hubkit` 指着一个不存在的目录，而 import 却是好的）。
这一点与 Go 侧的 `vendor` 同类：都是「维护者本地跑过一次命令就会多出来的东西」。

**③ SDK 的引用路径字段填 `hubkit`**（Python 的包名），而 Go 那边填的是 module 路径。
`build.rs` 的 `SDK_SOURCES` 第三个字段就是为这件事留的。

