package hubkit

import (
	"context"
	"sync"
	"time"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"

	"github.com/mahingbun-dev/plugin-hub/sdk/go/proto/hubv1"
)

// StateTokenMetadata 是状态凭证的 metadata 键，必须与中台侧一致
// （crates/hub-grpc/src/state.rs 的 STATE_TOKEN_METADATA）。
//
// 导出它而不是让各处各写一份字面量：写错的话中台会判成「无凭证」，
// 而插件侧只会看到一个 401，很难查。mockhub 与契约测试都用这一个。
const StateTokenMetadata = "x-hub-state-token"

// StateEntry 是扫描返回的一项。
type StateEntry struct {
	Key   string
	Value []byte
}

// StateClient 是中台外置状态（HubState）的客户端。
//
// 插件被强制无状态：实例内存不保证跨调用保留，需要跨调用保留的东西走这里。
// 客户端由 [Run] 在注册成功后注入给实现了 [StateAware] 的插件——插件作者不该
// 自己构造它，凭证只有中台知道。
//
// **键前缀不含版本号**：中台把键拼成 `hub:state:{插件名}:{namespace}:{key}`，
// 同一插件的**所有版本共用一个状态空间**。升版本不会清空状态（对登录缓存这类状态
// 正是要的），但两个版本往同一个 namespace 写就是**互相覆盖**。要按版本隔离，
// 请自己把版本写进 namespace。
//
// 凭证会被重新注册轮换（中台重启、实例被摘除后自愈），而注册循环跑在另一个
// goroutine 上，所以凭证的读写都要过锁。
type StateClient struct {
	client hubv1.HubStateClient

	// denied 在任一次调用撞上 UNAUTHENTICATED 时收到一个信号，注册循环据此重走
	// 注册流程（中台重启会换凭证、实例被摘除后旧凭证即失效）。
	//
	// 缓冲 1 + 非阻塞发送：并发调用一起撞上 401 时只留一个信号，不会把注册循环
	// 叫成风暴。为 nil 时（测试里自造的客户端）静默丢弃。
	denied chan struct{}

	// callTimeout 是单次调用的时间上限，由 [RunContext] 从 [Config.StateCallTimeout] 注入。
	// <= 0 表示不设上限、原样透传 ctx（测试里直接构造的客户端走这条）。
	callTimeout time.Duration

	mu    sync.RWMutex
	token string
}

func (c *StateClient) setToken(token string) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.token = token
}

func (c *StateClient) currentToken() string {
	c.mu.RLock()
	defer c.mu.RUnlock()
	return c.token
}

// callCtx 给一次状态调用加上时间上限。
//
// 语义全部由 context.WithTimeout 保证，不手写 min 逻辑：父 ctx 的 deadline 更早时
// 取更早的那个（不延长），父 ctx 无 deadline 时用 callTimeout 截断，父 ctx 已过期
// 或被取消时派生的 ctx 立刻失效。
//
// 超时产生的是 DeadlineExceeded 而不是 UNAUTHENTICATED，所以 noteDenied 不会被触发，
// 注册循环不会被中台的一次卡顿叫醒。
func (c *StateClient) callCtx(ctx context.Context) (context.Context, context.CancelFunc) {
	if c.callTimeout <= 0 {
		return ctx, func() {}
	}
	return context.WithTimeout(ctx, c.callTimeout)
}

// noteDenied 在凭证被中台拒绝时叫醒注册循环去换一张新凭证。
//
// 只认 UNAUTHENTICATED：其它错误（网络抖动、参数非法）与凭证无关，重注册解决不了，
// 反而会让循环空转。错误仍原样返回给调用方——重注册是后台的补救，不该掩盖这一次失败。
func (c *StateClient) noteDenied(err error) {
	if status.Code(err) != codes.Unauthenticated {
		return
	}
	select {
	case c.denied <- struct{}{}:
	default:
		// 已经有一个待处理的信号，重复的丢掉即可
	}
}

// StateAware 由需要外置状态的插件实现。
//
// 用可选接口而不是往 [Plugin] 里加方法：老插件一行不用改，新插件想用才实现。
type StateAware interface {
	// SetState 在注册成功后由骨架调用，并可能在每次重新注册后再次被调用
	// （中台重启、实例被摘除后自愈）。
	//
	// 骨架从注册循环那个 goroutine 调用它，而插件的 handler 通常跑在别的 goroutine 上，
	// 所以**同步是实现方的责任**：用互斥锁护住那个字段，或存进 atomic.Pointer[StateClient]。
	// 裸赋值给一个会被其它 goroutine 读取的字段就是数据竞争（`go test -race` 会报）。
	SetState(*StateClient)
}

// Get 读取一个键。found 区分"键不存在"与"值是空字节"。
func (c *StateClient) Get(ctx context.Context, namespace, key string) (value []byte, found bool, err error) {
	ctx, cancel := c.callCtx(ctx)
	defer cancel()

	resp, err := c.client.KvGet(withStateToken(ctx, c.currentToken()), &hubv1.KvGetRequest{
		Key: &hubv1.KvKey{Namespace: namespace, Key: key},
	})
	if err != nil {
		c.noteDenied(err)
		return nil, false, err
	}
	return resp.GetValue(), resp.GetFound(), nil
}

// Put 写入一个键。ttl 为 0 表示不过期。
//
// 注意：TTL 的粒度是秒（线协议是 ttl_seconds），不足 1 秒的 ttl 会被截断为 0，
// 也就是**永不过期**。想让键很快消失，请传 >= 1s 的值。
func (c *StateClient) Put(ctx context.Context, namespace, key string, value []byte, ttl time.Duration) error {
	ctx, cancel := c.callCtx(ctx)
	defer cancel()

	_, err := c.client.KvPut(withStateToken(ctx, c.currentToken()), &hubv1.KvPutRequest{
		Key:        &hubv1.KvKey{Namespace: namespace, Key: key},
		Value:      value,
		TtlSeconds: int64(ttl / time.Second),
	})
	if err != nil {
		c.noteDenied(err)
	}
	return err
}

// Delete 删除一个键。删不存在的键返回 (false, nil)，不是错误。
func (c *StateClient) Delete(ctx context.Context, namespace, key string) (bool, error) {
	ctx, cancel := c.callCtx(ctx)
	defer cancel()

	resp, err := c.client.KvDelete(withStateToken(ctx, c.currentToken()), &hubv1.KvDeleteRequest{
		Key: &hubv1.KvKey{Namespace: namespace, Key: key},
	})
	if err != nil {
		c.noteDenied(err)
		return false, err
	}
	return resp.GetDeleted(), nil
}

// Scan 扫描一个命名空间下前缀匹配的键，上限 limit（服务端另有硬上限）。
func (c *StateClient) Scan(ctx context.Context, namespace, prefix string, limit uint32) ([]StateEntry, error) {
	ctx, cancel := c.callCtx(ctx)
	defer cancel()

	resp, err := c.client.KvScan(withStateToken(ctx, c.currentToken()), &hubv1.KvScanRequest{
		Namespace: namespace,
		Prefix:    prefix,
		Limit:     limit,
	})
	if err != nil {
		c.noteDenied(err)
		return nil, err
	}
	entries := make([]StateEntry, 0, len(resp.GetEntries()))
	for _, e := range resp.GetEntries() {
		entries = append(entries, StateEntry{Key: e.GetKey(), Value: e.GetValue()})
	}
	return entries, nil
}

// PublishRejected 表示中台没有受理一次 Publish（accepted=false）。
//
// 这是**业务结果**而不是网络故障：防环、超限、目标 flow 不存在都属于此类，
// reason 会说明原因。调用方该据此改逻辑或换目标，而不是退避重试——重试是
// 给「中台/总线坏了」（gRPC 错误）准备的，两者被中台刻意分在两个通道里。
type PublishRejected struct {
	Reason string
}

func (e *PublishRejected) Error() string {
	if e.Reason == "" {
		return "hubkit: 中台未受理 Publish（未给出原因）"
	}
	return "hubkit: 中台未受理 Publish：" + e.Reason
}

// Publish 把一条信封投递到目标 flow / topic，异步触发、拿不到业务结果。
//
// 这是插件间**异步**协作的通道；需要下游处理结果的同步场景走
// [GatewayClient.InvokePlugin]。信封用 [NewEnvelope] 起步、载荷用
// [WithPayloadJSON] / [WithPayload] 装；subject 由中台覆盖为调用方身份，
// 自己填了也没用。
//
// 受理成功返回中台分配的 run_id（本次触发的 flow 执行标识）；未受理返回
// *PublishRejected，reason 随错误携带。
func (c *StateClient) Publish(ctx context.Context, target string, env *hubv1.Envelope) (string, error) {
	ctx, cancel := c.callCtx(ctx)
	defer cancel()

	resp, err := c.client.Publish(withStateToken(ctx, c.currentToken()), &hubv1.PublishRequest{
		Target:   target,
		Envelope: env,
	})
	if err != nil {
		c.noteDenied(err)
		return "", err
	}
	if !resp.GetAccepted() {
		return "", &PublishRejected{Reason: resp.GetReason()}
	}
	return resp.GetRunId(), nil
}

func withStateToken(ctx context.Context, token string) context.Context {
	return metadata.AppendToOutgoingContext(ctx, StateTokenMetadata, token)
}
