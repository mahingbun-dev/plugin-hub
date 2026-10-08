package hubkit

import (
	"context"
	"errors"
	"fmt"
	"math"
	"strings"
	"sync"
	"time"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	"google.golang.org/protobuf/proto"

	"github.com/mahingbun-dev/anc-hub/sdk/go/proto/hubv1"
)

// CallChainMeta 是互调链的 meta 键：链上是「已处理过该消息的插件名」，逗号分隔。
//
// 信封的 meta 是唯一的自由携带通道，链只能随它走。事实源是 testdata/hub-rules.json
// 的 gateway.callChainMeta（rules_test.go 钉住两侧一致），与中台
// crates/hub-grpc/src/gateway.rs 的 CALL_CHAIN_META 同名。
//
// 通过 [GatewayClient.InvokePlugin] 发起调用时**不要**自己往链里追加自己：
// 链的语义由中台维护，SDK 只负责把调用方收到的链原样带下去。
const CallChainMeta = "hub.call_chain"

// MaxInvokeDepth 是互调链的长度上限（含本次 caller）。
//
// SDK 侧不主动校验它——链的校验在中台（超了会以 outcome=ERROR 拒绝）——
// 导出它只为 mockhub 与插件自测能对齐同一个数。事实源同 [CallChainMeta]。
const MaxInvokeDepth = 8

// GatewayClient 是插件间发现与互调（PluginGateway 服务）的客户端。
//
// 与 [StateClient] 同款：由 [Run] 在注册成功后注入给实现了 [GatewayAware] 的
// 插件，凭证只有中台知道、且随每次重新注册轮换，所以 token 的读写都过锁；
// 调用撞上 UNAUTHENTICATED 时通过 denied 通道叫醒注册循环去换新凭证。
//
// 与 StateClient 的一点刻意差别：**Invoke 不吃 callTimeout**。状态调用是毫秒级的
// 管理面操作，卡 2 秒就该放弃；互调是业务调用，预算就是信封的 deadline——下游
// 真的处理 20 秒时，客户端先超时等于白让下游白干。所以发现三件套用 callTimeout
// 截断，Invoke / InvokePlugin 的超时完全由 ctx 与信封预算决定。
type GatewayClient struct {
	client hubv1.PluginGatewayClient

	// denied 的语义见 [StateClient.denied]：两者共用注册循环的同一个信号通道，
	// 任何一面撞上 401 都会触发重新注册（中台重启时两面凭证一起换）。
	denied chan struct{}

	// callTimeout 是**发现类**调用的时间上限，来自 [Config.StateCallTimeout]。
	// <= 0 表示不设上限、原样透传 ctx（测试里直接构造的客户端走这条）。
	callTimeout time.Duration

	mu    sync.RWMutex
	token string
}

func (c *GatewayClient) setToken(token string) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.token = token
}

func (c *GatewayClient) currentToken() string {
	c.mu.RLock()
	defer c.mu.RUnlock()
	return c.token
}

// callCtx 给一次发现类调用加上时间上限。语义见 [StateClient.callCtx]。
func (c *GatewayClient) callCtx(ctx context.Context) (context.Context, context.CancelFunc) {
	if c.callTimeout <= 0 {
		return ctx, func() {}
	}
	return context.WithTimeout(ctx, c.callTimeout)
}

// noteDenied 的语义见 [StateClient.noteDenied]：只认 UNAUTHENTICATED，
// 其它错误与凭证无关。
func (c *GatewayClient) noteDenied(err error) {
	if status.Code(err) != codes.Unauthenticated {
		return
	}
	select {
	case c.denied <- struct{}{}:
	default:
	}
}

// GatewayAware 由需要发现/互调能力的插件实现。
//
// 与 [StateAware] 同款的可选接口：老插件一行不改，注入同样发生在**每次**注册
// 成功后（凭证轮换），实现方自己负责并发安全。
type GatewayAware interface {
	// SetGateway 在注册成功后由骨架调用，并可能在每次重新注册后再次被调用。
	SetGateway(*GatewayClient)
}

// ListPlugins 返回插件清单。includeOffline=false 只列在线（有健康实例）的。
//
// 插件用它回答「能调谁」，不要旁路维护硬编码名单——那会跟注册表漂移。
func (c *GatewayClient) ListPlugins(ctx context.Context, includeOffline bool) ([]*hubv1.PluginSummary, error) {
	ctx, cancel := c.callCtx(ctx)
	defer cancel()

	resp, err := c.client.ListPlugins(withStateToken(ctx, c.currentToken()), &hubv1.ListPluginsRequest{
		IncludeOffline: includeOffline,
	})
	if err != nil {
		c.noteDenied(err)
		return nil, err
	}
	return resp.GetPlugins(), nil
}

// DescribeMessage 查一个消息类型由谁生产、由谁消费。
//
// fqName 的口径与 manifest 契约里的 fq_name 一致（如 `wms.v1.OrderCreated`）。
func (c *GatewayClient) DescribeMessage(ctx context.Context, fqName string) (*hubv1.DescribeMessageResponse, error) {
	ctx, cancel := c.callCtx(ctx)
	defer cancel()

	resp, err := c.client.DescribeMessage(withStateToken(ctx, c.currentToken()), &hubv1.DescribeMessageRequest{
		FqName: fqName,
	})
	if err != nil {
		c.noteDenied(err)
		return nil, err
	}
	return resp, nil
}

// GetContract 查一个插件的契约。
//
// version 传空取最新已注册版本；fqName 传空不展开单个消息的字段级 schema。
func (c *GatewayClient) GetContract(ctx context.Context, plugin, version, fqName string) (*hubv1.GetContractResponse, error) {
	ctx, cancel := c.callCtx(ctx)
	defer cancel()

	resp, err := c.client.GetContract(withStateToken(ctx, c.currentToken()), &hubv1.GetContractRequest{
		Plugin:  plugin,
		Version: version,
		FqName:  fqName,
	})
	if err != nil {
		c.noteDenied(err)
		return nil, err
	}
	return resp, nil
}

// Invoke 发起一次同步互调，返回中台的原始应答。
//
// 业务结果（HANDLED/REJECTED/ERROR）全在应答字段里，本方法不做映射——
// 要「HANDLED 给信封、其余给类型化错误」的便捷语义请用 [GatewayClient.InvokePlugin]。
func (c *GatewayClient) Invoke(ctx context.Context, req *hubv1.InvokeRequest) (*hubv1.InvokeResponse, error) {
	resp, err := c.client.Invoke(withStateToken(ctx, c.currentToken()), req)
	if err != nil {
		c.noteDenied(err)
		return nil, err
	}
	return resp, nil
}

// InvokeRejected 表示目标插件的校验器拒绝了这次调用（outcome=REJECTED）。
//
// reason 是中台给的人类可读原因；issues 是结构化的校验问题，path 能定位到
// 具体字段——调用方照着改数据即可，不必去翻下游日志。
type InvokeRejected struct {
	Reason string
	Issues []*hubv1.ValidationIssue
}

func (e *InvokeRejected) Error() string {
	if e.Reason == "" {
		return "hubkit: 互调被目标插件拒绝（未给出原因）"
	}
	return "hubkit: 互调被目标插件拒绝：" + e.Reason
}

// InvokeFailed 表示这次互调被中台判为失败（outcome=ERROR）。
//
// 未声明调用授权、超配额、互调成环、链深超限、下游不可达/出错都属于此类。
// reason 会说明原因：改数据重试没用的（成环、未授权）要改逻辑，下游暂时
// 不可达的才值得退避重试——两类靠 reason 区分，这正是它们不走 gRPC 错误
// 通道的原因。
type InvokeFailed struct {
	Reason string
}

func (e *InvokeFailed) Error() string {
	if e.Reason == "" {
		return "hubkit: 互调失败（未给出原因）"
	}
	return "hubkit: 互调失败：" + e.Reason
}

// InvokeOptions 是 [GatewayClient.InvokePlugin] 的入参。
type InvokeOptions struct {
	// Version 目标插件的版本，空 = 最新已注册版本。
	Version string

	// Timeout 是本次调用的超时预算，0 表示不填——由中台给默认预算兜底。
	//
	// 它会同时落到两处：信封的 deadline（与 CurrentEnvelope 的 deadline 取较早者，
	// 中台侧还会再夹一次）与请求的 timeout_ms。
	Timeout time.Duration

	// CurrentEnvelope 是插件当前正在处理的信封（可空）。
	//
	// 传入它，trace 才能从上游贯通到下游：其 trace_id / run_id / node_id 会被
	// 复制进新信封，meta 里的调用链（[CallChainMeta]）原样带上——**不追加自己**，
	// 中台负责把 caller 记进链。不传则生成全新 trace_id、不带链：那是一次
	// 顶层发起的调用，不是当前处理的延续。
	CurrentEnvelope *hubv1.Envelope

	// PayloadJSON 是 JSON 对象载荷（直接调用场景），与 [PayloadJSON] 同读法。
	// 与 Payload 二选一，都空则发不带载荷的信封。
	PayloadJSON map[string]any

	// Payload 是业务 proto 消息载荷（flow 内部传递语义），type_url 由消息的
	// 全限定名拼出。与 PayloadJSON 二选一。
	Payload proto.Message
}

// InvokePlugin 发起一次同步互调的便捷入口：装配信封（幂等键、trace 上下文、
// 调用链、deadline）、打包载荷、调用中台、把业务结果映射成 Go 的错误类型。
//
// 结果语义：
//   - HANDLED → 返回下游的信封（载荷用 [PayloadJSON] 取）；
//   - REJECTED → *InvokeRejected（reason + issues）；
//   - ERROR → *InvokeFailed（reason）；
//   - 基础设施故障（未鉴权、中台不可达）→ 原样的 gRPC 错误，用 errors.Is/As
//     之外请直接看 status.Code。
func (c *GatewayClient) InvokePlugin(ctx context.Context, plugin string, opts InvokeOptions) (*hubv1.Envelope, error) {
	if strings.TrimSpace(plugin) == "" {
		return nil, errors.New("hubkit: InvokePlugin 缺少目标插件名")
	}
	if opts.PayloadJSON != nil && opts.Payload != nil {
		return nil, errors.New("hubkit: InvokePlugin 的 PayloadJSON 与 Payload 只能二选一")
	}
	if opts.Timeout < 0 {
		return nil, errors.New("hubkit: InvokePlugin 的 Timeout 不能为负")
	}
	if opts.Timeout > math.MaxUint32*time.Millisecond {
		return nil, fmt.Errorf("hubkit: InvokePlugin 的 Timeout 超出线上协议上限（uint32 毫秒）: %s", opts.Timeout)
	}

	env := &hubv1.Envelope{
		// message_id 是幂等键，每次调用都要是新的——哪怕复用同一个 payload
		MessageId: NewULID(),
		Type:      hubv1.PayloadType_PAYLOAD_TYPE_REQUEST,
	}

	var deadlineMs int64
	if cur := opts.CurrentEnvelope; cur != nil {
		env.TraceId = cur.GetTraceId()
		env.RunId = cur.GetRunId()
		env.NodeId = cur.GetNodeId()
		// 链原样复制，不追加自己：链上记的是「已处理过该消息的插件」，
		// 追加 caller 是中台的事，SDK 抢着做会让链上出现重复节点
		if chain := cur.GetMeta()[CallChainMeta]; chain != "" {
			env.Meta = map[string]string{CallChainMeta: chain}
		}
		// 整体预算比本次预算更早时以它为准——调用不该活得比触发它的那次处理更久
		deadlineMs = cur.GetDeadlineMs()
	}
	if env.GetTraceId() == "" {
		env.TraceId = NewULID()
	}
	if opts.Timeout > 0 {
		// 中台会取 min(传入 deadline, now+timeout)，客户端先夹一次能更早放弃
		if ceiling := time.Now().Add(opts.Timeout).UnixMilli(); deadlineMs <= 0 || ceiling < deadlineMs {
			deadlineMs = ceiling
		}
	}
	env.DeadlineMs = deadlineMs

	var err error
	switch {
	case opts.PayloadJSON != nil:
		env, err = WithPayloadJSON(env, opts.PayloadJSON)
	case opts.Payload != nil:
		env, err = WithPayload(env, opts.Payload)
	}
	if err != nil {
		return nil, err
	}

	// 信封预算就是本次 gRPC 调用的预算：到点客户端先放弃，而不是等中台或下游超时
	if d := env.GetDeadlineMs(); d > 0 && time.UnixMilli(d).After(time.Now()) {
		var cancel context.CancelFunc
		ctx, cancel = context.WithDeadline(ctx, time.UnixMilli(d))
		defer cancel()
	}

	resp, err := c.Invoke(ctx, &hubv1.InvokeRequest{
		Plugin:    plugin,
		Version:   opts.Version,
		Envelope:  env,
		TimeoutMs: uint32(opts.Timeout / time.Millisecond),
	})
	if err != nil {
		return nil, err
	}

	switch resp.GetOutcome() {
	case hubv1.InvokeOutcome_HANDLED:
		if resp.GetEnvelope() == nil {
			return nil, errors.New("hubkit: 中台报告 HANDLED 但未返回信封")
		}
		return resp.GetEnvelope(), nil
	case hubv1.InvokeOutcome_REJECTED:
		return nil, &InvokeRejected{Reason: resp.GetReason(), Issues: resp.GetIssues()}
	case hubv1.InvokeOutcome_ERROR:
		return nil, &InvokeFailed{Reason: resp.GetReason()}
	default:
		return nil, fmt.Errorf("hubkit: 中台返回了未知的结果分类 %d", resp.GetOutcome())
	}
}
