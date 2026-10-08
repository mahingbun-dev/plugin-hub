package hubkit

import (
	"context"
	"errors"
	"strings"
	"sync"
	"testing"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"

	"github.com/mahingbun-dev/anc-hub/sdk/go/proto/hubv1"
)

// recordingGatewayClient 记录最近一次 Invoke 收到的请求与凭证，并按预设返回。
//
// 用它而不是 mockhub：InvokePlugin 的信封装配（链复制、trace 复用、deadline 夹紧）
// 是客户端自己的逻辑，需要对「发出去的请求」逐字段断言——mockhub 是在链路另一端，
// 看到的是中台整备之后的信封，验不出客户端这边的行为。
type recordingGatewayClient struct {
	mu         sync.Mutex
	token      string
	invokeReq  *hubv1.InvokeRequest
	invokeResp *hubv1.InvokeResponse
	invokeErr  error
}

func (c *recordingGatewayClient) Invoke(ctx context.Context, req *hubv1.InvokeRequest, _ ...grpc.CallOption) (*hubv1.InvokeResponse, error) {
	c.mu.Lock()
	c.invokeReq = req
	c.token = tokenOf(ctx)
	resp, err := c.invokeResp, c.invokeErr
	c.mu.Unlock()
	return resp, err
}

func (c *recordingGatewayClient) ListPlugins(context.Context, *hubv1.ListPluginsRequest, ...grpc.CallOption) (*hubv1.ListPluginsResponse, error) {
	return &hubv1.ListPluginsResponse{}, nil
}

func (c *recordingGatewayClient) DescribeMessage(context.Context, *hubv1.DescribeMessageRequest, ...grpc.CallOption) (*hubv1.DescribeMessageResponse, error) {
	return &hubv1.DescribeMessageResponse{}, nil
}

func (c *recordingGatewayClient) GetContract(context.Context, *hubv1.GetContractRequest, ...grpc.CallOption) (*hubv1.GetContractResponse, error) {
	return &hubv1.GetContractResponse{}, nil
}

// lastInvoke 取出最近一次 Invoke 的请求与凭证快照。
func (c *recordingGatewayClient) lastInvoke() (*hubv1.InvokeRequest, string) {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.invokeReq, c.token
}

func tokenOf(ctx context.Context) string {
	md, _ := metadata.FromOutgoingContext(ctx)
	if values := md.Get(StateTokenMetadata); len(values) > 0 {
		return values[0]
	}
	return ""
}

// handledFake 构造一个应答 HANDLED 的假客户端：信封装配用例只关心发出去的请求，
// 但 InvokePlugin 会把应答映射完才返回，所以给一个最小的合法 HANDLED。
func handledFake() *recordingGatewayClient {
	return &recordingGatewayClient{invokeResp: &hubv1.InvokeResponse{
		Outcome:  hubv1.InvokeOutcome_HANDLED,
		Envelope: &hubv1.Envelope{MessageId: "01J0HANDLED"},
	}}
}

// TestInvokePlugin复制当前信封的trace上下文 验「传入 current_envelope」的契约：
// trace/run/node 复制、链原样带上（不追加自己）、message_id 换新、deadline 取
// 整体预算与本次预算的较早者。
func TestInvokePlugin复制当前信封的trace上下文(t *testing.T) {
	fake := handledFake()
	c := &GatewayClient{client: fake, token: "tok-1"}

	current := &hubv1.Envelope{
		MessageId:  "01J0OLD",
		TraceId:    "01J0TRACE",
		RunId:      "run-7",
		NodeId:     "node-3",
		DeadlineMs: time.Now().Add(time.Hour).UnixMilli(), // 整体预算很远，应被本次预算夹紧
		Meta:       map[string]string{CallChainMeta: "p1,p2"},
	}

	before := time.Now()
	env, err := c.InvokePlugin(context.Background(), "target", InvokeOptions{
		Timeout:         5 * time.Second,
		CurrentEnvelope: current,
		PayloadJSON:     map[string]any{"text": "你好"},
	})
	if err != nil {
		t.Fatalf("InvokePlugin 失败: %v", err)
	}

	req, token := fake.lastInvoke()
	if token != "tok-1" {
		t.Fatalf("凭证必须走 %s metadata，实际 %q", StateTokenMetadata, token)
	}
	if req.GetPlugin() != "target" || req.GetTimeoutMs() != 5000 {
		t.Fatalf("请求字段不对: %+v", req)
	}

	sent := req.GetEnvelope()
	if sent.GetTraceId() != "01J0TRACE" || sent.GetRunId() != "run-7" || sent.GetNodeId() != "node-3" {
		t.Fatalf("trace 上下文没复制全: trace=%q run=%q node=%q",
			sent.GetTraceId(), sent.GetRunId(), sent.GetNodeId())
	}
	// 链是「原样复制」：多一个自己或少一段都算错——追加 caller 是中台的事
	if got := sent.GetMeta()[CallChainMeta]; got != "p1,p2" {
		t.Fatalf("链应原样复制为 %q，实际 %q", "p1,p2", got)
	}
	if len(sent.GetMeta()) != 1 {
		t.Fatalf("meta 里不该有链以外的键: %v", sent.GetMeta())
	}
	if sent.GetMessageId() == "" || sent.GetMessageId() == "01J0OLD" {
		t.Fatalf("message_id 应换新，实际 %q", sent.GetMessageId())
	}
	if sent.GetType() != hubv1.PayloadType_PAYLOAD_TYPE_REQUEST {
		t.Fatalf("互调的信封应标为 REQUEST，实际 %v", sent.GetType())
	}

	// deadline 取 min(整体预算, now+5s)：整体预算在一小时后，所以应落在 5s 附近
	deadline := time.UnixMilli(sent.GetDeadlineMs())
	if deadline.Before(before.Add(4*time.Second)) || deadline.After(before.Add(6*time.Second)) {
		t.Fatalf("deadline 应夹到本次预算（%s 附近），实际 %s", before.Add(5*time.Second), deadline)
	}
	if env.GetMessageId() != "01J0HANDLED" {
		t.Fatalf("应返回下游的信封，实际 %+v", env)
	}
}

// TestInvokePlugin整体预算更早时以其为准 验夹紧的另一边：本次预算不允许把
// 整体 deadline 往后顶。
func TestInvokePlugin整体预算更早时以其为准(t *testing.T) {
	fake := handledFake()
	c := &GatewayClient{client: fake}

	soon := time.Now().Add(2 * time.Second)
	if _, err := c.InvokePlugin(context.Background(), "target", InvokeOptions{
		Timeout:         30 * time.Second,
		CurrentEnvelope: &hubv1.Envelope{TraceId: "t", DeadlineMs: soon.UnixMilli()},
	}); err != nil {
		t.Fatalf("InvokePlugin 失败: %v", err)
	}

	req, _ := fake.lastInvoke()
	if got := time.UnixMilli(req.GetEnvelope().GetDeadlineMs()); got.After(soon.Add(time.Second)) {
		t.Fatalf("deadline 不该被本次预算顶到整体预算之后：整体 %s，实际 %s", soon, got)
	}
}

// TestInvokePlugin无当前信封时新开trace 验顶层直调的分支：新 trace_id、不带链。
func TestInvokePlugin无当前信封时新开trace(t *testing.T) {
	fake := handledFake()
	c := &GatewayClient{client: fake}

	if _, err := c.InvokePlugin(context.Background(), "target", InvokeOptions{
		PayloadJSON: map[string]any{"n": 1.0},
	}); err != nil {
		t.Fatalf("InvokePlugin 失败: %v", err)
	}

	req, _ := fake.lastInvoke()
	sent := req.GetEnvelope()
	if got := len(sent.GetTraceId()); got != 26 {
		t.Fatalf("新 trace_id 应是 26 字符的 ULID，实际 %q（%d 字符）", sent.GetTraceId(), got)
	}
	if _, ok := sent.GetMeta()[CallChainMeta]; ok {
		t.Fatal("顶层直调不该带调用链")
	}
	if sent.GetRunId() != "" || sent.GetNodeId() != "" {
		t.Fatalf("没有当前信封时 run/node 不该有值: %q %q", sent.GetRunId(), sent.GetNodeId())
	}
}

// TestInvokePlugin结果映射 验 HANDLED / REJECTED / ERROR 三类业务结果到
// Go 错误类型的映射，reason 与 issues 必须随错误携带。
func TestInvokePlugin结果映射(t *testing.T) {
	env := &hubv1.Envelope{MessageId: "01J0OUT", Payload: nil}

	t.Run("HANDLED返回下游信封", func(t *testing.T) {
		fake := &recordingGatewayClient{invokeResp: &hubv1.InvokeResponse{
			Outcome:  hubv1.InvokeOutcome_HANDLED,
			Envelope: env,
		}}
		c := &GatewayClient{client: fake}

		got, err := c.InvokePlugin(context.Background(), "target", InvokeOptions{})
		if err != nil {
			t.Fatalf("HANDLED 不该是错误: %v", err)
		}
		if got.GetMessageId() != "01J0OUT" {
			t.Fatalf("应原样返回下游信封，实际 %+v", got)
		}
	})

	t.Run("REJECTED带issues", func(t *testing.T) {
		fake := &recordingGatewayClient{invokeResp: &hubv1.InvokeResponse{
			Outcome: hubv1.InvokeOutcome_REJECTED,
			Reason:  "目标插件校验未通过，见 issues",
			Issues:  []*hubv1.ValidationIssue{{Path: "payload.sku", Message: "缺少必填字段"}},
		}}
		c := &GatewayClient{client: fake}

		_, err := c.InvokePlugin(context.Background(), "target", InvokeOptions{})
		var rejected *InvokeRejected
		if !errors.As(err, &rejected) {
			t.Fatalf("REJECTED 应映射为 *InvokeRejected，实际 %T: %v", err, err)
		}
		if len(rejected.Issues) != 1 || rejected.Issues[0].GetPath() != "payload.sku" {
			t.Fatalf("issues 应随错误携带: %+v", rejected.Issues)
		}
		if rejected.Reason == "" {
			t.Fatal("reason 应随错误携带")
		}
	})

	t.Run("ERROR带reason", func(t *testing.T) {
		fake := &recordingGatewayClient{invokeResp: &hubv1.InvokeResponse{
			Outcome: hubv1.InvokeOutcome_ERROR,
			Reason:  "未声明对 target 的调用授权",
		}}
		c := &GatewayClient{client: fake}

		_, err := c.InvokePlugin(context.Background(), "target", InvokeOptions{})
		var failed *InvokeFailed
		if !errors.As(err, &failed) {
			t.Fatalf("ERROR 应映射为 *InvokeFailed，实际 %T: %v", err, err)
		}
		if failed.Reason != "未声明对 target 的调用授权" {
			t.Fatalf("reason 应随错误携带，实际 %q", failed.Reason)
		}
	})
}

// TestInvokePlugin基础设施故障原样透传 验 gRPC 层错误不被吞掉也不被改写：
// 未鉴权就该是 Unauthenticated，调用方才知道该等凭证轮换而不是改数据。
func TestInvokePlugin基础设施故障原样透传(t *testing.T) {
	fake := &recordingGatewayClient{invokeErr: status.Error(codes.Unauthenticated, "状态凭证无效或已失效")}
	c := &GatewayClient{client: fake, denied: make(chan struct{}, 1)}

	_, err := c.InvokePlugin(context.Background(), "target", InvokeOptions{})
	if status.Code(err) != codes.Unauthenticated {
		t.Fatalf("应原样透传 Unauthenticated，实际 %v", err)
	}
	select {
	case <-c.denied:
	default:
		t.Fatal("401 应触发 denied 信号去换新凭证")
	}
}

// TestInvokePlugin参数防线 验两个必查的入参错误。
func TestInvokePlugin参数防线(t *testing.T) {
	c := &GatewayClient{client: &recordingGatewayClient{}}

	if _, err := c.InvokePlugin(context.Background(), "  ", InvokeOptions{}); err == nil {
		t.Fatal("目标插件名为空白时应报错")
	}
	if _, err := c.InvokePlugin(context.Background(), "t", InvokeOptions{
		PayloadJSON: map[string]any{},
		Payload:     &hubv1.Envelope{},
	}); err == nil {
		t.Fatal("两种载荷同时给时应报错")
	}
}

// recordingStateClient 只为 Publish 封装服务：其余四个方法不会走到，给零值应答。
type recordingStateClient struct {
	mu      sync.Mutex
	token   string
	pubReq  *hubv1.PublishRequest
	pubResp *hubv1.PublishResponse
	pubErr  error
}

func (c *recordingStateClient) Publish(ctx context.Context, req *hubv1.PublishRequest, _ ...grpc.CallOption) (*hubv1.PublishResponse, error) {
	c.mu.Lock()
	c.pubReq = req
	c.token = tokenOf(ctx)
	resp, err := c.pubResp, c.pubErr
	c.mu.Unlock()
	return resp, err
}

func (c *recordingStateClient) KvGet(context.Context, *hubv1.KvGetRequest, ...grpc.CallOption) (*hubv1.KvGetResponse, error) {
	return &hubv1.KvGetResponse{}, nil
}

func (c *recordingStateClient) KvPut(context.Context, *hubv1.KvPutRequest, ...grpc.CallOption) (*hubv1.KvPutResponse, error) {
	return &hubv1.KvPutResponse{}, nil
}

func (c *recordingStateClient) KvDelete(context.Context, *hubv1.KvDeleteRequest, ...grpc.CallOption) (*hubv1.KvDeleteResponse, error) {
	return &hubv1.KvDeleteResponse{}, nil
}

func (c *recordingStateClient) KvScan(context.Context, *hubv1.KvScanRequest, ...grpc.CallOption) (*hubv1.KvScanResponse, error) {
	return &hubv1.KvScanResponse{}, nil
}

func (c *recordingStateClient) lastPublish() (*hubv1.PublishRequest, string) {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.pubReq, c.token
}

// TestPublish封装请求与结果 验 Publish 封装的三件事：请求字段、凭证 metadata、
// accepted=false 时 reason 以类型化错误暴露。
func TestPublish封装请求与结果(t *testing.T) {
	t.Run("受理返回run_id", func(t *testing.T) {
		fake := &recordingStateClient{pubResp: &hubv1.PublishResponse{Accepted: true, RunId: "run-9"}}
		c := &StateClient{client: fake, token: "tok-1"}

		runID, err := c.Publish(context.Background(), "order.flow", &hubv1.Envelope{MessageId: "m-1"})
		if err != nil {
			t.Fatalf("受理不该是错误: %v", err)
		}
		if runID != "run-9" {
			t.Fatalf("应返回中台分配的 run_id，实际 %q", runID)
		}
		req, token := fake.lastPublish()
		if req.GetTarget() != "order.flow" || req.GetEnvelope().GetMessageId() != "m-1" {
			t.Fatalf("请求字段不对: %+v", req)
		}
		if token != "tok-1" {
			t.Fatalf("凭证必须走 %s metadata，实际 %q", StateTokenMetadata, token)
		}
	})

	t.Run("未受理暴露reason", func(t *testing.T) {
		fake := &recordingStateClient{pubResp: &hubv1.PublishResponse{
			Accepted: false,
			Reason:   "检测到环：order.flow 已在本次触发链上（order.flow）",
		}}
		c := &StateClient{client: fake}

		_, err := c.Publish(context.Background(), "order.flow", &hubv1.Envelope{})
		var rejected *PublishRejected
		if !errors.As(err, &rejected) {
			t.Fatalf("accepted=false 应映射为 *PublishRejected，实际 %T: %v", err, err)
		}
		if rejected.Reason == "" {
			t.Fatal("reason 应随错误携带")
		}
	})
}

// TestNewULID形状与唯一性 把 ULID 的对外承诺钉住：26 字符、Crockford 字母表、
// 时间前缀单调、连续生成不重复。
//
// 单调性只对**前 10 个字符**（48 位毫秒时间戳）作保：同一毫秒内生成多个 id 时，
// 高位相同、低位是各自的随机数，整体字符串序不保证递增——这是 ULID 的定义，
// 不是实现的毛病，中台侧的 ulid crate 同样如此。
func TestNewULID形状与唯一性(t *testing.T) {
	seen := make(map[string]bool, 1000)
	var prev string
	for i := 0; i < 1000; i++ {
		id := NewULID()
		if len(id) != 26 {
			t.Fatalf("ULID 应为 26 字符，实际 %q（%d 字符）", id, len(id))
		}
		for j := 0; j < len(id); j++ {
			if !strings.ContainsRune(ulidEncoding, rune(id[j])) {
				t.Fatalf("字符 %q 不在 Crockford 字母表里: %q", id[j], id)
			}
		}
		if prev != "" && strings.Compare(id[:10], prev[:10]) < 0 {
			t.Fatalf("时间前缀应单调递增: %s < %s", id[:10], prev[:10])
		}
		if seen[id] {
			t.Fatalf("连续生成出现重复 id: %s", id)
		}
		seen[id] = true
		prev = id
	}
}

// TestNewEnvelope给全新id 验起步信封的两个 id 都在且互不相同。
func TestNewEnvelope给全新id(t *testing.T) {
	env := NewEnvelope()
	if env.GetMessageId() == "" || env.GetTraceId() == "" {
		t.Fatalf("message_id 与 trace_id 都应生成: %+v", env)
	}
	if env.GetMessageId() == env.GetTraceId() {
		t.Fatal("message_id 与 trace_id 不该是同一个值")
	}
}
