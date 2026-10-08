package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/mahingbun-dev/anc-hub/sdk/go/hubkit"
)

func Test解析常用选项(t *testing.T) {
	opts, rest, err := parseArgs([]string{
		"http://127.0.0.1:9000",
		"--payload", `{"text":"hi"}`,
		"--message-id", "m-1",
		"--timeout", "5s",
	})
	if err != nil {
		t.Fatalf("解析失败: %v", err)
	}
	if len(rest) != 1 || rest[0] != "http://127.0.0.1:9000" {
		t.Errorf("rest = %v", rest)
	}
	if opts.payload != `{"text":"hi"}` {
		t.Errorf("payload = %q", opts.payload)
	}
	if opts.messageID != "m-1" {
		t.Errorf("messageID = %q", opts.messageID)
	}
	if opts.timeout != 5*time.Second {
		t.Errorf("timeout = %v", opts.timeout)
	}
}

func Test选项可重复且支持等号形式(t *testing.T) {
	opts, _, err := parseArgs([]string{
		"http://127.0.0.1:9000",
		"--meta=tenant=t1",
		"--meta", "region=cn",
	})
	if err != nil {
		t.Fatalf("解析失败: %v", err)
	}
	if opts.meta["tenant"] != "t1" || opts.meta["region"] != "cn" {
		t.Errorf("meta = %v", opts.meta)
	}
}

func Test超时取值非法时报错(t *testing.T) {
	_, _, err := parseArgs([]string{"http://x", "--timeout", "五秒"})
	if err == nil {
		t.Fatal("非法时长应报错")
	}
	if !strings.Contains(err.Error(), "合法时长") {
		t.Errorf("错误信息应说清楚格式，实际 %v", err)
	}
}

func Test_meta_格式非法时报错(t *testing.T) {
	_, _, err := parseArgs([]string{"http://x", "--meta", "没有等号"})
	if err == nil {
		t.Fatal("meta 缺少 = 应报错")
	}
}

func Test未知选项报错(t *testing.T) {
	if _, _, err := parseArgs([]string{"http://x", "--nope"}); err == nil {
		t.Fatal("未知选项应报错而不是被忽略")
	}
}

func Test构造信封(t *testing.T) {
	env, err := buildEnvelope(options{
		payload:   `{"orderId":"SO-1","qty":2}`,
		messageID: "m-9",
		timeout:   5 * time.Second,
		meta:      map[string]string{"tenant": "t1"},
	})
	if err != nil {
		t.Fatalf("构造失败: %v", err)
	}

	if env.GetMessageId() != "m-9" {
		t.Errorf("message_id = %q", env.GetMessageId())
	}
	if env.GetMeta()["tenant"] != "t1" {
		t.Errorf("meta = %v", env.GetMeta())
	}

	budget, ok := hubkit.Budget(env)
	if !ok {
		t.Fatal("应设置 deadline")
	}
	if budget <= 4*time.Second || budget > 5*time.Second {
		t.Errorf("预算约 5 秒，实际 %v", budget)
	}

	payload, ok := hubkit.PayloadJSON(env)
	if !ok {
		t.Fatal("载荷应是 JSON")
	}
	if payload["orderId"] != "SO-1" {
		t.Errorf("orderId = %v", payload["orderId"])
	}
	// Struct 的数值一律是 float64，调试时看到 2 而不是 2 是正常的
	if payload["qty"] != float64(2) {
		t.Errorf("qty = %v（%T）", payload["qty"], payload["qty"])
	}
}

func Test未给载荷时构造空对象(t *testing.T) {
	env, err := buildEnvelope(options{timeout: time.Second, meta: map[string]string{}})
	if err != nil {
		t.Fatalf("构造失败: %v", err)
	}
	payload, ok := hubkit.PayloadJSON(env)
	if !ok {
		t.Fatal("应有 JSON 载荷")
	}
	if len(payload) != 0 {
		t.Errorf("应为空对象，实际 %v", payload)
	}
}

func Test载荷不是对象时报错(t *testing.T) {
	_, err := buildEnvelope(options{payload: `[1,2,3]`, timeout: time.Second, meta: map[string]string{}})
	if err == nil {
		t.Fatal("数组载荷应被拒绝")
	}
	if !strings.Contains(err.Error(), "JSON 对象") {
		t.Errorf("错误信息应说清要求，实际 %v", err)
	}

	if _, err := buildEnvelope(options{payload: `{不是 JSON}`, timeout: time.Second, meta: map[string]string{}}); err == nil {
		t.Fatal("非法 JSON 应被拒绝")
	}
}

func Test未给_message_id_时自动生成(t *testing.T) {
	env, err := buildEnvelope(options{timeout: time.Second, meta: map[string]string{}})
	if err != nil {
		t.Fatalf("构造失败: %v", err)
	}
	if !strings.HasPrefix(env.GetMessageId(), "hubprobe-") {
		t.Errorf("自动生成的 id = %q", env.GetMessageId())
	}
	if env.GetTraceId() == "" {
		t.Error("trace_id 不该为空——排障全靠它")
	}
}

// ---- e2e：经中台打完整链路 ----

// fakeHub 是一个假中台。它对 e2e 命令来说只要做两件事：把收到的请求记下来，
// 再按剧本回一个响应。真中台怎么实现这条链路不是这里要验的——e2e 要验的是我们
// 拼的 URL、发的请求体、以及按状态码分流的输出。
type fakeHub struct {
	mu   sync.Mutex
	last fakeHubRequest
}

type fakeHubRequest struct {
	path   string
	method string
	ctype  string
	body   map[string]any
}

func (h *fakeHub) recorded() fakeHubRequest {
	h.mu.Lock()
	defer h.mu.Unlock()
	return h.last
}

func newFakeHub(t *testing.T, status int, contentType, responseBody string) (*httptest.Server, *fakeHub) {
	t.Helper()
	hub := &fakeHub{}
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		raw, _ := io.ReadAll(r.Body)
		var parsed map[string]any
		_ = json.Unmarshal(raw, &parsed)

		hub.mu.Lock()
		hub.last = fakeHubRequest{
			path:   r.URL.Path,
			method: r.Method,
			ctype:  r.Header.Get("Content-Type"),
			body:   parsed,
		}
		hub.mu.Unlock()

		if contentType != "" {
			w.Header().Set("Content-Type", contentType)
		}
		w.WriteHeader(status)
		_, _ = io.WriteString(w, responseBody)
	}))
	t.Cleanup(srv.Close)
	return srv, hub
}

// captureOutput 把 os.Stdout / os.Stderr 换成管道。runE2E 直接往这两个句柄写，
// 测试里没有别的办法拿到它说了什么。两路合成一份：对读的人来说，「人话」与
// 「原样响应」本来就是同一条信息的两段。
func captureOutput(t *testing.T, fn func()) string {
	t.Helper()
	oldOut, oldErr := os.Stdout, os.Stderr
	r, w, err := os.Pipe()
	if err != nil {
		t.Fatalf("建管道失败: %v", err)
	}
	os.Stdout, os.Stderr = w, w

	done := make(chan string, 1)
	go func() {
		var buf bytes.Buffer
		_, _ = io.Copy(&buf, r)
		done <- buf.String()
	}()

	fn()
	_ = w.Close()
	os.Stdout, os.Stderr = oldOut, oldErr
	return <-done
}

func runE2ECapture(t *testing.T, base, plugin string, opts options) (int, string) {
	t.Helper()
	var code int
	out := captureOutput(t, func() {
		code = runE2E(context.Background(), base, plugin, opts)
	})
	return code, out
}

func e2eOptions(payload string, timeout time.Duration) options {
	return options{payload: payload, timeout: timeout, meta: map[string]string{}}
}

func Test_e2e_成功时请求体与路径都要对(t *testing.T) {
	srv, hub := newFakeHub(t, http.StatusOK, "application/json",
		`{"message_id":"m-1","trace_id":"t-1","plugin":"auth","version":"v1","instance_id":"auth-1","elapsed_ms":12,"payload":{"ok":true}}`)

	code, out := runE2ECapture(t, srv.URL, "auth", options{
		payload:   `{"token":"t"}`,
		messageID: "m-1",
		timeout:   5 * time.Second,
		meta:      map[string]string{},
	})

	if code != exitOK {
		t.Fatalf("退出码 = %d，期望 %d；输出：\n%s", code, exitOK, out)
	}

	got := hub.recorded()
	if got.method != http.MethodPost {
		t.Errorf("方法 = %q，期望 POST", got.method)
	}
	if got.path != "/ingress/auth" {
		t.Errorf("路径 = %q，期望 /ingress/auth", got.path)
	}
	if !strings.HasPrefix(got.ctype, "application/json") {
		t.Errorf("Content-Type = %q", got.ctype)
	}
	// 字段名错一个，中台不会报错，只会当没传、让缺省值悄悄生效，所以逐个断言
	if got.body["message_id"] != "m-1" {
		t.Errorf("message_id = %v", got.body["message_id"])
	}
	if got.body["timeout_ms"] != float64(5000) {
		t.Errorf("timeout_ms = %v（%T），期望 5000", got.body["timeout_ms"], got.body["timeout_ms"])
	}
	payload, ok := got.body["payload"].(map[string]any)
	if !ok {
		t.Fatalf("payload 应是 JSON 对象，实际 %v", got.body["payload"])
	}
	if payload["token"] != "t" {
		t.Errorf("payload.token = %v", payload["token"])
	}
	if _, exists := got.body["meta"]; exists {
		t.Errorf("没传 --meta 时不该出现 meta 字段：%v", got.body)
	}

	if !strings.Contains(out, "端到端跑通") {
		t.Errorf("成功要有一句人话说明，实际：\n%s", out)
	}
	// 响应原样给出来，instance_id 这种排障要用的字段不能被吞掉
	if !strings.Contains(out, "auth-1") || !strings.Contains(out, "elapsed_ms") {
		t.Errorf("应打印中台的原样响应，实际：\n%s", out)
	}
}

func Test_e2e_未给_message_id_时交给中台生成(t *testing.T) {
	srv, hub := newFakeHub(t, http.StatusOK, "application/json", `{"plugin":"auth","payload":{}}`)

	code, out := runE2ECapture(t, srv.URL, "auth", e2eOptions(`{}`, time.Second))
	if code != exitOK {
		t.Fatalf("退出码 = %d；输出：\n%s", code, out)
	}
	if _, exists := hub.recorded().body["message_id"]; exists {
		t.Errorf("没传 --message-id 时应让中台生成 ULID，实际发了 %v", hub.recorded().body["message_id"])
	}
}

func Test_e2e_地址当前缀拼且末尾斜杠要处理干净(t *testing.T) {
	cases := []struct {
		name   string
		suffix string
		want   string
	}{
		{"直连中台", "", "/ingress/auth"},
		{"直连中台带末尾斜杠", "/", "/ingress/auth"},
		{"经 nginx 带前缀", "/hub-api", "/hub-api/ingress/auth"},
		{"前缀末尾带斜杠", "/hub-api/", "/hub-api/ingress/auth"},
		{"前缀末尾两道斜杠", "/hub-api//", "/hub-api/ingress/auth"},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			srv, hub := newFakeHub(t, http.StatusOK, "application/json", `{"plugin":"auth"}`)
			base := srv.URL + tc.suffix

			code, out := runE2ECapture(t, base, "auth", e2eOptions(`{}`, time.Second))
			if code != exitOK {
				t.Fatalf("base=%s 退出码 = %d；输出：\n%s", base, code, out)
			}
			if got := hub.recorded().path; got != tc.want {
				t.Errorf("base=%s 路径 = %q，期望 %q", base, got, tc.want)
			}
		})
	}
}

func Test_e2e_按状态码分流成人话(t *testing.T) {
	cases := []struct {
		name     string
		status   int
		ctype    string
		body     string
		wantText []string
	}{
		{
			name:     "404 插件未注册",
			status:   http.StatusNotFound,
			ctype:    "application/json",
			body:     `{"error":"not_found","message":"插件 auth 未注册"}`,
			wantText: []string{"404", "不认识插件", "/admin/plugins", "插件 auth 未注册"},
		},
		{
			name:   "422 校验器拒绝",
			status: http.StatusUnprocessableEntity,
			ctype:  "application/json",
			body: `{"error":"validation_rejected","message":"插件 auth 的校验器拒绝了该数据",` +
				`"issues":[{"path":"$.token","message":"token 不能为空","severity":1}]}`,
			wantText: []string{"422", "校验器拒绝", "--payload", "hubprobe validate", "$.token"},
		},
		{
			// 只断言跟成因无关的部分；「两种成因都要覆盖」由下面那个专门的用例锁
			name:     "502 插件不可达",
			status:   http.StatusBadGateway,
			ctype:    "application/json",
			body:     `{"error":"plugin_unavailable","message":"该插件当前没有可用实例（全部掉线或尚未注册）","plugin":"auth"}`,
			wantText: []string{"502", "该插件当前没有可用实例"},
		},
		{
			// 413 是 DefaultBodyLimit 在进 handler 之前挡下的，回的是纯文本不是错误体，
			// 顺带把「非 JSON 也不能崩」这条也验了
			name:     "413 请求体超限",
			status:   http.StatusRequestEntityTooLarge,
			ctype:    "text/plain; charset=utf-8",
			body:     "length limit exceeded",
			wantText: []string{"413", "8MB", "length limit exceeded", "响应不是 JSON"},
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			srv, _ := newFakeHub(t, tc.status, tc.ctype, tc.body)

			code, out := runE2ECapture(t, srv.URL, "auth", e2eOptions(`{}`, time.Second))
			if code != exitFail {
				t.Errorf("退出码 = %d，期望 %d（非 200 一律 1，好塞进脚本）", code, exitFail)
			}
			for _, want := range tc.wantText {
				if !strings.Contains(out, want) {
					t.Errorf("输出里应有 %q，实际：\n%s", want, out)
				}
			}
		})
	}
}

// 中台的 502（error=plugin_unavailable）一个 code 盖着两种成因：压根没有可用实例，
// 与实例在册但中台拨不通。从 error 字段分不开（同一个 Upstream），而按 message 文本做
// 字符串分支又会在中台改口径时静默走错路——那就比笼统更糟。所以解释必须同时覆盖两种，
// 并给一个用户自己就能分清的动作。只写一种就会把人带偏：实例数为 0 时，让对方去查
// 「登记地址/容器网络」，查的是一个根本不存在的实例。
func Test_e2e_502_覆盖两种成因并给出区分办法(t *testing.T) {
	srv, _ := newFakeHub(t, http.StatusBadGateway, "application/json",
		`{"error":"plugin_unavailable","message":"该插件当前没有可用实例（全部掉线或尚未注册）","plugin":"auth"}`)

	code, out := runE2ECapture(t, srv.URL, "auth", e2eOptions(`{}`, time.Second))
	if code != exitFail {
		t.Errorf("退出码 = %d，期望 %d", code, exitFail)
	}

	// 用户用来分清是哪一种的那一步
	for _, want := range []string{"/admin/plugins", "instance_count"} {
		if !strings.Contains(out, want) {
			t.Errorf("要给出 %q 这条区分路径，实际：\n%s", want, out)
		}
	}
	// 成因一：没有可用实例——该查的是注册/启动，跟地址无关
	if !strings.Contains(out, "启动日志") {
		t.Errorf("要覆盖「没有可用实例」这一成因，并把下一步指向注册那一步，实际：\n%s", out)
	}
	// 成因二：实例在册却没调通——这才轮到直连确认与地址排查。
	// 连不上/超时/没回信封三种都落在同一个 Upstream 里，所以不能只说「拨不通」，
	// 否则超时的人会照着网络排查白跑
	for _, want := range []string{"hubprobe health", "中台拨不到", "没按契约回信封"} {
		if !strings.Contains(out, want) {
			t.Errorf("要覆盖「有实例却没调通」这一成因（含 %q），实际：\n%s", want, out)
		}
	}
	// 上一条只对成因二成立，不能拿来当 502 的总结论
	if strings.Contains(out, "链路断在中台到插件这一段") {
		t.Errorf("不该用只在单一成因下成立的结论，实际：\n%s", out)
	}
	// 中台的原话照旧要带回来——它是判断成因的第一手信息
	if !strings.Contains(out, "该插件当前没有可用实例") {
		t.Errorf("要原样带回中台的 message，实际：\n%s", out)
	}
}

func Test_e2e_非_JSON_响应原样打印不崩(t *testing.T) {
	html := "<html>\r\n<head><title>502 Bad Gateway</title></head>\r\n" +
		"<body><center><h1>502 Bad Gateway</h1></center><hr><center>nginx</center></body>\r\n</html>"
	srv, _ := newFakeHub(t, http.StatusBadGateway, "text/html", html)

	code, out := runE2ECapture(t, srv.URL, "auth", e2eOptions(`{}`, time.Second))
	if code != exitFail {
		t.Errorf("退出码 = %d，期望 %d", code, exitFail)
	}
	if !strings.Contains(out, "响应不是 JSON，可能是 nginx 或中间层返回的") {
		t.Errorf("要有这句提示，实际：\n%s", out)
	}
	if !strings.Contains(out, "<title>502 Bad Gateway</title>") {
		t.Errorf("非 JSON 响应要原样打印，实际：\n%s", out)
	}
}

// 200 也可能是假绿：nginx 配错路径时能回 200 的 HTML 目录页。这个命令是验收工具
// （脚本里 `hubprobe e2e ... && 下一步`），把它当通过会直接导致错误的部署决定，
// 所以「状态码 200」不足以放行，响应体必须是中台那份 JSON。
func Test_e2e_200_但响应不是_JSON_不算通过(t *testing.T) {
	srv, _ := newFakeHub(t, http.StatusOK, "text/html",
		"<html><head><title>Welcome to nginx!</title></head><body>...</body></html>")

	code, out := runE2ECapture(t, srv.URL, "auth", e2eOptions(`{}`, time.Second))
	if code != exitFail {
		t.Errorf("退出码 = %d，期望 %d——200 加非 JSON 是假绿，不能放行", code, exitFail)
	}
	if !strings.Contains(out, "响应不是 JSON，可能是 nginx 或中间层返回的") {
		t.Errorf("要有非 JSON 的提示，实际：\n%s", out)
	}
	if !strings.Contains(out, "不能算验收通过") {
		t.Errorf("要说清为什么不算通过，而不是给人一句「跑通了」，实际：\n%s", out)
	}
	if !strings.Contains(out, "Welcome to nginx!") {
		t.Errorf("原样响应要打出来，实际：\n%s", out)
	}
}

func Test_e2e_200_但响应体为空不算通过(t *testing.T) {
	srv, _ := newFakeHub(t, http.StatusOK, "application/json", "")

	code, out := runE2ECapture(t, srv.URL, "auth", e2eOptions(`{}`, time.Second))
	if code != exitFail {
		t.Errorf("退出码 = %d，期望 %d——空响应体同样拿不到中台的结论", code, exitFail)
	}
	if strings.Contains(out, "端到端跑通") {
		t.Errorf("空响应体不能说成跑通，实际：\n%s", out)
	}
}

// 光判「能不能解析成 JSON」不够：null / [] / "x" 都是合法 JSON，解析得过，却一个字段都
// 取不到，跟空体一样拿不到中台的结论。放行它们等于在这条防假绿规则上留了个洞——
// 中间层随手回一个 200 的 `null` 就能骗过验收脚本，而脚本正是这个命令的主场。
func Test_e2e_200_但响应不是_JSON_对象不算通过(t *testing.T) {
	cases := []struct {
		name string
		body string
	}{
		{"null", "null"},
		{"空数组", "[]"},
		{"字符串", `"x"`},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			srv, _ := newFakeHub(t, http.StatusOK, "application/json", tc.body)

			code, out := runE2ECapture(t, srv.URL, "auth", e2eOptions(`{}`, time.Second))
			if code != exitFail {
				t.Errorf("响应体 %s 退出码 = %d，期望 %d——200 加非对象 JSON 是假绿，不能放行", tc.body, code, exitFail)
			}
			if strings.Contains(out, "端到端跑通") {
				t.Errorf("响应体 %s 不该说成跑通，实际：\n%s", tc.body, out)
			}
			// 「是 JSON 但不是对象」得单独说清：它跟「压根不是 JSON」成因不同，
			// 用错那句会让人去查一个不存在的问题（比如去翻 nginx 的错误页）
			if !strings.Contains(out, "不是一个 JSON 对象") {
				t.Errorf("响应体 %s 要说明是「合法 JSON 但不是对象」，实际：\n%s", tc.body, out)
			}
			if !strings.Contains(out, "不能算验收通过") {
				t.Errorf("响应体 %s 要说清为什么不算通过，实际：\n%s", tc.body, out)
			}
		})
	}
}

// 「是 JSON 对象」这级还是太松：中台自己的 404 错误体（{"error":"not_found","message":...}）
// 就是个货真价实的 JSON 对象。中间层把一份错误响应配成 200 转发过来（或代理改写了状态码），
// 光按「是对象」放行，脚本里 `hubprobe e2e ... && 下一步` 照样被骗过去——而这是验收工具，
// 假绿比误报贵得多。
//
// 判据只到「带中台的物证字段（plugin）」，不再往字段取值上收紧：误伤的会是正常响应，
// 而被误伤的人第一反应是怀疑这个工具本身，那比漏掉一个边角场景更损伤它。
func Test_e2e_200_但响应没有中台物证字段不算通过(t *testing.T) {
	cases := []struct {
		name string
		body string
	}{
		{
			// 验证方实测骗过验收脚本的那一份：状态码 200，体是中台的 404 错误体
			name: "把中台的 404 错误体配成 200",
			body: `{"error":"not_found","message":"插件 x 未注册"}`,
		},
		{
			name: "空对象",
			body: `{}`,
		},
		{
			// plugin 是 null 也算没有物证：拿不到「中台定位到了哪个插件」这个结论
			name: "有 plugin 键但值为 null",
			body: `{"plugin":null}`,
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			srv, _ := newFakeHub(t, http.StatusOK, "application/json", tc.body)

			code, out := runE2ECapture(t, srv.URL, "auth", e2eOptions(`{}`, time.Second))
			if code != exitFail {
				t.Errorf("响应体 %s 退出码 = %d，期望 %d——200 加任意 JSON 对象是假绿，不能放行",
					tc.body, code, exitFail)
			}
			if strings.Contains(out, "端到端跑通") {
				t.Errorf("响应体 %s 不该说成跑通，实际：\n%s", tc.body, out)
			}
			// 缺什么得点名，否则用户拿手上那份响应根本对不上号
			if !strings.Contains(out, "plugin") || !strings.Contains(out, "不能算验收通过") {
				t.Errorf("响应体 %s 要说清少了哪个物证字段、以及为什么不算通过，实际：\n%s", tc.body, out)
			}
			// 中台的原话照旧要打出来——它是判断这份响应来自哪里的第一手信息
			if !strings.Contains(out, "未注册") && strings.Contains(tc.body, "未注册") {
				t.Errorf("响应体 %s 要原样打印，实际：\n%s", tc.body, out)
			}
		})
	}
}

// 反面：收紧不能伤到正常响应。中台 200 的成功体按 IngressResponse 的定义带 plugin，
// 但**不带** payload 也是合法的（插件回业务类型时给的是 payload_type_url / payload_base64），
// 所以判据不能挂在 payload 上——那样会把一类正常插件判成失败。
func Test_e2e_200_的正常响应不被误伤(t *testing.T) {
	cases := []struct {
		name string
		body string
	}{
		{
			name: "Struct 载荷（带 payload）",
			body: `{"message_id":"m-1","trace_id":"t-1","plugin":"auth","version":"v1","instance_id":"auth-1","elapsed_ms":12,"payload":{"ok":true}}`,
		},
		{
			name: "业务类型载荷（没有 payload，只有 payload_type_url）",
			body: `{"message_id":"m-1","trace_id":"t-1","plugin":"auth","version":"v1","instance_id":"auth-1","elapsed_ms":12,"payload_type_url":"type.googleapis.com/wms.v1.Order","payload_base64":"CgN4eXo="}`,
		},
		{
			name: "引用通道（超内联上限，只有 payload_ref）",
			body: `{"message_id":"m-1","trace_id":"t-1","plugin":"auth","version":"v1","instance_id":"auth-1","elapsed_ms":12,"payload_ref":{"uri":"/blobs/01M2S6BZF4X361ZPGAPSVPJ6S5"}}`,
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			srv, _ := newFakeHub(t, http.StatusOK, "application/json", tc.body)

			code, out := runE2ECapture(t, srv.URL, "auth", e2eOptions(`{}`, time.Second))
			if code != exitOK {
				t.Errorf("响应体 %s 退出码 = %d，期望 %d（这是中台的正常响应，误伤比漏报更糟）；输出：\n%s",
					tc.body, code, exitOK, out)
			}
			if !strings.Contains(out, "端到端跑通") {
				t.Errorf("响应体 %s 要说跑通，实际：\n%s", tc.body, out)
			}
		})
	}
}

func Test_e2e_连不上中台时给出下一步(t *testing.T) {
	srv, _ := newFakeHub(t, http.StatusOK, "application/json", `{}`)
	base := srv.URL
	srv.Close() // 端口立刻没人监听，制造「连不上」

	code, out := runE2ECapture(t, base, "auth", e2eOptions(`{}`, time.Second))
	if code != exitFail {
		t.Errorf("退出码 = %d，期望 %d", code, exitFail)
	}
	if !strings.Contains(out, "连不上") {
		t.Errorf("要说清是连不上而不是别的，实际：\n%s", out)
	}
	if !strings.Contains(out, "http://") {
		t.Errorf("要点到地址写法，实际：\n%s", out)
	}
}

func Test_e2e_超时非正数本地拦下(t *testing.T) {
	// 中台会用 400 拒掉非正的 timeout_ms，本地先拦能省一轮往返并说清是哪个参数
	code, out := runE2ECapture(t, "http://127.0.0.1:1", "auth", e2eOptions(`{}`, 0))
	if code != exitUsage {
		t.Errorf("退出码 = %d，期望 %d", code, exitUsage)
	}
	if !strings.Contains(out, "timeout_ms") {
		t.Errorf("要说到 timeout_ms，实际：\n%s", out)
	}
}

func Test_e2e_载荷不是对象时报错(t *testing.T) {
	code, out := runE2ECapture(t, "http://127.0.0.1:1", "auth", e2eOptions(`[1,2,3]`, time.Second))
	if code != exitFail {
		t.Errorf("退出码 = %d，期望 %d", code, exitFail)
	}
	if !strings.Contains(out, "JSON 对象") {
		t.Errorf("错误信息应说清要求，实际：\n%s", out)
	}
}

func Test_e2e_经_run_分发时参数串对(t *testing.T) {
	srv, hub := newFakeHub(t, http.StatusOK, "application/json",
		`{"plugin":"auth","instance_id":"auth-1","elapsed_ms":3}`)

	var code int
	out := captureOutput(t, func() {
		code = run([]string{"e2e", srv.URL, "auth", "--payload", `{"token":"t"}`, "--timeout", "2s"})
	})
	if code != exitOK {
		t.Fatalf("退出码 = %d；输出：\n%s", code, out)
	}

	got := hub.recorded()
	if got.path != "/ingress/auth" {
		t.Errorf("路径 = %q", got.path)
	}
	if got.body["timeout_ms"] != float64(2000) {
		t.Errorf("timeout_ms = %v，期望 2000（--timeout 就是中台的 timeout_ms）", got.body["timeout_ms"])
	}
	payload, _ := got.body["payload"].(map[string]any)
	if payload["token"] != "t" {
		t.Errorf("payload = %v", got.body["payload"])
	}
}

func Test_e2e_少给参数时报用法错误(t *testing.T) {
	var code int
	out := captureOutput(t, func() { code = run([]string{"e2e", "http://127.0.0.1:8092"}) })

	if code != exitUsage {
		t.Errorf("退出码 = %d，期望 %d", code, exitUsage)
	}
	if !strings.Contains(out, "<中台地址> <插件名>") {
		t.Errorf("要说清缺的是哪个参数，实际：\n%s", out)
	}
}

// ---- 地址口径：本地 127.0.0.1:8092（HTTP 面）/ UAT 经 nginx 的域名地址 ----
//
// 两个环境各有各的中台 HTTP 面地址，写反了在本地就是一句 connection refused。
// 示例命令写给本地的开发者看，所以用 8092。
//
// UAT 一律只给**域名形式**（https://<域名>:8081/hub-api），不给裸端口：UAT 主机上中台
// 自己听在另一个内部端口，那是部署视角的数字，在这个工具里写出来就会被读者拼成
// 127.0.0.1:<那个端口> 再试一遍——而他手上正是一条连不上的错误，这条提示是他唯一的线索，
// 指错方向等于把人送去排查一个根本不存在的服务。

func Test_用法示例里的中台地址用本地的端口(t *testing.T) {
	out := captureOutput(t, func() { usage() })

	if !strings.Contains(out, "hubprobe e2e http://127.0.0.1:8092") {
		t.Errorf("e2e 示例要用本地中台的 HTTP 面 8092，实际：\n%s", out)
	}
	if strings.Contains(out, "http://127.0.0.1:8095") {
		t.Errorf("8095 是 UAT 主机内部的监听地址，在开发机上敲不到，不能出现在这个工具里，实际：\n%s", out)
	}
	// 只断言「不含 http://127.0.0.1:8095」会漏掉 N1 的**原形态**——当初的写法正是裸端口
	// 「UAT 上是 8095」，没有 127.0.0.1 前缀。读者照样会把它拼成 127.0.0.1:8095 再试一遍，
	// 所以这里直接卡死裸端口：UAT 那侧只允许域名形式出现。
	if strings.Contains(out, "8095") {
		t.Errorf("usage 里不能出现裸端口 8095（N1 的原形态就是这个），UAT 只给域名形式，实际：\n%s", out)
	}
	if !strings.Contains(out, "https://<域名>:8081/hub-api") {
		t.Errorf("UAT 要给出域名形式的可敲地址，实际：\n%s", out)
	}
}

func Test_连不上时的提示给两个环境的可敲地址且不含裸端口(t *testing.T) {
	// 这条提示是用户手上唯一的线索，只给一个环境，另一个环境的人照着改就正好改错。
	// 但 UAT 那一份只能是域名形式——裸端口会被拼到 127.0.0.1 上再试一遍。
	msg := explainTransportError("http://127.0.0.1:8092", "http://127.0.0.1:8092/ingress/auth",
		errors.New("connection refused"))

	for _, want := range []string{"127.0.0.1:8092", "https://<域名>:8081/hub-api", "UAT"} {
		if !strings.Contains(msg, want) {
			t.Errorf("提示里要有 %q（本地与 UAT 都得覆盖），实际：\n%s", want, msg)
		}
	}
	if strings.Contains(msg, "8095") {
		t.Errorf("不能出现裸端口 8095：读者会把它拼成 127.0.0.1:8095 再试一遍，实际：\n%s", msg)
	}
	if !strings.Contains(msg, "连不上") || !strings.Contains(msg, "connection refused") {
		t.Errorf("仍要说清是连不上、并带回原始错误，实际：\n%s", msg)
	}
}

// 自证动作必须指向**用户填进来的那个中台地址**，不能指向拼好的调用地址：
// endpoint 已经带了 /ingress/<插件名>，后面再挂 /health 是个不存在的路径，回一个 404，
// 而文档里写着「404 或一段 HTML 是前缀不对」——照做的人会被引去查一个不存在的前缀问题。
func Test_连不上时的自证动作指向用户填的地址(t *testing.T) {
	base := "https://hub.example.com:8081/hub-api"
	endpoint := base + "/ingress/v2-reader"

	msg := explainTransportError(base, endpoint, errors.New("connection refused"))

	if !strings.Contains(msg, base+"/health") {
		t.Errorf("要教人拿**填进来的地址**接 /health 去试，即 %s/health，实际：\n%s", base, msg)
	}
	if strings.Contains(msg, endpoint+"/health") {
		t.Errorf("不能教人拿拼好的调用地址接 /health（那个路径不存在，会回 404 把人引偏），实际：\n%s", msg)
	}

	// 末尾带斜杠的地址不能拼出 //health——那个路径在 nginx 上同样是 404，
	// 而这里给的是「照着敲」的命令，敲出来必须是能用的
	msg = explainTransportError("http://127.0.0.1:8092/", "http://127.0.0.1:8092/ingress/auth",
		errors.New("connection refused"))
	if !strings.Contains(msg, "http://127.0.0.1:8092/health") {
		t.Errorf("末尾斜杠要去干净，实际：\n%s", msg)
	}
	if strings.Contains(msg, "8092//health") {
		t.Errorf("不能拼出 //health，实际：\n%s", msg)
	}
}
