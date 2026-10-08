// Command hubprobe 是插件的调试工具，相当于我们这个契约的 grpcurl。
//
// 它直接连插件的 gRPC 地址，按中台的方式构造信封去调它——插件开发者不必把中台跑起来
// 就能验证自己的实现。
//
//	hubprobe health   <插件地址>
//	hubprobe describe <插件地址>
//	hubprobe validate <插件地址> --payload '{"text":"hi"}'
//	hubprobe invoke   <插件地址> --payload '{"text":"hi"}'
//	hubprobe conform  <插件地址>
//	hubprobe e2e      <中台地址> <插件名> --payload '{"text":"hi"}'
//
// 只有 e2e 打的是中台的 HTTP 面。这两组命令回答的是不同的问题，上线前都要过：
// 前五个验「插件自己做得对不对」，e2e 验「注册进来之后，中台按登记的地址真的拨得到它」。
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"strings"
	"time"

	"github.com/mahingbun-dev/plugin-hub/sdk/go/conformance"
	"github.com/mahingbun-dev/plugin-hub/sdk/go/hubkit"
	"github.com/mahingbun-dev/plugin-hub/sdk/go/mockhub"
	"github.com/mahingbun-dev/plugin-hub/sdk/go/proto/hubv1"
)

const (
	exitOK    = 0
	exitUsage = 2
	exitFail  = 1

	defaultTimeout = 30 * time.Second

	// e2e 下 --timeout 是发给中台的 timeout_ms，本进程的 context 要再宽这么一点——
	// 理由写在 run() 那条分支里。
	e2eClientGrace = 5 * time.Second
)

func main() {
	os.Exit(run(os.Args[1:]))
}

func run(args []string) int {
	if len(args) == 0 {
		usage()
		return exitUsage
	}

	switch args[0] {
	case "help", "-h", "--help":
		usage()
		return exitOK
	}

	command := args[0]
	opts, rest, err := parseArgs(args[1:])
	if err != nil {
		fmt.Fprintf(os.Stderr, "参数错误: %v\n\n", err)
		usage()
		return exitUsage
	}

	// e2e 打的是中台、还多一个「插件名」位置参数，校验与 deadline 都跟下面那条路不一样，
	// 单独分出去，免得为它把公共路径改得弯弯绕绕。
	if command == "e2e" {
		if len(rest) < 2 {
			fmt.Fprintln(os.Stderr, "e2e 需要两个参数：<中台地址> <插件名>")
			return exitUsage
		}
		// --timeout 在这里是发给中台的 timeout_ms，中台据此给插件定 deadline；给本进程
		// 再加一段宽限，否则中台还没到点回 502，我们自己先掐断了连接，用户看到的就成了
		// 一句「context deadline exceeded」——那恰好把中台想告诉他的原因盖掉了。
		ctx, cancel := context.WithTimeout(context.Background(), opts.timeout+e2eClientGrace)
		defer cancel()
		return runE2E(ctx, rest[0], rest[1], opts)
	}

	if len(rest) == 0 {
		fmt.Fprintln(os.Stderr, "缺少插件地址")
		return exitUsage
	}
	addr := rest[0]

	ctx, cancel := context.WithTimeout(context.Background(), opts.timeout)
	defer cancel()

	switch command {
	case "conform":
		return runConform(ctx, addr)
	case "health", "describe", "validate", "invoke":
		return runCall(ctx, command, addr, opts)
	default:
		fmt.Fprintf(os.Stderr, "未知命令 %q\n\n", command)
		usage()
		return exitUsage
	}
}

type options struct {
	payload   string
	messageID string
	timeout   time.Duration
	meta      map[string]string
}

func parseArgs(args []string) (options, []string, error) {
	opts := options{timeout: defaultTimeout, meta: map[string]string{}}
	var rest []string

	for i := 0; i < len(args); i++ {
		arg := args[i]
		key, value, hasValue := strings.Cut(arg, "=")
		takeValue := func() (string, error) {
			if hasValue {
				return value, nil
			}
			i++
			if i >= len(args) {
				return "", fmt.Errorf("%s 缺少取值", key)
			}
			return args[i], nil
		}

		switch key {
		case "--payload", "-payload":
			v, err := takeValue()
			if err != nil {
				return opts, nil, err
			}
			opts.payload = v
		case "--message-id", "-message-id":
			v, err := takeValue()
			if err != nil {
				return opts, nil, err
			}
			opts.messageID = v
		case "--timeout", "-timeout":
			v, err := takeValue()
			if err != nil {
				return opts, nil, err
			}
			d, err := time.ParseDuration(v)
			if err != nil {
				return opts, nil, fmt.Errorf("--timeout 取值 %q 不是合法时长（如 5s）", v)
			}
			opts.timeout = d
		case "--meta", "-meta":
			v, err := takeValue()
			if err != nil {
				return opts, nil, err
			}
			k, val, ok := strings.Cut(v, "=")
			if !ok {
				return opts, nil, fmt.Errorf("--meta 需要 k=v 形式，实际 %q", v)
			}
			opts.meta[k] = val
		default:
			if strings.HasPrefix(arg, "-") {
				return opts, nil, fmt.Errorf("未知选项 %s", arg)
			}
			rest = append(rest, arg)
		}
	}

	return opts, rest, nil
}

func usage() {
	fmt.Print(`hubprobe —— plugin-hub 插件调试工具

用法:
  hubprobe <命令> <插件地址> [选项]
  hubprobe e2e <中台地址> <插件名> [选项]

命令:
  health     探活
  describe   拉取 manifest
  validate   只跑校验器
  invoke     跑校验器 + 插件体（默认场景）
  conform    跑契约一致性检查
  e2e        经中台打一次完整链路（中台 → 插件）

前五个命令直连插件，验的是「插件自己做得对不对」；e2e 打中台的 HTTP 面，
验的是「注册进来之后，中台按登记的地址真的拨得到它」——上线前这两关都要过。

选项:
  --payload <json>     业务载荷（JSON 对象），缺省为空对象
  --message-id <id>    幂等键，缺省交给中台生成（直连插件时本地生成）
  --timeout <dur>      deadline 预算，如 5s（缺省 30s）；e2e 下它就是请求体的 timeout_ms
  --meta k=v           附加到信封 meta，可重复

示例:
  hubprobe health http://127.0.0.1:9000
  hubprobe invoke http://127.0.0.1:9000 --payload '{"text":"你好"}'
  hubprobe conform http://127.0.0.1:9000
  hubprobe e2e http://127.0.0.1:8092 auth --payload '{"token":"t"}'
  hubprobe e2e https://hub.example.com:8081/hub-api auth

e2e 的 <中台地址> 是中台的 HTTP 面：本地是 http://127.0.0.1:8092，UAT 是经 nginx 的
https://<域名>:8081/hub-api——两个环境各有各的地址，不能换着用。别把插件面 8093 填进来，
那是中台听插件的地方，不是这里。

（UAT 只给域名形式是刻意的：UAT 主机内部的监听端口是个「部署视角」的数字，在开发机上敲不到，
写成裸端口只会让人把它拼成 127.0.0.1:<端口> 再试一遍。缘由见 docs/plugin-onboarding.md 的「端口速查」。）
`)
}

func runConform(ctx context.Context, addr string) int {
	report := conformance.Runtime(ctx, addr)
	fmt.Println(report)
	if !report.Passed() {
		return exitFail
	}
	return exitOK
}

func runCall(ctx context.Context, command, addr string, opts options) int {
	client, err := mockhub.DialPlugin(addr)
	if err != nil {
		fmt.Fprintf(os.Stderr, "%v\n", err)
		return exitFail
	}
	defer client.Close()

	switch command {
	case "health":
		return printJSONOrFail(client.Health(ctx))
	case "describe":
		return printJSONOrFail(client.Describe(ctx))
	}

	env, err := buildEnvelope(opts)
	if err != nil {
		fmt.Fprintf(os.Stderr, "%v\n", err)
		return exitFail
	}

	if command == "validate" {
		resp, err := client.Validate(ctx, env)
		if err != nil {
			fmt.Fprintf(os.Stderr, "校验调用失败: %v\n", err)
			return exitFail
		}
		printJSON(resp)
		if !resp.GetValid() {
			// 校验不通过是明确的失败信号，退出码要能反映出来
			return exitFail
		}
		return exitOK
	}

	// invoke：中台的顺序是先校验、通过才进插件体，这里保持一致
	resp, err := client.Validate(ctx, env)
	if err != nil {
		fmt.Fprintf(os.Stderr, "校验调用失败: %v\n", err)
		return exitFail
	}
	if !resp.GetValid() {
		fmt.Fprintln(os.Stderr, "校验未通过，链路在此短路（中台不会调用插件体）：")
		printJSON(resp)
		return exitFail
	}

	out, err := client.Handle(ctx, env)
	if err != nil {
		fmt.Fprintf(os.Stderr, "插件体执行失败: %v\n", err)
		return exitFail
	}

	if payload, ok := hubkit.PayloadJSON(out); ok {
		fmt.Println("== 输出载荷（JSON）==")
		printJSON(payload)
		return exitOK
	}

	fmt.Println("== 输出载荷（非 JSON，给出原始描述）==")
	printJSON(map[string]any{
		"type_url":      out.GetPayload().GetTypeUrl(),
		"value_bytes":   len(out.GetPayload().GetValue()),
		"message_id":    out.GetMessageId(),
		"trace_id":      out.GetTraceId(),
		"envelope_meta": out.GetMeta(),
	})
	return exitOK
}

// ingressRequest 是 POST /ingress/{plugin} 的请求体，字段名以主 README 的「HTTP 接口」
// 一节为准——写错一个字段名中台不会报错，只会当没传，缺省值悄悄生效。
//
// message_id 与 meta 是 omitempty：中台会给缺省的 message_id 生成 ULID，我们替它编一个
// 没有好处（本地生成的毫秒时间戳还当不了稳定的幂等键）；没传 --meta 时也不该凭空多一个
// 字段出去。payload 相反，中台那边是必填，永远要发。
type ingressRequest struct {
	Payload   map[string]any    `json:"payload"`
	MessageID string            `json:"message_id,omitempty"`
	Meta      map[string]string `json:"meta,omitempty"`
	TimeoutMS int64             `json:"timeout_ms"`
}

// runE2E 走的是中台 → 插件的完整真实链路，而不是像别的子命令那样直连插件。
//
// 这是上线前唯一能验到「注册进来的地址中台真的拨得到」的一步：直连插件通了只说明插件
// 自己没问题，说明不了中台能不能按它登记的地址找到它——两件事在网络上是两回事。
func runE2E(ctx context.Context, base, plugin string, opts options) int {
	if opts.timeout <= 0 {
		// 中台的 resolve_timeout 会以 400 拒掉非正数；在这里拦下能省掉一轮往返，
		// 而且能说清是哪个参数的问题
		fmt.Fprintf(os.Stderr, "--timeout 必须为正数（它要当 timeout_ms 发给中台），实际 %v\n", opts.timeout)
		return exitUsage
	}

	payload, err := parsePayload(opts)
	if err != nil {
		fmt.Fprintf(os.Stderr, "%v\n", err)
		return exitFail
	}

	body, err := json.Marshal(ingressRequest{
		Payload:   payload,
		MessageID: opts.messageID,
		Meta:      opts.meta,
		TimeoutMS: opts.timeout.Milliseconds(),
	})
	if err != nil {
		fmt.Fprintf(os.Stderr, "请求体序列化失败: %v\n", err)
		return exitFail
	}

	// 基础地址当**前缀**用，不猜它是不是已经带了 /ingress：直连 http://127.0.0.1:8092 与
	// 经 nginx 的 https://.../hub-api 两种写法因此都不必额外配置。末尾的斜杠要去干净，
	// 否则拼出来是 /hub-api//ingress/auth，中台那边就是一个跟拼写无关的 404。
	endpoint := strings.TrimRight(base, "/") + "/ingress/" + url.PathEscape(plugin)

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, endpoint, bytes.NewReader(body))
	if err != nil {
		fmt.Fprintf(os.Stderr, "中台地址 %q 拼不出合法 URL: %v\n", base, err)
		return exitUsage
	}
	req.Header.Set("Content-Type", "application/json")

	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		fmt.Fprintln(os.Stderr, explainTransportError(base, endpoint, err))
		return exitFail
	}
	defer resp.Body.Close()

	raw, err := io.ReadAll(resp.Body)
	if err != nil {
		fmt.Fprintf(os.Stderr, "读中台响应失败（连接可能中途断了）: %v\n", err)
		return exitFail
	}

	// 人话走 stderr、响应体走 stdout，这样 `... | jq` 拿到的还是干净的一份 JSON。
	//
	// 「不是 JSON」这句要排在状态码解释前面：它是解释的前提——nginx 的错误页也长着 502，
	// 先亮明这份响应可能不出自中台，下面的状态码语义才不会被当成定论。
	decoded, shape := classifyBody(raw)
	if shape == bodyNotJSON {
		fmt.Fprintln(os.Stderr, "响应不是 JSON，可能是 nginx 或中间层返回的（下面原样打印）：")
	}
	fmt.Fprintln(os.Stderr, explainIngress(resp.StatusCode, plugin, shape))

	if shape == bodyNotJSON {
		// nginx 的错误页、413 的纯文本都落在这里。原样打出来——真正有用的信息就在这份
		// 非 JSON 里，被一句「解析失败」盖掉就白跑了这一趟。
		fmt.Println(string(raw))
	} else {
		printJSON(decoded)
	}

	// 只有「200、响应体是 JSON 对象、且带 plugin 字段」才算通过。假绿比误报贵得多：这个命令
	// 是验收工具，脚本里 `hubprobe e2e ... && 下一步` 一旦被中间层的 200 错误页骗过去，代价
	// 是一次错误的部署；而中台的 200 按 IngressResponse 的定义必然带 plugin，不带本身就说
	// 明这份响应不出自中台。
	//
	// 三级收紧各有各的对手：非 JSON 挡的是 nginx 的 HTML 页；非对象挡的是中间层随手回的
	// `null` / `[]` / `"x"`（它们过得了 Unmarshal，却一个字段都取不到）；而 `{"error":...}`
	// 是货真价实的 JSON 对象——中台自己的 404 就长这样，只判「是对象」等于给「把错误响应
	// 用 200 转发」留了条路。判到「有 plugin」为止：再往字段取值上收紧，误伤的就会是正常
	// 响应，而这个工具的信用比多挡一个边角场景值钱。
	if resp.StatusCode == http.StatusOK && shape == bodyObject {
		return exitOK
	}
	return exitFail
}

// ingressSuccessMarker 是「这份 200 确实出自中台」的最小物证。
//
// 只挑一个字段、且只要求它是个非空字符串（不比对取值）：中台的 200 成功响应（crates 里
// hub-api 的 IngressResponse）必然带 message_id / trace_id / plugin / version /
// instance_id，挑 plugin 是因为它语义最直白——响应里说得出是哪个插件，就说明中台真的
// 定位并调用了那个插件。要求非空而不是仅要求「键在」，是因为 plugin 为 null 或空串同样
// 说不出「中台调了谁」；要求多个字段则只会平添误伤。
const ingressSuccessMarker = "plugin"

// bodyShape 是响应体的形状。200 那一档光看状态码不足以说明这份响应出自中台，
// 得连形状一起看；而「不是 JSON」「是 JSON 但不是对象」「是对象但没有中台的物证字段」
// 三者的成因和修法都不一样，所以分开列，让 explainIngress 能各说各的。
type bodyShape int

const (
	bodyNotJSON         bodyShape = iota // 压根解析不出 JSON：多半是 nginx 或中间层自己的页面
	bodyNonObject                        // 是合法 JSON，但不是对象——null / [] / "x" 都算
	bodyObjectNoMarker                   // 是 JSON 对象，但没有 ingressSuccessMarker 这个字段
	bodyObject                           // JSON 对象且带那个字段：中台 200 成功响应的形状
)

func classifyBody(raw []byte) (any, bodyShape) {
	var decoded any
	if err := json.Unmarshal(raw, &decoded); err != nil {
		return nil, bodyNotJSON
	}
	obj, ok := decoded.(map[string]any)
	if !ok {
		return decoded, bodyNonObject
	}
	if name, ok := obj[ingressSuccessMarker].(string); !ok || name == "" {
		return decoded, bodyObjectNoMarker
	}
	return decoded, bodyObject
}

// explainIngress 把中台的状态码翻成「发生了什么 + 下一步做什么」。
//
// 中台的错误体（error / message / issues）已经很具体，这里补的是它没说出口的那半句：
// 这个状态码该由谁去修、修完拿什么复验——裸状态码逼人猜，猜错方向最费时间。
// 措辞按 hub-registry 那边「一句话说清 + 一句请柬」的写法来，不堆术语。
// bodyShape 是「响应体解析成了什么」，只有 200 那一档用得上：状态码本身不足以说明
// 这份响应出自中台，两件事得一起看。
// 各状态码的含义以主 README 的「HTTP 接口」一节为准。
func explainIngress(status int, plugin string, shape bodyShape) string {
	switch status {
	case http.StatusOK:
		switch shape {
		case bodyObject:
			return fmt.Sprintf("端到端跑通了：中台 → 插件 %s 的完整链路走通，下面是中台的原样响应。", plugin)
		case bodyObjectNoMarker:
			// 这一档以前是放行的：200 加任意 JSON 对象就算通过。可中台自己的错误体
			//（{"error":"not_found","message":...}）正是个 JSON 对象，中间层把错误响应配成
			// 200 转发过来，验收脚本就被骗过去了。措辞要说清「少了什么物证」，而不是笼统地
			// 说「格式不对」——用户手上那份响应到底缺哪一块，只有点名了才对得上号。
			return fmt.Sprintf("状态码是 200，响应也是 JSON 对象，但里面没有 %q 字段（或它是空的）——中台的 200 成功响应必然带 plugin / version / instance_id 这些字段，所以这份响应大概率不出自中台（中间层把一份错误响应配成 200 转发，就会长这样），不能算验收通过。", ingressSuccessMarker)
		case bodyNonObject:
			// 这一档以前漏在「不是 JSON」之外：null / [] / "x" 都解析成功，说「不是 JSON」是错的，
			// 而说「跑通」又放过了假绿。措辞据实说「是 JSON 但不是对象」，再点出对象里该有什么，
			// 用户才分得清自己是拿到了中台的响应、还是撞上了中间层。
			return "状态码是 200，响应也是合法 JSON，但它不是一个 JSON 对象——中台的 200 成功响应按定义必然是一个带 plugin / version / instance_id 等字段的对象，所以这份响应大概率不出自中台，不能算验收通过。"
		default:
			// 中台的 200 按定义必然带 JSON 体，这一条不成立，「链路走通」就无从谈起；
			// 上面那句「响应不是 JSON」会紧挨着它出现，两句连起来读就是完整结论
			return "状态码是 200，但响应不是 JSON——中台的 200 按定义必然带 JSON 体，所以这份响应大概率不出自中台，不能算验收通过。"
		}
	case http.StatusUnprocessableEntity:
		return `校验器拒绝了这份载荷（422）——是数据的问题，原样重试没有意义。
请按下面 issues 里的 path 改 --payload 再跑；想少绕一圈就直连插件调校验器：hubprobe validate <插件地址>`
	case http.StatusNotFound:
		return fmt.Sprintf(`中台不认识插件「%s」（404）——它还没注册上来，或者名字对不上。
请开 <中台地址>/admin/plugins 看已注册的名字（注意大小写与全名）；名字对得上就去查插件启动日志里注册那一步报了什么。`, plugin)
	case http.StatusBadGateway:
		// 中台把**两种成因**合用一个 code（crates/hub-api/src/error.rs 的 Upstream）：
		// 压根没有可用实例，与实例在册却没调通。后者还细分连不上 / 超时 / 没回信封三种，
		// 都在同一个 Upstream 里。从 error 字段分不开，只能读到 message 文本——但按文案做
		// 字符串分支，中台一改口径这里就静默走错路，比笼统更糟。所以成因都写出来，并给一个
		// 用户自己就能分清的动作：instance_count 是 0 还是不是 0。
		return fmt.Sprintf(`中台没能替你把这趟调用送到插件（502）——两种成因，先分清是哪一种。
请看 <中台地址>/admin/plugins 里「%s」的 instance_count：
  - 是 0：从没注册成功，或者实例全掉线了——这时跟地址、网络都无关，去查插件启动日志里注册那一步（L3）报了什么。
  - 不是 0：实例在册却没调通（连不上、超时、或者没按契约回信封，中台的 message 会说是哪种）——这时才轮到中台到插件这一段：先 hubprobe health <插件地址> 直连确认它还活着，直连通而经中台不通，说明它登记给中台的地址中台拨不到（容器网络不通、端口写错、或者只监听了 127.0.0.1）。`, plugin)
	case http.StatusRequestEntityTooLarge:
		// 文案里的两个数字取自 crates/hub-api/src/payload.rs：MAX_INLINE_BYTES（4MB，
		// 内联上限，超了就改走 /blobs 引用通道）与 MAX_REQUEST_BYTES（8MB，请求体硬上限，
		// 超了在进 handler 之前就被 DefaultBodyLimit 挡成 413，所以这一档拿不到中台
		// 错误体里的 message，只有纯文本）。上限定额调整时，这句要跟着改。
		return `请求体超过中台硬上限（413，8MB）——它在进插件之前就被挡下了，插件根本没收到这次调用。
请把载荷减到 4MB 以内走内联；再大就得用 /blobs 引用通道，或者把这段处理编排成异步 flow。`
	case http.StatusTooManyRequests, http.StatusServiceUnavailable:
		return fmt.Sprintf(`被实例级治理拦下了（%d）——不是这份数据的错，是那个实例现在不该接活（熔断，或并发到顶且排队超时）。
请稍后重试；一直如此就看 <中台地址>/admin/governance 里它的失败计数与熔断状态。`, status)
	default:
		return fmt.Sprintf(`中台返回了预期外的状态码 %d。
请先确认这个响应确实来自中台——地址前缀拼错时 nginx 常拿自己的错误页回你；如果中台版本比这个命令新，也可能是它新加的语义。`, status)
	}
}

// explainTransportError 处理连状态码都没有的失败。这里最费时间的恰恰是分不清
// 「中台没起来」「地址写错」「走 nginx 前缀不对」这三种，所以措辞要点到怎么区分。
//
// base 是用户**填进来的**中台地址，endpoint 是拼上 /ingress/<插件名> 之后真正去调的地址。
// 两个都要，因为用途不同：前三条讲的是「地址该怎么写」，说的都是 base；而末尾那个自证动作
// 只有拿 base 去敲才成立——拿 endpoint 接 /health 拼出来的是 /ingress/<插件名>/health，
// 那个路径根本不存在，回一个 404 反倒把人送去查「前缀不对」这个不相干的方向。
func explainTransportError(base, endpoint string, err error) string {
	if errors.Is(err, context.DeadlineExceeded) {
		return fmt.Sprintf(`等 %s 回应超时。
请调大 --timeout 重试。注意中台自己等插件超时会回 502（那时会走本命令的 502 分支）；连 502 都没等到，说明卡在中台内部或中间层，去中台日志里找这次调用。`, endpoint)
	}
	// 两个环境各有各的地址（本地 127.0.0.1:8092 / UAT 经 nginx 的域名地址），只给一个，
	// 另一个环境的人照着改就正好改错——而且这条提示往往是他手上唯一的线索，指错方向等于
	// 把人送去排查一个根本不存在的服务。
	//
	// UAT 这里**只给域名形式**：UAT 主机内部那个监听端口是部署视角的数字，写进来就会被
	// 读者拼成 127.0.0.1:<那个端口> 再试一遍——而他手上正是一条连不上的错误。
	//
	// 末尾的自证动作指向 base 而不是 endpoint，且把完整 URL 打出来：endpoint 已经带了
	// /ingress/<插件名>，在它后面接 /health 拼出来的是个不存在的路径，回一个 404——而文档里
	// 写着「404 或一段 HTML 是前缀不对」，照做的人会被引去查一个不存在的前缀问题。
	// 末尾斜杠去干净，否则拼出来是 //health（与 endpoint 的处理一致）。
	healthURL := strings.TrimRight(base, "/") + "/health"
	return fmt.Sprintf(`连不上 %s：%v
请按这三条逐一核对，通常是其中一条不对：
  1) 地址带了 http:// 或 https://；
  2) 地址是「你要连的那个环境」的中台 HTTP 面：本地是 http://127.0.0.1:8092，UAT 是经 nginx 的 https://<域名>:8081/hub-api——两者不能换着用（UAT 主机上中台自己听在另一个内部端口，那个数字只在 UAT 主机上有意义；换成插件面 8093 也一样连不上）；
  3) 走 nginx 时前缀别漏也别写错，UAT 形如 https://<域名>:8081/hub-api——前缀错了常被 nginx 用自己的页面接走。
还拿不准就自己验一下：探活不需要鉴权，直接 curl %s（= 你填进来的中台地址 + /health），返回 {"status":"ok",...} 说明地址对了，回不来就是地址不对。
注意要拿「你填进来的地址」去接，别在开头那条已经拼好 /ingress/<插件名> 的调用地址后面接 /health——那个路径不存在，回你一个 404，看着倒像是前缀写错了。`, endpoint, err, healthURL)
}

func buildEnvelope(opts options) (*hubv1.Envelope, error) {
	payload, err := parsePayload(opts)
	if err != nil {
		return nil, err
	}

	messageID := opts.messageID
	if messageID == "" {
		messageID = fmt.Sprintf("hubprobe-%d", time.Now().UnixMilli())
	}

	env, err := hubkit.WithPayloadJSON(&hubv1.Envelope{
		MessageId:  messageID,
		TraceId:    fmt.Sprintf("hubprobe-%d", time.Now().UnixNano()),
		DeadlineMs: time.Now().Add(opts.timeout).UnixMilli(),
		Meta:       opts.meta,
	}, payload)
	if err != nil {
		return nil, err
	}
	return env, nil
}

// parsePayload 把 --payload 解析成 JSON 对象。直连插件（信封里的 Struct）与经中台
// （请求体里的 payload 字段）要求是同一条，两处共用一个实现，免得报错口径跑偏。
func parsePayload(opts options) (map[string]any, error) {
	payload := map[string]any{}
	if strings.TrimSpace(opts.payload) == "" {
		return payload, nil
	}
	if err := json.Unmarshal([]byte(opts.payload), &payload); err != nil {
		return nil, fmt.Errorf("--payload 不是合法的 JSON 对象: %w", err)
	}
	return payload, nil
}

func printJSONOrFail[T any](value T, err error) int {
	if err != nil {
		fmt.Fprintf(os.Stderr, "调用失败: %v\n", err)
		return exitFail
	}
	printJSON(value)
	return exitOK
}

func printJSON(value any) {
	raw, err := json.MarshalIndent(value, "", "  ")
	if err != nil {
		fmt.Println(value)
		return
	}
	fmt.Println(string(raw))
}
