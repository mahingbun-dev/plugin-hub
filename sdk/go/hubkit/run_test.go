package hubkit_test

import (
	"context"
	"io"
	"log/slog"
	"strings"
	"sync"
	"testing"
	"time"

	"google.golang.org/protobuf/reflect/protoreflect"

	"github.com/mahingbun-dev/anc-hub/sdk/go/hubkit"
	"github.com/mahingbun-dev/anc-hub/sdk/go/mockhub"
	"github.com/mahingbun-dev/anc-hub/sdk/go/proto/hubv1"
)

// echoPlugin 是一个最小但完整的插件：有契约、有校验器、有插件体。
//
// 它额外实现 [hubkit.StateAware]，因为那是**客户端侧**「注册真的完成了」的唯一
// 可观测点：注入发生在 SDK 写下 stateToken 之后，而注销用的正是那个凭证。
// 只等中台回话说「收到注册了」还差一截——在那个窗口内退出，SDK 会按
// 「本实例没有凭证」这个正当理由跳过注销（见 [registrar.unregister]）。
type echoPlugin struct {
	hubkit.Base

	// 注入来自注册循环那个 goroutine，测试在另一个 goroutine 上读，
	// 所以用锁护住——裸字段会让 `go test -race` 报警（同 state_test.go 的 statePlugin）。
	mu    sync.Mutex
	state *hubkit.StateClient
}

func (p *echoPlugin) SetState(s *hubkit.StateClient) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.state = s
}

// stateClient 返回已注入的客户端；还没注入时是 nil，即「凭证还没到手」。
func (p *echoPlugin) stateClient() *hubkit.StateClient {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.state
}

func newEchoPlugin() *echoPlugin {
	return &echoPlugin{Base: hubkit.Base{
		Files: []protoreflect.FileDescriptor{hubv1.File_hub_v1_envelope_proto},
	}}
}

func (p *echoPlugin) Manifest() *hubv1.PluginManifest {
	return &hubv1.PluginManifest{
		Name:        "echo",
		Version:     "1.0.0",
		Description: "回显插件",
		Owner:       "testkit",
		Consumes: []*hubv1.MessageContract{
			{FqName: hubkit.StructFQName},
		},
		Tools: []*hubv1.ToolDecl{
			{Name: "echo", Description: "原样回显"},
		},
	}
}

func (p *echoPlugin) Validate(_ context.Context, env *hubv1.Envelope) (*hubv1.ValidateResponse, error) {
	if _, ok := hubkit.PayloadJSON(env); !ok {
		return hubkit.Invalid(hubkit.Issue("payload", "需要 JSON 对象载荷")), nil
	}
	return hubkit.Valid(), nil
}

func (p *echoPlugin) Handle(_ context.Context, env *hubv1.Envelope) (*hubv1.Envelope, error) {
	payload, ok := hubkit.PayloadJSON(env)
	if !ok {
		return nil, io.ErrUnexpectedEOF
	}
	payload["handled_by"] = "echo"
	return hubkit.WithPayloadJSON(env, payload)
}

// startPlugin 起一个插件，返回停止函数与它的对外地址。
func startPlugin(t *testing.T, hubAddr string, mutate func(*hubkit.Config)) (context.CancelFunc, string) {
	t.Helper()
	return startPluginWith(t, hubAddr, newEchoPlugin(), mutate)
}

// startPluginWith 是 startPlugin 的可注入版本：调用方自己给插件实例，
// 因而能在插件侧观察状态——比如「凭证已到手」（见 echoPlugin 的说明）。
func startPluginWith(t *testing.T, hubAddr string, plugin hubkit.Plugin, mutate func(*hubkit.Config)) (context.CancelFunc, string) {
	t.Helper()

	addr, err := mockhub.FreeAddr()
	if err != nil {
		t.Fatalf("挑端口失败: %v", err)
	}

	cfg := hubkit.Config{
		HubAddr: hubAddr,
		// 关键：上报的必须是中台能拨通的地址，而不是本机视角的 localhost
		AdvertiseAddr: "http://" + addr,
		ListenAddr:    addr,
		InstanceID:    "test-instance",
		RetryInterval: 20 * time.Millisecond,
		Logger:        slog.New(slog.NewTextHandler(io.Discard, nil)),
	}
	if mutate != nil {
		mutate(&cfg)
	}

	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() {
		defer close(done)
		if err := hubkit.RunContext(ctx, plugin, cfg); err != nil {
			t.Logf("插件退出: %v", err)
		}
	}()

	// 等插件真的在监听，避免注册时探测失败
	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		if client, err := mockhub.DialPlugin("http://" + addr); err == nil {
			if _, err := client.Health(context.Background()); err == nil {
				client.Close()
				break
			}
			client.Close()
		}
		time.Sleep(10 * time.Millisecond)
	}

	return func() {
		cancel()
		<-done
	}, addr
}

// waitForState 等到插件收到状态客户端注入，或 ctx 结束。
//
// 这是「客户端侧注册完成」的同步点：注入在 stateToken 写入之后发生，
// 所以等到它就等于等到凭证到手。
func waitForState(t *testing.T, ctx context.Context, plugin *echoPlugin) {
	t.Helper()

	ticker := time.NewTicker(10 * time.Millisecond)
	defer ticker.Stop()

	for {
		if plugin.stateClient() != nil {
			return
		}
		select {
		case <-ctx.Done():
			t.Fatal("插件应当收到状态客户端注入（它就是「凭证已到手」的信号）")
		case <-ticker.C:
		}
	}
}

func startHub(t *testing.T, opts mockhub.Options) *mockhub.Hub {
	t.Helper()
	hub, err := mockhub.Start(opts)
	if err != nil {
		t.Fatalf("启动 mock 中台失败: %v", err)
	}
	t.Cleanup(hub.Close)
	return hub
}

func Test插件注册所需的一切都报给了中台(t *testing.T) {
	hub := startHub(t, mockhub.Options{})
	stop, addr := startPlugin(t, hub.Addr(), nil)
	defer stop()

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	if err := hub.WaitForRegistration(ctx, 1); err != nil {
		t.Fatal(err)
	}

	reg := hub.LastRegistration()
	if reg.GetPluginName() != "echo" {
		t.Errorf("plugin_name = %q", reg.GetPluginName())
	}
	if reg.GetVersion() != "1.0.0" {
		t.Errorf("version = %q", reg.GetVersion())
	}
	if want := "http://" + addr; reg.GetAdvertiseAddr() != want {
		t.Errorf("advertise_addr = %q，期望 %q", reg.GetAdvertiseAddr(), want)
	}
	if reg.GetInstanceId() != "test-instance" {
		t.Errorf("instance_id = %q", reg.GetInstanceId())
	}
	if reg.GetManifest() == nil {
		t.Fatal("manifest 必须随注册提交")
	}
	if got := reg.GetManifest().GetConsumes(); len(got) != 1 || got[0].GetFqName() != hubkit.StructFQName {
		t.Errorf("manifest 的 consumes 不对: %v", got)
	}
	if len(reg.GetDescriptorSet()) == 0 {
		t.Error("descriptor 必须随注册提交——中台靠它做字段级兼容检查")
	}
}

func Test被调用时校验器先于插件体(t *testing.T) {
	hub := startHub(t, mockhub.Options{})
	stop, addr := startPlugin(t, hub.Addr(), nil)
	defer stop()

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := hub.WaitForRegistration(ctx, 1); err != nil {
		t.Fatal(err)
	}

	client, err := mockhub.DialPlugin("http://" + addr)
	if err != nil {
		t.Fatalf("连插件失败: %v", err)
	}
	defer client.Close()

	t.Run("合法载荷通过校验并回显", func(t *testing.T) {
		env, err := hubkit.WithPayloadJSON(&hubv1.Envelope{MessageId: "m-1"}, map[string]any{"text": "你好"})
		if err != nil {
			t.Fatalf("打包失败: %v", err)
		}

		resp, err := client.Validate(ctx, env)
		if err != nil {
			t.Fatalf("校验调用失败: %v", err)
		}
		if !resp.GetValid() {
			t.Fatalf("应通过校验: %v", resp.GetIssues())
		}

		out, err := client.Handle(ctx, env)
		if err != nil {
			t.Fatalf("处理失败: %v", err)
		}
		payload, ok := hubkit.PayloadJSON(out)
		if !ok {
			t.Fatal("输出应仍是 JSON 载荷")
		}
		if payload["handled_by"] != "echo" {
			t.Errorf("handled_by = %v", payload["handled_by"])
		}
		if payload["text"] != "你好" {
			t.Errorf("原载荷应保留，text = %v", payload["text"])
		}
	})

	t.Run("非 JSON 载荷被校验器拒绝", func(t *testing.T) {
		env := &hubv1.Envelope{MessageId: "m-2", Payload: nil}

		resp, err := client.Validate(ctx, env)
		if err != nil {
			t.Fatalf("校验调用失败: %v", err)
		}
		if resp.GetValid() {
			t.Fatal("没有 JSON 载荷时应拒绝")
		}
		if resp.GetIssues()[0].GetPath() != "payload" {
			t.Errorf("问题应定位到 payload，实际 %q", resp.GetIssues()[0].GetPath())
		}
	})
}

func Test心跳按时发送(t *testing.T) {
	hub := startHub(t, mockhub.Options{HeartbeatIntervalSeconds: 1})
	stop, _ := startPlugin(t, hub.Addr(), nil)
	defer stop()

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	if err := hub.WaitForRegistration(ctx, 1); err != nil {
		t.Fatal(err)
	}
	// 中台指定 1 秒一拍，等到 2 拍说明心跳循环确实在跑
	if err := hub.WaitForHeartbeats(ctx, 2); err != nil {
		t.Fatal(err)
	}
}

func Test被摘除后能自动重新注册(t *testing.T) {
	// 收到第 1 拍心跳后就开始要求重新注册
	hub := startHub(t, mockhub.Options{
		HeartbeatIntervalSeconds: 1,
		ReregisterAfterBeats:     1,
	})
	stop, _ := startPlugin(t, hub.Addr(), nil)
	defer stop()

	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()

	if err := hub.WaitForRegistration(ctx, 1); err != nil {
		t.Fatal(err)
	}
	// 关键在于「第二次注册」：心跳被中台要求重注册后，插件应重走注册流程
	if err := hub.WaitForRegistration(ctx, 2); err != nil {
		t.Fatal(err)
	}
}

func Test注册被拒时如实报出原因且持续重试(t *testing.T) {
	hub := startHub(t, mockhub.Options{
		RejectRegister: func(*hubv1.RegisterRequest) []*hubv1.Rejection {
			return []*hubv1.Rejection{{
				Code:    hubv1.RejectCode_REJECT_CODE_BREAKING_CHANGE,
				Message: "相对版本 1.0.0 存在破坏性契约变更",
				Detail:  "wms.v1.Order 字段 sku（编号 1）已被删除",
			}}
		},
	})
	stop, _ := startPlugin(t, hub.Addr(), nil)
	defer stop()

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	// 重试间隔 20ms，短时间内应看到多次尝试——插件不该因为一次被拒就放弃
	if err := hub.WaitForRegistration(ctx, 3); err != nil {
		t.Fatal(err)
	}
}

func Test退出时主动注销(t *testing.T) {
	hub := startHub(t, mockhub.Options{})
	plugin := newEchoPlugin()
	stop, _ := startPluginWith(t, hub.Addr(), plugin, nil)

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := hub.WaitForRegistration(ctx, 1); err != nil {
		t.Fatal(err)
	}
	// **必须连客户端侧一起等到**：中台回「收到注册了」只说明它记下了那一行，
	// 而注销要用的是插件手里的 stateToken，它晚一步才被写进去。只等前者就 stop()，
	// 会在两者之间的窗口里退出——SDK 于是按「本实例没有凭证」跳过注销，
	// 下面那条断言便以一个与被测行为无关的理由失败（实测 -count=30 挂 7 次以上）。
	waitForState(t, ctx, plugin)

	// WaitForRegistration 只能证明「中台收到了注册请求」，证明不了「插件已处理完
	// 注册响应、拿到凭证」。cancel 恰好落在两者之间的窗口里（亚毫秒级）时，插件侧
	// registerOnce 会因 ctx 取消拿不到响应，凭证未置位，退出时**正确地**跳过注销
	// ——那不是插件的行为问题，是测试撤销得太快。这里等一个远大于竞态窗口的间隔。
	time.Sleep(50 * time.Millisecond)

	stop()

	unregisters := hub.Unregisters()
	if len(unregisters) == 0 {
		t.Fatal("退出时应主动注销，中台才能立刻摘掉实例而不必等心跳超时")
	}
	if unregisters[0] != "test-instance" {
		t.Errorf("注销的实例 id = %q", unregisters[0])
	}
	// 注销要带上注册时下发的凭证：中台靠它认出「你要摘的是不是自己那一行」。
	// `instance_id` 是插件自报的、可以撞，没有凭证中台无从判断。
	if tokens := hub.UnregisterTokens(); tokens[0] != hub.StateToken() {
		t.Errorf("注销携带的凭证 = %q，应为注册时下发的 %q", tokens[0], hub.StateToken())
	}
}

// **注册从未成功过就不发注销**。
//
// 凭证只在注册成功时下发，所以「手里没有凭证」等于「本实例没进过注册表」，此时发注销
// 摘不掉任何东西。而 `instance_id` 是插件自报的、可以跟别的插件撞（缺省「主机名-PID」，
// 同一 host 网络下容器 PID 又都是 1）——多发的这一发若被中台按 `instance_id` 删行，
// 删掉的正是**对方**那一行：实测 auth 重启一次，sql-executor 的工具从 MCP 工具面上
// 全部消失，而它自己的日志停在「已注册到中台」之后毫无异常。
func Test注册从未成功时不发注销(t *testing.T) {
	attempted := make(chan struct{}, 1)
	hub := startHub(t, mockhub.Options{
		RejectRegister: func(*hubv1.RegisterRequest) []*hubv1.Rejection {
			select {
			case attempted <- struct{}{}:
			default:
			}
			return []*hubv1.Rejection{{
				Code:    hubv1.RejectCode_REJECT_CODE_MANIFEST_INVALID,
				Message: "本测试总是拒绝注册",
			}}
		},
	})
	stop, _ := startPlugin(t, hub.Addr(), nil)

	// 先确认插件确实尝试过注册，否则「没发注销」可能只是它还没跑起来
	select {
	case <-attempted:
	case <-time.After(5 * time.Second):
		t.Fatal("插件没有尝试注册")
	}

	stop()

	if got := hub.Unregisters(); len(got) != 0 {
		t.Errorf("注册没成功过就不该发注销，实际发了 %v", got)
	}
}

func Test缺少必填配置时明确报错(t *testing.T) {
	err := hubkit.RunContext(context.Background(), newEchoPlugin(), hubkit.Config{})
	if err == nil {
		t.Fatal("缺少必填配置应报错")
	}
	for _, want := range []string{"HUB_ADDR", "HUB_ADVERTISE_ADDR"} {
		if !strings.Contains(err.Error(), want) {
			t.Errorf("错误信息应提到 %s，实际: %v", want, err)
		}
	}
}
