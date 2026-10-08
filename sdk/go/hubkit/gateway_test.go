package hubkit_test

import (
	"context"
	"errors"
	"io"
	"log/slog"
	"strings"
	"sync"
	"testing"
	"time"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	"google.golang.org/protobuf/proto"

	"github.com/mahingbun-dev/anc-hub/sdk/go/hubkit"
	"github.com/mahingbun-dev/anc-hub/sdk/go/mockhub"
	"github.com/mahingbun-dev/anc-hub/sdk/go/proto/hubv1"
)

// gwEchoPlugin 是被调方：把收到的 JSON 载荷原样回显，并记下收到的信封供断言。
//
// 记录信封是为了验「中台整备」到底做了什么（subject 覆盖、链追加 caller）——
// 这些发生在 mockhub 一侧，调用方自己的视角看不见。
type gwEchoPlugin struct {
	hubkit.Base

	mu       sync.Mutex
	received []*hubv1.Envelope
}

func (p *gwEchoPlugin) Manifest() *hubv1.PluginManifest {
	return &hubv1.PluginManifest{
		Name:     "echo-test",
		Version:  "1.0.0",
		Consumes: []*hubv1.MessageContract{{FqName: hubkit.StructFQName}},
	}
}

func (p *gwEchoPlugin) Validate(context.Context, *hubv1.Envelope) (*hubv1.ValidateResponse, error) {
	return hubkit.Valid(), nil
}

func (p *gwEchoPlugin) Handle(_ context.Context, env *hubv1.Envelope) (*hubv1.Envelope, error) {
	p.mu.Lock()
	p.received = append(p.received, cloneOf(env))
	p.mu.Unlock()

	payload, _ := hubkit.PayloadJSON(env)
	text, _ := payload["text"].(string)
	return hubkit.WithPayloadJSON(env, map[string]any{"echo": text})
}

// receivedEnvelopes 返回收到的信封快照（副本），供测试断言。
func (p *gwEchoPlugin) receivedEnvelopes() []*hubv1.Envelope {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.received
}

func cloneOf(env *hubv1.Envelope) *hubv1.Envelope { return proto.Clone(env).(*hubv1.Envelope) }

// gwInvokerPlugin 是调用方：实现了 GatewayAware，测试用 [gwInvokerPlugin.invoke]
// 以某个信封为「当前信封」发起互调。
type gwInvokerPlugin struct {
	hubkit.Base

	mu      sync.Mutex
	gateway *hubkit.GatewayClient
}

func (p *gwInvokerPlugin) Manifest() *hubv1.PluginManifest {
	return &hubv1.PluginManifest{Name: "invoker-test", Version: "1.0.0"}
}

func (p *gwInvokerPlugin) Validate(context.Context, *hubv1.Envelope) (*hubv1.ValidateResponse, error) {
	return hubkit.Valid(), nil
}

func (p *gwInvokerPlugin) Handle(ctx context.Context, env *hubv1.Envelope) (*hubv1.Envelope, error) {
	return env, nil
}

func (p *gwInvokerPlugin) SetGateway(g *hubkit.GatewayClient) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.gateway = g
}

func (p *gwInvokerPlugin) gatewayClient() *hubkit.GatewayClient {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.gateway
}

// invokeWith 以 env 为当前信封调用 target——等价于插件在 Handle 里调用
// InvokePlugin 的真实姿势。
func (p *gwInvokerPlugin) invokeWith(ctx context.Context, env *hubv1.Envelope, target string, opts hubkit.InvokeOptions) (*hubv1.Envelope, error) {
	g := p.gatewayClient()
	if g == nil {
		return nil, status.Error(codes.Internal, "GatewayClient 尚未注入")
	}
	opts.CurrentEnvelope = env
	return g.InvokePlugin(ctx, target, opts)
}

// startTestPlugin 起一个插件并等它注册成功。
func startTestPlugin(t *testing.T, hubAddr string, plugin hubkit.Plugin) context.CancelFunc {
	t.Helper()

	addr, err := mockhub.FreeAddr()
	if err != nil {
		t.Fatalf("挑端口失败: %v", err)
	}

	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() {
		defer close(done)
		_ = hubkit.RunContext(ctx, plugin, hubkit.Config{
			HubAddr:       hubAddr,
			AdvertiseAddr: "http://" + addr,
			ListenAddr:    addr,
			RetryInterval: 10 * time.Millisecond,
			Logger:        slog.New(slog.NewTextHandler(io.Discard, nil)),
		})
	}()

	stop := func() {
		cancel()
		<-done
	}
	t.Cleanup(stop)
	return stop
}

// waitGatewayClient 等到调用方插件收到网关客户端注入。
func waitGatewayClient(t *testing.T, ctx context.Context, p *gwInvokerPlugin) *hubkit.GatewayClient {
	t.Helper()

	ticker := time.NewTicker(10 * time.Millisecond)
	defer ticker.Stop()

	for {
		if g := p.gatewayClient(); g != nil {
			return g
		}
		select {
		case <-ctx.Done():
			t.Fatal("实现了 GatewayAware 的插件必须收到网关客户端注入")
			return nil
		case <-ticker.C:
		}
	}
}

// TestInvokePlugin端到端走mock中台 验整条链：调用方以「当前信封」发起 InvokePlugin，
// mock 中台整备后经真实 gRPC 调到被调方，结果一路映射回来。
//
// 断言分两端看：调用方拿到的结果（HANDLED → 信封），以及被调方**实际收到**的信封
// （subject 已被覆盖、链已被追加 caller、trace 从上游贯通）。
func TestInvokePlugin端到端走mock中台(t *testing.T) {
	hub, err := mockhub.Start(mockhub.Options{})
	if err != nil {
		t.Fatalf("起 mock 中台失败: %v", err)
	}
	defer hub.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	callee := &gwEchoPlugin{}
	startTestPlugin(t, hub.Addr(), callee)
	if err := hub.WaitForRegistration(ctx, 1); err != nil {
		t.Fatal(err)
	}

	caller := &gwInvokerPlugin{}
	startTestPlugin(t, hub.Addr(), caller)
	if err := hub.WaitForRegistration(ctx, 2); err != nil {
		t.Fatal(err)
	}
	waitGatewayClient(t, ctx, caller)

	// 模拟「调用方正在处理一条从上游来的消息」：trace/链都是上游带的
	current := &hubv1.Envelope{
		MessageId:  "01J0CURRENT",
		TraceId:    "01J0TRACE01",
		RunId:      "run-7",
		NodeId:     "node-3",
		DeadlineMs: time.Now().Add(10 * time.Second).UnixMilli(),
		Meta:       map[string]string{hubkit.CallChainMeta: "p1,p2"},
	}

	out, err := caller.invokeWith(ctx, current, "echo-test", hubkit.InvokeOptions{
		Timeout:     5 * time.Second,
		PayloadJSON: map[string]any{"text": "你好"},
	})
	if err != nil {
		t.Fatalf("互调失败: %v", err)
	}

	// 调用方视角：拿到被调方的回显载荷
	payload, ok := hubkit.PayloadJSON(out)
	if !ok || payload["echo"] != "你好" {
		t.Fatalf("应拿到被调方的回显，实际 %v (ok=%v)", payload, ok)
	}

	// 被调方视角：信封是中台整备之后的
	got := callee.receivedEnvelopes()
	if len(got) != 1 {
		t.Fatalf("被调方应恰好被调一次，实际 %d 次", len(got))
	}
	env := got[0]
	if env.GetTraceId() != "01J0TRACE01" || env.GetRunId() != "run-7" || env.GetNodeId() != "node-3" {
		t.Fatalf("trace 上下文应从上游贯通: trace=%q run=%q node=%q",
			env.GetTraceId(), env.GetRunId(), env.GetNodeId())
	}
	// mock 的身份反查固定返回 mock-plugin（见 mockhub.authenticate 的注释），
	// 所以链上追加的是它，而不是调用方的注册名
	if gotChain := env.GetMeta()[hubkit.CallChainMeta]; gotChain != "p1,p2,mock-plugin" {
		t.Fatalf("链应是调用方收到的链 + caller，实际 %q", gotChain)
	}
	if subject := env.GetSubject(); subject.GetId() != "mock-plugin" || subject.GetKind() != hubv1.SubjectKind_SUBJECT_KIND_PLUGIN {
		t.Fatalf("subject 应被中台覆盖为 caller，实际 %+v", subject)
	}
	if env.GetType() != hubv1.PayloadType_PAYLOAD_TYPE_REQUEST {
		t.Fatalf("互调信封应标为 REQUEST，实际 %v", env.GetType())
	}
	if env.GetMessageId() == "01J0CURRENT" {
		t.Fatal("message_id 应是新信封的新值，不能复用当前信封的")
	}
	// deadline 应被夹到 now+5s（整体预算 10s 更晚）
	if left := time.Until(time.UnixMilli(env.GetDeadlineMs())); left > 6*time.Second || left < 3*time.Second {
		t.Fatalf("deadline 应夹到本次预算附近，剩余 %v", left)
	}
}

// TestInvokePlugin未注册目标映射为InvokeFailed 验「解析不到目标」是业务结果：
// 调用方拿到的是带 reason 的 *InvokeFailed，而不是 gRPC 错误。
func TestInvokePlugin未注册目标映射为InvokeFailed(t *testing.T) {
	hub, err := mockhub.Start(mockhub.Options{})
	if err != nil {
		t.Fatalf("起 mock 中台失败: %v", err)
	}
	defer hub.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	caller := &gwInvokerPlugin{}
	startTestPlugin(t, hub.Addr(), caller)
	if err := hub.WaitForRegistration(ctx, 1); err != nil {
		t.Fatal(err)
	}
	waitGatewayClient(t, ctx, caller)

	_, err = caller.invokeWith(ctx, hubkit.NewEnvelope(), "ghost", hubkit.InvokeOptions{})
	var failed *hubkit.InvokeFailed
	if !errors.As(err, &failed) {
		t.Fatalf("应映射为 *InvokeFailed，实际 %T: %v", err, err)
	}
	if !strings.Contains(failed.Reason, "未注册") {
		t.Fatalf("reason 应说明目标未注册，实际 %q", failed.Reason)
	}
}

// TestInvokePlugin互调成环被拒 验防环在链路上真实生效：caller 已在链上时
// mock 中台以 outcome=ERROR 拒绝，且被调方根本不会被碰到。
func TestInvokePlugin互调成环被拒(t *testing.T) {
	hub, err := mockhub.Start(mockhub.Options{})
	if err != nil {
		t.Fatalf("起 mock 中台失败: %v", err)
	}
	defer hub.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	callee := &gwEchoPlugin{}
	startTestPlugin(t, hub.Addr(), callee)
	if err := hub.WaitForRegistration(ctx, 1); err != nil {
		t.Fatal(err)
	}

	caller := &gwInvokerPlugin{}
	startTestPlugin(t, hub.Addr(), caller)
	if err := hub.WaitForRegistration(ctx, 2); err != nil {
		t.Fatal(err)
	}
	waitGatewayClient(t, ctx, caller)

	// 链上已经有 mock-plugin（mock 认定的 caller）：再调一次就是 A→B→A 的环
	current := &hubv1.Envelope{
		TraceId: "01J0TRACE02",
		Meta:    map[string]string{hubkit.CallChainMeta: "p1,mock-plugin"},
	}

	_, err = caller.invokeWith(ctx, current, "echo-test", hubkit.InvokeOptions{})
	var failed *hubkit.InvokeFailed
	if !errors.As(err, &failed) {
		t.Fatalf("应映射为 *InvokeFailed，实际 %T: %v", err, err)
	}
	if !strings.Contains(failed.Reason, "互调环") {
		t.Fatalf("reason 应说明是环，实际 %q", failed.Reason)
	}
	if got := len(callee.receivedEnvelopes()); got != 0 {
		t.Fatalf("成环被拒时被调方不该被调到，实际被调 %d 次", got)
	}
}

// Test网关撞401触发重新注册 验 GatewayClient 与 StateClient 共用 denied 通道：
// 网关调用被拒（凭证吊销）同样叫醒注册循环去换新凭证。
func Test网关撞401触发重新注册(t *testing.T) {
	hub, err := mockhub.Start(mockhub.Options{DenyStateToken: true})
	if err != nil {
		t.Fatalf("起 mock 中台失败: %v", err)
	}
	defer hub.Close()

	// 冷却窗口取 100ms（它同时是 denial 的速率下限，见 registrar.heartbeatLoop）
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	caller := &gwInvokerPlugin{}
	startTestPlugin(t, hub.Addr(), caller)
	if err := hub.WaitForRegistration(ctx, 1); err != nil {
		t.Fatal(err)
	}
	waitGatewayClient(t, ctx, caller)

	time.Sleep(200 * time.Millisecond) // 先睡过冷却窗口，让本次 denial 能被采纳

	_, err = caller.invokeWith(ctx, hubkit.NewEnvelope(), "echo-test", hubkit.InvokeOptions{})
	if status.Code(err) != codes.Unauthenticated {
		t.Fatalf("应把 Unauthenticated 交给调用方，实际 %v", err)
	}

	// 关键行为：网关面的 401 也触发了重新注册
	if err := hub.WaitForRegistration(ctx, 2); err != nil {
		t.Fatalf("网关调用撞 401 后应触发重新注册: %v", err)
	}
}

// Test发现三件套走mock中台 验 ListPlugins / DescribeMessage / GetContract
// 能从 mock 的注册表回答「能调谁、谁生产这个消息、它吃什么吐什么」。
func Test发现三件套走mock中台(t *testing.T) {
	hub, err := mockhub.Start(mockhub.Options{})
	if err != nil {
		t.Fatalf("起 mock 中台失败: %v", err)
	}
	defer hub.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	callee := &gwDeclaredPlugin{}
	startTestPlugin(t, hub.Addr(), callee)
	if err := hub.WaitForRegistration(ctx, 1); err != nil {
		t.Fatal(err)
	}

	caller := &gwInvokerPlugin{}
	startTestPlugin(t, hub.Addr(), caller)
	if err := hub.WaitForRegistration(ctx, 2); err != nil {
		t.Fatal(err)
	}
	g := waitGatewayClient(t, ctx, caller)

	// ListPlugins：注册过的插件都能被看到
	plugins, err := g.ListPlugins(ctx, false)
	if err != nil {
		t.Fatalf("ListPlugins 失败: %v", err)
	}
	names := map[string]bool{}
	for _, p := range plugins {
		names[p.GetName()] = true
	}
	if !names["declared-test"] || !names["invoker-test"] {
		t.Fatalf("清单应包含两个已注册插件，实际 %v", names)
	}

	// DescribeMessage：按消息反查生产者（用 well-known 的 Struct 当样本消息——
	// 声明自有类型就得提供 descriptor，这里没必要）
	desc, err := g.DescribeMessage(ctx, hubkit.StructFQName)
	if err != nil {
		t.Fatalf("DescribeMessage 失败: %v", err)
	}
	if len(desc.GetProducers()) != 1 || desc.GetProducers()[0].GetPlugin() != "declared-test" {
		t.Fatalf("生产者应是 declared-test，实际 %v", desc.GetProducers())
	}

	// GetContract：契约、invokes 声明与工具一起返回
	contract, err := g.GetContract(ctx, "declared-test", "", "")
	if err != nil {
		t.Fatalf("GetContract 失败: %v", err)
	}
	if contract.GetVersion() != "1.0.0" {
		t.Fatalf("应取到最新版本，实际 %q", contract.GetVersion())
	}
	if len(contract.GetInvokes()) != 1 || contract.GetInvokes()[0] != "echo-test" {
		t.Fatalf("invokes 应来自 manifest 声明，实际 %v", contract.GetInvokes())
	}
	if len(contract.GetProduces()) != 1 || contract.GetProduces()[0].GetFqName() != hubkit.StructFQName {
		t.Fatalf("produces 应来自 manifest，实际 %v", contract.GetProduces())
	}
	if len(contract.GetTools()) != 1 || contract.GetTools()[0].GetName() != "do-thing" {
		t.Fatalf("tools 应来自 manifest，实际 %v", contract.GetTools())
	}

	// 查无此插件是 gRPC NotFound（基础设施面的「参数错了」，不是业务结果）
	if _, err := g.GetContract(ctx, "ghost", "", ""); status.Code(err) != codes.NotFound {
		t.Fatalf("查无插件应返回 NotFound，实际 %v", err)
	}
}

// gwDeclaredPlugin 带完整声明的插件：produces/consumes、invokes 授权名单与工具，
// 供发现三件套的断言有据可查。
type gwDeclaredPlugin struct {
	hubkit.Base
}

func (p *gwDeclaredPlugin) Manifest() *hubv1.PluginManifest {
	return &hubv1.PluginManifest{
		Name:     "declared-test",
		Version:  "1.0.0",
		Produces: []*hubv1.MessageContract{{FqName: hubkit.StructFQName}},
		Consumes: []*hubv1.MessageContract{{FqName: hubkit.StructFQName}},
		Invokes:  []string{"echo-test"},
		Tools: []*hubv1.ToolDecl{{
			Name:        "do-thing",
			Description: "做一件事",
		}},
	}
}

func (p *gwDeclaredPlugin) Validate(context.Context, *hubv1.Envelope) (*hubv1.ValidateResponse, error) {
	return hubkit.Valid(), nil
}

func (p *gwDeclaredPlugin) Handle(ctx context.Context, env *hubv1.Envelope) (*hubv1.Envelope, error) {
	return env, nil
}
