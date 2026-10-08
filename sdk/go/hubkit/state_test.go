package hubkit_test

import (
	"context"
	"io"
	"log/slog"
	"sync"
	"testing"
	"time"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	"github.com/mahingbun-dev/anc-hub/sdk/go/hubkit"
	"github.com/mahingbun-dev/anc-hub/sdk/go/mockhub"
	"github.com/mahingbun-dev/anc-hub/sdk/go/proto/hubv1"
)

// statePlugin 实现了 StateAware：注册成功后应当收到注入。
//
// 注入来自注册循环那个 goroutine，测试在另一个 goroutine 上读，所以这里用锁护住——
// 裸字段会让 `go test -race` 报警（测试自己有竞态，等于白测）。
type statePlugin struct {
	hubkit.Base

	mu    sync.Mutex
	state *hubkit.StateClient
}

func (p *statePlugin) SetState(s *hubkit.StateClient) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.state = s
}

// stateClient 返回当前收到的客户端；还没注入时是 nil。
func (p *statePlugin) stateClient() *hubkit.StateClient {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.state
}

func (p *statePlugin) Manifest() *hubv1.PluginManifest {
	return &hubv1.PluginManifest{Name: "state-test", Version: "1.0.0"}
}

func (p *statePlugin) Validate(context.Context, *hubv1.Envelope) (*hubv1.ValidateResponse, error) {
	return hubkit.Valid(), nil
}

func (p *statePlugin) Handle(_ context.Context, env *hubv1.Envelope) (*hubv1.Envelope, error) {
	return env, nil
}

// startStatePlugin 起一个实现了 StateAware 的插件，返回它的配置与停止函数。
//
// 监听地址取一个空闲端口而不是缺省的 :9000——本机上跑着别的插件（9000 常被占），
// 固定端口会让测试莫名其妙地 bind 失败。
//
// retryInterval 同时是"denial 触发的强制重注册"的冷却窗口（见 [registrar.heartbeatLoop]），
// 所以下面几个用例各按自己需要传值：要它立刻生效就传小值并先睡过窗口，
// 要它被挡住就传大值。
func startStatePlugin(t *testing.T, hubAddr string, retryInterval time.Duration) (*statePlugin, context.CancelFunc) {
	t.Helper()

	addr, err := mockhub.FreeAddr()
	if err != nil {
		t.Fatalf("挑端口失败: %v", err)
	}

	plugin := &statePlugin{}
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() {
		defer close(done)
		_ = hubkit.RunContext(ctx, plugin, hubkit.Config{
			HubAddr:       hubAddr,
			AdvertiseAddr: "http://" + addr,
			ListenAddr:    addr,
			RetryInterval: retryInterval,
			Logger:        slog.New(slog.NewTextHandler(io.Discard, nil)),
		})
	}()

	return plugin, func() {
		cancel()
		<-done
	}
}

// waitForStateClient 等到插件收到注入，或 ctx 结束。
func waitForStateClient(t *testing.T, ctx context.Context, plugin *statePlugin) *hubkit.StateClient {
	t.Helper()

	ticker := time.NewTicker(10 * time.Millisecond)
	defer ticker.Stop()

	for {
		if client := plugin.stateClient(); client != nil {
			return client
		}
		select {
		case <-ctx.Done():
			t.Fatal("实现了 StateAware 的插件必须收到状态客户端注入")
			return nil
		case <-ticker.C:
		}
	}
}

func TestStateAware被注入且能往返(t *testing.T) {
	hub, err := mockhub.Start(mockhub.Options{})
	if err != nil {
		t.Fatalf("起 mock 中台失败: %v", err)
	}
	defer hub.Close()

	plugin, stop := startStatePlugin(t, hub.Addr(), 10*time.Millisecond)
	defer stop()

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	if err := hub.WaitForRegistration(ctx, 1); err != nil {
		t.Fatal(err)
	}
	state := waitForStateClient(t, ctx, plugin)

	if err := state.Put(ctx, "s", "k1", []byte("v1"), 0); err != nil {
		t.Fatalf("写入失败: %v", err)
	}
	value, found, err := state.Get(ctx, "s", "k1")
	if err != nil {
		t.Fatalf("读取失败: %v", err)
	}
	if !found || string(value) != "v1" {
		t.Fatalf("往返失败: found=%v value=%q", found, value)
	}

	// 另外三个方法也走同一条连接、同一套凭证，一起验掉，避免"只有 Get/Put 接对了"
	if deleted, err := state.Delete(ctx, "s", "k1"); err != nil || !deleted {
		t.Fatalf("删除失败: deleted=%v err=%v", deleted, err)
	}
	if _, found, err := state.Get(ctx, "s", "k1"); err != nil || found {
		t.Fatalf("删除后不该还读得到: found=%v err=%v", found, err)
	}
	if err := state.Put(ctx, "scan", "p1", []byte("v1"), 0); err != nil {
		t.Fatalf("写入失败: %v", err)
	}
	if err := state.Put(ctx, "scan", "p2", []byte("v2"), 0); err != nil {
		t.Fatalf("写入失败: %v", err)
	}
	entries, err := state.Scan(ctx, "scan", "p", 10)
	if err != nil {
		t.Fatalf("扫描失败: %v", err)
	}
	if len(entries) != 2 || entries[0].Key != "p1" || string(entries[0].Value) != "v1" {
		t.Fatalf("扫描结果不对: %+v", entries)
	}
}

// Test状态凭证被拒时触发重新注册 验的是"插件侧收到 UNAUTHENTICATED 就该重走注册流程"
// 这个行为本身。
//
// **不去验"重注册后恢复可用"**：mock 的 DenyStateToken 只拒不放（凭证照发、校验全拒），
// 那条闭环在 mock 上验不了——不是没写对。真实身份反查与恢复由真中台的集成测试覆盖。
func Test状态凭证被拒时触发重新注册(t *testing.T) {
	hub, err := mockhub.Start(mockhub.Options{DenyStateToken: true})
	if err != nil {
		t.Fatalf("起 mock 中台失败: %v", err)
	}
	defer hub.Close()

	plugin, stop := startStatePlugin(t, hub.Addr(), 100*time.Millisecond)
	defer stop()

	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()

	if err := hub.WaitForRegistration(ctx, 1); err != nil {
		t.Fatal(err)
	}
	state := waitForStateClient(t, ctx, plugin)

	// DenyStateToken 的语义是"凭证照发、校验全拒"，所以凭证不是空的
	if hub.StateToken() == "" {
		t.Fatal("DenyStateToken 只影响校验，凭证仍应下发")
	}

	// 先睡过冷却窗口（它等于 RetryInterval）：刚注册就被拒的那次 denial 会被
	// 速率下限挡掉，本用例要验的是"窗口之外"的那次。
	time.Sleep(200 * time.Millisecond)

	// 插件拿到的必须是原样的 Unauthenticated——既不能被吞掉，也不能变成别的错误码
	if _, _, err := state.Get(ctx, "s", "k"); status.Code(err) != codes.Unauthenticated {
		t.Fatalf("状态凭证被拒时应把 Unauthenticated 交给插件，实际 %v", err)
	}

	// 关键行为：收到 UNAUTHENTICATED 后，注册循环应重走注册流程（中台重启/摘除会让旧凭证失效）
	if err := hub.WaitForRegistration(ctx, 2); err != nil {
		t.Fatalf("收到 UNAUTHENTICATED 后应触发重新注册: %v", err)
	}
}

// Test冷却窗口内的denial被忽略 是上面那条的速率下限：窗口内的 denial 必须被丢掉。
//
// 为什么必须有这条：`loop` 只对**失败的**注册应用 RetryInterval，一次成功的注册之后
// 若紧接着排空 denial 信号，就会**零延迟**转回注册——速率等于 Register RPC 的延迟。
// 持续拒绝（中台校验滞后、撤销尚未传播、或非 token 原因表现为 401）时，通道一直是满的，
// 于是每次 heartbeatLoop 启动都立刻排空并返回：单个插件每秒数百次注册、无限持续，
// 同时还在反复探测插件自己的地址。
//
// 冷却窗口取得比任何可能的测试停顿都长（2s），所以"注册之后马上敲一次 denial"这件事
// 在窗口内是确定的，不存在时序竞争。
func Test冷却窗口内的denial被忽略(t *testing.T) {
	hub, err := mockhub.Start(mockhub.Options{DenyStateToken: true})
	if err != nil {
		t.Fatalf("起 mock 中台失败: %v", err)
	}
	defer hub.Close()

	plugin, stop := startStatePlugin(t, hub.Addr(), 2*time.Second)
	defer stop()

	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()

	if err := hub.WaitForRegistration(ctx, 1); err != nil {
		t.Fatal(err)
	}
	state := waitForStateClient(t, ctx, plugin)

	// 注册之后立刻敲（此刻距上次注册只有毫秒级，远在冷却窗口内）
	if _, _, err := state.Get(ctx, "s", "k"); status.Code(err) != codes.Unauthenticated {
		t.Fatalf("状态凭证被拒时应把 Unauthenticated 交给插件，实际 %v", err)
	}

	// 给"没有冷却"的实现足够多的时间把第 2 次注册发出来（Register 是本地 RPC，毫秒级）
	time.Sleep(300 * time.Millisecond)

	if got := len(hub.Registrations()); got != 1 {
		t.Fatalf("冷却窗口内的 denial 应被忽略，实际注册了 %d 次（期望 1 次）", got)
	}
}

// Test冷却窗口过后denial仍能触发重注册 是上一条的反面：速率下限该挡的挡、不该挡的不能挡。
//
// 窗口取 200ms 并先睡 400ms 再敲，所以"这次不在窗口内"也是确定的。
func Test冷却窗口过后denial仍能触发重注册(t *testing.T) {
	hub, err := mockhub.Start(mockhub.Options{DenyStateToken: true})
	if err != nil {
		t.Fatalf("起 mock 中台失败: %v", err)
	}
	defer hub.Close()

	plugin, stop := startStatePlugin(t, hub.Addr(), 200*time.Millisecond)
	defer stop()

	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()

	if err := hub.WaitForRegistration(ctx, 1); err != nil {
		t.Fatal(err)
	}
	state := waitForStateClient(t, ctx, plugin)

	time.Sleep(400 * time.Millisecond)

	if _, _, err := state.Get(ctx, "s", "k"); status.Code(err) != codes.Unauthenticated {
		t.Fatalf("状态凭证被拒时应把 Unauthenticated 交给插件，实际 %v", err)
	}

	// 窗口已过：这次 denial 必须被采纳，注册循环要重走注册流程
	if err := hub.WaitForRegistration(ctx, 2); err != nil {
		t.Fatalf("冷却窗口过后的 denial 仍应触发重新注册: %v", err)
	}
}
