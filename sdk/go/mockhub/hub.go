// Package mockhub 是一个本地 mock 中台。
//
// 用途有二：
//
//   - **离线开发**：插件团队不必等中台就绪，也不必连测试环境就能把插件跑起来。
//   - **自动化测试**：断言「注册请求里到底报了什么」「心跳有没有按时发」「被摘除后有没有
//     重新注册」这类行为。
//
// 它实现插件面（PluginRegistry）与状态面（HubState），不实现中台的其它部分——
// 插件感知不到差别。
//
// 一条纪律：**mock 不该比真中台宽松**。离线的 mock 一旦比真环境宽容，问题就会
// 推迟到上线才爆。所以状态面的认证、键字符集、上限、TTL、错误码都照
// `crates/hub-grpc/src/state.rs` 来。
package mockhub

import (
	"context"
	"fmt"
	"net"
	"slices"
	"sort"
	"strings"
	"sync"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"
	"google.golang.org/protobuf/proto"

	"github.com/mahingbun-dev/plugin-hub/sdk/go/hubkit"
	"github.com/mahingbun-dev/plugin-hub/sdk/go/proto/hubv1"
)

// Options 控制 mock 中台的行为。
type Options struct {
	// HeartbeatIntervalSeconds 中台指定的心跳周期，缺省 1 秒（测试里要快）。
	HeartbeatIntervalSeconds int32

	// RejectRegister 非 nil 时用它来拒绝注册。
	//
	// 用来验证插件侧对拒绝的处理：拒绝原因有没有打全、会不会一直重试。
	RejectRegister func(*hubv1.RegisterRequest) []*hubv1.Rejection

	// ReregisterAfterBeats 大于 0 时，收到这么多拍心跳后开始要求插件重新注册。
	//
	// 用来验证「实例被摘除后插件能自愈」。
	ReregisterAfterBeats int

	// DenyStateToken 为 true 时，状态面的每一次调用都以 Unauthenticated 拒绝，
	// 凭证本身照常在注册响应里下发（模拟"凭证被中台吊销/轮换后旧凭证还在插件手里"）。
	//
	// 用来验证插件侧对 401 的处理——比如会不会退化成 panic 或者静默丢数据。
	// 非 true 时正常校验。
	DenyStateToken bool
}

// Registration 一次被记录下来的注册请求。
type Registration struct {
	Request *hubv1.RegisterRequest
	At      time.Time
}

// Hub 是运行中的 mock 中台。
//
// 这里刻意不嵌 `hubv1.UnimplementedHubStateServer`：嵌了之后，proto 新增方法时
// mock 会**悄悄地**返回 Unimplemented，而"mock 少实现了一个方法"正是最该被
// 编译期拦下的偏差。不嵌就是编译错误，逼着实现者做决定。
type Hub struct {
	opts     Options
	listener net.Listener
	server   *grpc.Server

	mu            sync.Mutex
	registrations []Registration
	heartbeats    int
	// 存整个请求而不只是 instance_id：注销还要看凭证（见 [Hub.UnregisterTokens]）
	unregisters []*hubv1.UnregisterRequest

	// 状态面：一个内存 KV，够插件离线开发与测试用。真中台用 Redis，插件感知不到差别。
	//
	// stateToken 在 Start 时确定、之后不再变更（读它不需要持锁）；
	// stateExpiry 里没有的键表示不过期。
	stateToken  string
	stateKV     map[string][]byte
	stateExpiry map[string]time.Time

	// 网关面：被接受的注册构成插件清单（注册被拒的不算「在线」），以及
	// 互调配额的固定窗口记账。真中台查 PG / Redis，mock 用内存，形状一致。
	registry     map[string]*hubv1.RegisterRequest
	instances    map[string]map[string]struct{}
	invokeWindow uint64
	invokeCounts map[string]int
}

// Start 起一个 mock 中台，监听在随机端口上。
func Start(opts Options) (*Hub, error) {
	if opts.HeartbeatIntervalSeconds <= 0 {
		opts.HeartbeatIntervalSeconds = 1
	}

	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return nil, fmt.Errorf("mockhub: 监听失败: %w", err)
	}

	hub := &Hub{
		opts:         opts,
		listener:     listener,
		server:       grpc.NewServer(),
		stateToken:   mockStateToken,
		stateKV:      map[string][]byte{},
		stateExpiry:  map[string]time.Time{},
		registry:     map[string]*hubv1.RegisterRequest{},
		instances:    map[string]map[string]struct{}{},
		invokeCounts: map[string]int{},
	}
	hubv1.RegisterPluginRegistryServer(hub.server, hub)
	hubv1.RegisterHubStateServer(hub.server, hub)
	hubv1.RegisterPluginGatewayServer(hub.server, hub)

	go func() { _ = hub.server.Serve(listener) }()
	return hub, nil
}

// Addr 返回插件应当连接的中台地址（含 http:// 前缀）。
func (h *Hub) Addr() string {
	return "http://" + h.listener.Addr().String()
}

// Close 停掉 mock 中台。
func (h *Hub) Close() {
	h.server.Stop()
	_ = h.listener.Close()
}

// Registrations 返回收到的全部注册请求（含被拒的）。
func (h *Hub) Registrations() []Registration {
	h.mu.Lock()
	defer h.mu.Unlock()
	return append([]Registration(nil), h.registrations...)
}

// LastRegistration 返回最近一次注册请求；没有则返回 nil。
func (h *Hub) LastRegistration() *hubv1.RegisterRequest {
	h.mu.Lock()
	defer h.mu.Unlock()
	if len(h.registrations) == 0 {
		return nil
	}
	return h.registrations[len(h.registrations)-1].Request
}

// WaitForRegistration 等到至少收到 n 次注册，或 ctx 结束。
func (h *Hub) WaitForRegistration(ctx context.Context, n int) error {
	ticker := time.NewTicker(10 * time.Millisecond)
	defer ticker.Stop()

	for {
		h.mu.Lock()
		count := len(h.registrations)
		h.mu.Unlock()

		if count >= n {
			return nil
		}

		select {
		case <-ctx.Done():
			return fmt.Errorf("mockhub: 等待第 %d 次注册超时（已收到 %d 次）: %w", n, count, ctx.Err())
		case <-ticker.C:
		}
	}
}

// WaitForHeartbeats 等到至少收到 n 拍心跳，或 ctx 结束。
func (h *Hub) WaitForHeartbeats(ctx context.Context, n int) error {
	ticker := time.NewTicker(10 * time.Millisecond)
	defer ticker.Stop()

	for {
		h.mu.Lock()
		count := h.heartbeats
		h.mu.Unlock()

		if count >= n {
			return nil
		}

		select {
		case <-ctx.Done():
			return fmt.Errorf("mockhub: 等待第 %d 拍心跳超时（已收到 %d 拍）: %w", n, count, ctx.Err())
		case <-ticker.C:
		}
	}
}

// Unregisters 返回收到的注销请求里的实例 id。
func (h *Hub) Unregisters() []string {
	h.mu.Lock()
	defer h.mu.Unlock()
	ids := make([]string, 0, len(h.unregisters))
	for _, req := range h.unregisters {
		ids = append(ids, req.GetInstanceId())
	}
	return ids
}

// UnregisterTokens 返回收到的注销请求里携带的状态凭证，与 [Hub.Unregisters] 同序。
//
// 中台要靠它认出「你注销的是不是自己那一行」——`instance_id` 是插件自报的、可以撞，
// 凭证才是属主证明。
func (h *Hub) UnregisterTokens() []string {
	h.mu.Lock()
	defer h.mu.Unlock()
	tokens := make([]string, 0, len(h.unregisters))
	for _, req := range h.unregisters {
		tokens = append(tokens, req.GetStateToken())
	}
	return tokens
}

// StateToken 返回当前下发的状态凭证，供测试断言。
//
// 凭证在 Start 时确定、之后不再变更，所以不必持锁。
func (h *Hub) StateToken() string {
	return h.stateToken
}

// StateKV 返回状态存储的快照，供测试断言。
//
// 键是 mock 内部的复合键（`插件名\x00命名空间\x00键`，插件名固定为
// [mockStatePlugin]）；值是复制出来的，测试改动快照不会污染 mock 内部。
// 已过期的键不在快照里——忽略 TTL 就等于比真中台宽松。
func (h *Hub) StateKV() map[string][]byte {
	h.mu.Lock()
	defer h.mu.Unlock()
	h.purgeExpiredLocked(time.Now())

	snapshot := make(map[string][]byte, len(h.stateKV))
	for key, value := range h.stateKV {
		snapshot[key] = append([]byte(nil), value...)
	}
	return snapshot
}

// FreeAddr 返回一个当前空闲的 127.0.0.1 地址。
//
// 测试里插件要先知道自己的对外地址（中台会连它做可达性探测），而监听端口由系统分配时
// 拿不到真实端口——用它先探一个再显式指定。存在极小的竞态窗口，测试场景可接受。
func FreeAddr() (string, error) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return "", fmt.Errorf("mockhub: 探测空闲端口失败: %w", err)
	}
	addr := listener.Addr().String()
	_ = listener.Close()
	return addr, nil
}

// ---------------------------------------------------------------- 服务端实现

func (h *Hub) Register(_ context.Context, req *hubv1.RegisterRequest) (*hubv1.RegisterResponse, error) {
	h.mu.Lock()
	h.registrations = append(h.registrations, Registration{Request: req, At: time.Now()})
	h.mu.Unlock()

	if h.opts.RejectRegister != nil {
		if rejections := h.opts.RejectRegister(req); len(rejections) > 0 {
			return &hubv1.RegisterResponse{Accepted: false, Rejections: rejections}, nil
		}
	}

	// 通过注册的才进网关面的清单：被拒的插件不该被别人发现、也不该被调到。
	// 同名注册覆盖旧版——mock 与真中台一样只认「每个插件的最新注册」。
	h.mu.Lock()
	h.registry[req.GetPluginName()] = req
	set := h.instances[req.GetPluginName()]
	if set == nil {
		set = map[string]struct{}{}
		h.instances[req.GetPluginName()] = set
	}
	set[req.GetInstanceId()] = struct{}{}
	h.mu.Unlock()

	return &hubv1.RegisterResponse{
		Accepted:                 true,
		InstanceId:               req.GetInstanceId(),
		HeartbeatIntervalSeconds: h.opts.HeartbeatIntervalSeconds,
		StateToken:               h.stateToken,
	}, nil
}

func (h *Hub) Heartbeat(_ context.Context, _ *hubv1.HeartbeatRequest) (*hubv1.HeartbeatResponse, error) {
	h.mu.Lock()
	h.heartbeats++
	beats := h.heartbeats
	h.mu.Unlock()

	requireReregister := h.opts.ReregisterAfterBeats > 0 && beats > h.opts.ReregisterAfterBeats

	return &hubv1.HeartbeatResponse{
		Accepted:                 !requireReregister,
		HeartbeatIntervalSeconds: h.opts.HeartbeatIntervalSeconds,
		ReregisterRequired:       requireReregister,
	}, nil
}

func (h *Hub) Unregister(_ context.Context, req *hubv1.UnregisterRequest) (*hubv1.UnregisterResponse, error) {
	h.mu.Lock()
	h.unregisters = append(h.unregisters, req)
	h.mu.Unlock()
	return &hubv1.UnregisterResponse{}, nil
}

// ---------------------------------------------------------------- 状态面实现

const (
	// maxStateValueBytes 与中台一致：HubState 不是对象存储。
	maxStateValueBytes = 1024 * 1024
	// maxStateScanLimit 与中台一致：防止一次拉走整个命名空间。
	maxStateScanLimit = uint32(1000)
)

const (
	// mockStateToken 是 mock 里固定下发的凭证。
	mockStateToken = "mock-state-token"
	// mockStatePlugin 是 mock 里固定的插件名。
	//
	// 中台是拿凭证查库反查出插件名的；mock 不做那件事——真实的身份反查由
	// Task 4 的集成测试（真中台 + 真 Redis）覆盖。但键里仍然带上插件名，
	// 隔离的形状与中台一致。
	mockStatePlugin = "mock-plugin"
)

// 编译期确认 mock 把状态面实现全了。
var _ hubv1.HubStateServer = (*Hub)(nil)

// stateKeyOf 取出必填的键并校验两端。
func stateKeyOf(key *hubv1.KvKey) (*hubv1.KvKey, error) {
	if key == nil {
		return nil, status.Error(codes.InvalidArgument, "缺少 key")
	}
	if !hubkit.ValidStateSegment(key.GetNamespace()) {
		return nil, status.Errorf(codes.InvalidArgument,
			"namespace 只允许 [A-Za-z0-9_.-] 且非空，收到 %q", key.GetNamespace())
	}
	if !hubkit.ValidStateSegment(key.GetKey()) {
		return nil, status.Errorf(codes.InvalidArgument,
			"key 只允许 [A-Za-z0-9_.-] 且非空，收到 %q", key.GetKey())
	}
	return key, nil
}

// stateKey 拼内存里的存储键，形状与中台的
// `hub:state:{插件}:{命名空间}:{键}` 对齐：插件名在最前面，插件之间天然读不到
// 彼此的数据。分隔符用 \x00 是因为它不在白名单字符里，拼出来不会有歧义。
func (h *Hub) stateKey(plugin, namespace, key string) string {
	return plugin + "\x00" + namespace + "\x00" + key
}

// authenticate 校验凭证并返回插件名。
//
// 顺序与中台一致：**先认证、再校验其余参数**。反过来的话，"没凭证 + 参数非法"的
// 请求会看到与真中台不同的错误码，插件侧的容错就会照着 mock 写错。
func (h *Hub) authenticate(ctx context.Context) (string, error) {
	if h.opts.DenyStateToken {
		return "", status.Error(codes.Unauthenticated, "状态凭证无效或已失效")
	}

	md, _ := metadata.FromIncomingContext(ctx)
	values := md.Get(hubkit.StateTokenMetadata)
	if len(values) == 0 || values[0] == "" {
		return "", status.Error(codes.Unauthenticated, "缺少 "+hubkit.StateTokenMetadata)
	}
	if values[0] != h.stateToken {
		return "", status.Error(codes.Unauthenticated, "状态凭证无效或已失效")
	}

	return mockStatePlugin, nil
}

// purgeExpiredLocked 清掉已过期的键。调用方必须持有 h.mu。
func (h *Hub) purgeExpiredLocked(now time.Time) {
	for key, at := range h.stateExpiry {
		if !now.Before(at) {
			delete(h.stateKV, key)
			delete(h.stateExpiry, key)
		}
	}
}

func (h *Hub) KvGet(ctx context.Context, req *hubv1.KvGetRequest) (*hubv1.KvGetResponse, error) {
	plugin, err := h.authenticate(ctx)
	if err != nil {
		return nil, err
	}
	key, err := stateKeyOf(req.GetKey())
	if err != nil {
		return nil, err
	}

	h.mu.Lock()
	defer h.mu.Unlock()
	h.purgeExpiredLocked(time.Now())

	value, ok := h.stateKV[h.stateKey(plugin, key.GetNamespace(), key.GetKey())]
	if !ok {
		// found=false 与"键在、值是空字节"是两回事，别把它们合并
		return &hubv1.KvGetResponse{}, nil
	}
	return &hubv1.KvGetResponse{Found: true, Value: append([]byte(nil), value...)}, nil
}

func (h *Hub) KvPut(ctx context.Context, req *hubv1.KvPutRequest) (*hubv1.KvPutResponse, error) {
	plugin, err := h.authenticate(ctx)
	if err != nil {
		return nil, err
	}
	key, err := stateKeyOf(req.GetKey())
	if err != nil {
		return nil, err
	}
	if len(req.GetValue()) > maxStateValueBytes {
		return nil, status.Errorf(codes.InvalidArgument,
			"value 超过上限 %d 字节，收到 %d 字节", maxStateValueBytes, len(req.GetValue()))
	}
	if req.GetTtlSeconds() < 0 {
		return nil, status.Error(codes.InvalidArgument, "ttl_seconds 不能为负")
	}

	full := h.stateKey(plugin, key.GetNamespace(), key.GetKey())

	h.mu.Lock()
	defer h.mu.Unlock()
	h.stateKV[full] = append([]byte(nil), req.GetValue()...)
	if ttl := req.GetTtlSeconds(); ttl > 0 {
		h.stateExpiry[full] = time.Now().Add(time.Duration(ttl) * time.Second)
	} else {
		// ttl=0 是"不过期"：覆盖旧值时要把残留的过期时间清掉，否则旧 TTL 会
		// 把新值带走——中台用 SET 覆盖，同样会清掉 TTL
		delete(h.stateExpiry, full)
	}
	return &hubv1.KvPutResponse{}, nil
}

func (h *Hub) KvDelete(ctx context.Context, req *hubv1.KvDeleteRequest) (*hubv1.KvDeleteResponse, error) {
	plugin, err := h.authenticate(ctx)
	if err != nil {
		return nil, err
	}
	key, err := stateKeyOf(req.GetKey())
	if err != nil {
		return nil, err
	}

	full := h.stateKey(plugin, key.GetNamespace(), key.GetKey())

	h.mu.Lock()
	defer h.mu.Unlock()
	h.purgeExpiredLocked(time.Now())

	_, ok := h.stateKV[full]
	delete(h.stateKV, full)
	delete(h.stateExpiry, full)

	// 删不存在的键返回 deleted=false，不是错误：调用方的意图（这键没了）已达成
	return &hubv1.KvDeleteResponse{Deleted: ok}, nil
}

func (h *Hub) KvScan(ctx context.Context, req *hubv1.KvScanRequest) (*hubv1.KvScanResponse, error) {
	plugin, err := h.authenticate(ctx)
	if err != nil {
		return nil, err
	}
	if !hubkit.ValidStateSegment(req.GetNamespace()) {
		return nil, status.Errorf(codes.InvalidArgument,
			"namespace 只允许 [A-Za-z0-9_.-] 且非空，收到 %q", req.GetNamespace())
	}
	// prefix 允许为空（扫整个命名空间），非空时同样只接受白名单字符
	if prefix := req.GetPrefix(); prefix != "" && !hubkit.ValidStateSegment(prefix) {
		return nil, status.Errorf(codes.InvalidArgument,
			"prefix 只允许 [A-Za-z0-9_.-]，收到 %q", prefix)
	}
	if req.GetLimit() == 0 || req.GetLimit() > maxStateScanLimit {
		return nil, status.Errorf(codes.InvalidArgument,
			"limit 必须在 1..=%d 之间，收到 %d", maxStateScanLimit, req.GetLimit())
	}

	// 前缀里含插件名，匹配被限制在本插件的命名空间内
	namespacePrefix := plugin + "\x00" + req.GetNamespace() + "\x00"
	pattern := namespacePrefix + req.GetPrefix()

	h.mu.Lock()
	defer h.mu.Unlock()
	h.purgeExpiredLocked(time.Now())

	// 中台的 SCAN 顺序本就是任意的，这里排序只是让 mock 的行为可复现
	matched := make([]string, 0, len(h.stateKV))
	for full := range h.stateKV {
		if strings.HasPrefix(full, pattern) {
			matched = append(matched, full)
		}
	}
	sort.Strings(matched)

	// limit 是硬上限
	if limit := int(req.GetLimit()); len(matched) > limit {
		matched = matched[:limit]
	}

	entries := make([]*hubv1.KvEntry, 0, len(matched))
	for _, full := range matched {
		entries = append(entries, &hubv1.KvEntry{
			// 对外只暴露插件自己的键名，不带前缀
			Key:   full[len(namespacePrefix):],
			Value: append([]byte(nil), h.stateKV[full]...),
		})
	}
	return &hubv1.KvScanResponse{Entries: entries}, nil
}

// Publish 明确不做——与中台一致（防环与限流是独立课题，见 docs/design.md）。
//
// 这里**不校验凭证**：中台对 Publish 一律直接返回 Unimplemented，mock 若先查凭证，
// 插件就会照着"不带凭证的 Publish 会先撞 401"去写，上真环境才发现不是。
func (h *Hub) Publish(context.Context, *hubv1.PublishRequest) (*hubv1.PublishResponse, error) {
	return nil, status.Error(codes.Unimplemented,
		"Publish 尚未实现：防环与限流是独立课题，见 docs/design.md")
}

// ---------------------------------------------------------------- 网关面实现

// 编译期确认 mock 把网关面实现全了。
var _ hubv1.PluginGatewayServer = (*Hub)(nil)

const (
	// invokeQuotaPerMinute 与中台一致：crates/hub-grpc/src/gateway.rs 的
	// INVOKE_QUOTA_PER_MINUTE，事实源是 hub-rules.json 的 gateway.invokeQuotaPerMinute。
	invokeQuotaPerMinute = 60

	// defaultInvokeTimeoutMs 与中台一致：timeout_ms=0 是「没填」而不是「不等结果」。
	defaultInvokeTimeoutMs = 30_000
)

// latestVersionOf 取某插件最新注册的版本号；从未注册成功返回空。调用方必须持有 h.mu。
func (h *Hub) latestVersionOf(plugin string) string {
	return h.registry[plugin].GetVersion()
}

// registeredNames 列出注册成功的插件名。排序是为了让 mock 的行为可复现——
// 真中台的返回顺序同样不作承诺。
func (h *Hub) registeredNames() []string {
	names := make([]string, 0, len(h.registry))
	for name := range h.registry {
		names = append(names, name)
	}
	sort.Strings(names)
	return names
}

// callChainOf 解出信封 meta 里的互调链。空段必须滤掉：没设这个键时它可能是
// 空串，空段会变成一个「叫空字符串的插件」参与环检测——脏数据不该有语义。
// 与中台 gateway.rs 的 call_chain 同规则。
func callChainOf(meta map[string]string) []string {
	var chain []string
	for _, seg := range strings.Split(meta[hubkit.CallChainMeta], ",") {
		if seg = strings.TrimSpace(seg); seg != "" {
			chain = append(chain, seg)
		}
	}
	return chain
}

// clampDeadline 与中台 gateway.rs 的 clamp_deadline 同规则：
// deadline 取 min(传入, now+timeout)；传入缺失（<=0）或已过期时用 now+timeout
// 重新起算——沿用一个已过期的 deadline 会让调用瞬间超时；timeout=0 是「没填」，
// 给 defaultInvokeTimeoutMs 兜底。
func clampDeadline(requestedMs int64, timeoutMs uint32, nowMs int64) int64 {
	budget := int64(timeoutMs)
	if budget == 0 {
		budget = defaultInvokeTimeoutMs
	}
	ceiling := nowMs + budget
	if requestedMs <= 0 || requestedMs < nowMs {
		return ceiling
	}
	return min(requestedMs, ceiling)
}

func (h *Hub) ListPlugins(ctx context.Context, req *hubv1.ListPluginsRequest) (*hubv1.ListPluginsResponse, error) {
	if _, err := h.authenticate(ctx); err != nil {
		return nil, err
	}

	h.mu.Lock()
	defer h.mu.Unlock()

	plugins := make([]*hubv1.PluginSummary, 0, len(h.registry))
	for _, name := range h.registeredNames() {
		// mock 不做健康探测：注册过即在线。真中台的 instance_count 统计
		// status='healthy' 的实例，被摘除的实例会在那里消失——这一层 mock 不追。
		online := len(h.instances[name]) > 0
		if !req.GetIncludeOffline() && !online {
			continue
		}
		plugins = append(plugins, &hubv1.PluginSummary{
			Name:          name,
			LatestVersion: h.latestVersionOf(name),
			Online:        online,
			InstanceCount: uint32(len(h.instances[name])),
			Description:   h.registry[name].GetManifest().GetDescription(),
		})
	}
	return &hubv1.ListPluginsResponse{Plugins: plugins}, nil
}

func (h *Hub) DescribeMessage(ctx context.Context, req *hubv1.DescribeMessageRequest) (*hubv1.DescribeMessageResponse, error) {
	if _, err := h.authenticate(ctx); err != nil {
		return nil, err
	}

	h.mu.Lock()
	defer h.mu.Unlock()

	resp := &hubv1.DescribeMessageResponse{}
	for _, name := range h.registeredNames() {
		version := h.latestVersionOf(name)
		manifest := h.registry[name].GetManifest()
		for _, c := range manifest.GetProduces() {
			if c.GetFqName() == req.GetFqName() {
				resp.Producers = append(resp.Producers, &hubv1.MessageEndpoint{Plugin: name, Version: version})
			}
		}
		for _, c := range manifest.GetConsumes() {
			if c.GetFqName() == req.GetFqName() {
				resp.Consumers = append(resp.Consumers, &hubv1.MessageEndpoint{Plugin: name, Version: version})
			}
		}
	}
	return resp, nil
}

func (h *Hub) GetContract(ctx context.Context, req *hubv1.GetContractRequest) (*hubv1.GetContractResponse, error) {
	if _, err := h.authenticate(ctx); err != nil {
		return nil, err
	}
	name := strings.TrimSpace(req.GetPlugin())
	if name == "" {
		return nil, status.Error(codes.InvalidArgument, "plugin 不能为空")
	}

	h.mu.Lock()
	defer h.mu.Unlock()

	reg, ok := h.registry[name]
	if !ok {
		return nil, status.Errorf(codes.NotFound, "插件 %s 未注册", name)
	}
	if v := req.GetVersion(); v != "" && reg.GetVersion() != v {
		return nil, status.Errorf(codes.NotFound, "插件 %s 没有版本 %s", name, v)
	}
	manifest := reg.GetManifest()
	return &hubv1.GetContractResponse{
		Name:     name,
		Version:  reg.GetVersion(),
		Produces: manifest.GetProduces(),
		Consumes: manifest.GetConsumes(),
		Invokes:  manifest.GetInvokes(),
		Tools:    manifest.GetTools(),
		// schema_json 要拿 descriptor 做字段级摊平，mock 不复刻那套——
		// 依赖它的行为请连真中台验证。
	}, nil
}

// Invoke 是同步互调的替身：鉴权 → 配额 → 链校验 → 解析目标 → 整备信封 →
// Validate → Handle，与真中台 gateway.rs 的流程同序、规则同源（常量与判定
// 一律引用 hubkit 与本文件的镜像值）。业务结果走 outcome + reason，
// 只有基础设施故障才回 gRPC 错误——这两条纪律 mock 同样不放宽。
//
// 与真中台的一处已知差异：身份反查。真中台拿凭证查 PG 得到 caller 插件名，
// mock 里凭证持有者一律是 [mockStatePlugin]（与状态面同一取舍，见其注释）。
func (h *Hub) Invoke(ctx context.Context, req *hubv1.InvokeRequest) (*hubv1.InvokeResponse, error) {
	caller, err := h.authenticate(ctx)
	if err != nil {
		return nil, err
	}
	if req.GetPlugin() == "" {
		return nil, status.Error(codes.InvalidArgument, "plugin 不能为空")
	}
	if req.GetEnvelope() == nil {
		return nil, status.Error(codes.InvalidArgument, "缺少信封")
	}

	started := time.Now()
	elapsed := func() uint64 { return uint64(time.Since(started).Milliseconds()) }
	// 业务结果（超限/成环/链深/解析不到目标/下游失败）一律走 outcome=ERROR，
	// 绝不悄悄变成 gRPC Status——与真中台同一分工
	invokeError := func(reason string) *hubv1.InvokeResponse {
		return &hubv1.InvokeResponse{
			Outcome:   hubv1.InvokeOutcome_ERROR,
			Reason:    reason,
			ElapsedMs: elapsed(),
		}
	}

	h.mu.Lock()
	// ---- 配额：每插件每分钟固定窗口，与状态面的配额同一形状 ----
	window := uint64(time.Now().Unix() / 60)
	if window != h.invokeWindow {
		h.invokeWindow = window
		h.invokeCounts = map[string]int{}
	}
	h.invokeCounts[caller]++
	if n := h.invokeCounts[caller]; n > invokeQuotaPerMinute {
		h.mu.Unlock()
		return invokeError(fmt.Sprintf("本分钟互调已达上限 %d 次（本次是第 %d 次）", invokeQuotaPerMinute, n)), nil
	}

	// ---- 链校验：caller 已在链上就是环；深度含本次 caller ----
	env := proto.Clone(req.GetEnvelope()).(*hubv1.Envelope)
	chain := callChainOf(env.GetMeta())
	if slices.Contains(chain, caller) {
		h.mu.Unlock()
		return invokeError(fmt.Sprintf("检测到互调环: %s 已在调用链上（%s）", caller, strings.Join(chain, " → "))), nil
	}
	if len(chain)+1 > hubkit.MaxInvokeDepth {
		h.mu.Unlock()
		return invokeError(fmt.Sprintf("互调链已达上限（%d），当前 %d 段（%s）",
			hubkit.MaxInvokeDepth, len(chain), strings.Join(chain, " → "))), nil
	}

	// ---- 解析目标 ----
	reg, ok := h.registry[req.GetPlugin()]
	if !ok {
		h.mu.Unlock()
		return invokeError(fmt.Sprintf("目标插件 %s 未注册", req.GetPlugin())), nil
	}
	if v := req.GetVersion(); v != "" && reg.GetVersion() != v {
		h.mu.Unlock()
		return invokeError(fmt.Sprintf("目标插件 %s 没有版本 %s", req.GetPlugin(), v)), nil
	}
	advertise := reg.GetAdvertiseAddr()

	// ---- 整备（与真中台同一批规则）：链追加 caller、subject 覆盖为 caller、
	// deadline 夹紧、trace 补齐、type 未指定时置 REQUEST ----
	chain = append(chain, caller)
	if env.GetMeta() == nil {
		env.Meta = map[string]string{}
	}
	env.Meta[hubkit.CallChainMeta] = strings.Join(chain, ",")
	env.Subject = &hubv1.Subject{Kind: hubv1.SubjectKind_SUBJECT_KIND_PLUGIN, Id: caller}
	env.DeadlineMs = clampDeadline(env.GetDeadlineMs(), req.GetTimeoutMs(), time.Now().UnixMilli())
	if env.GetTraceId() == "" {
		env.TraceId = hubkit.NewULID()
	}
	if env.GetType() == hubv1.PayloadType_PAYLOAD_TYPE_UNSPECIFIED {
		env.Type = hubv1.PayloadType_PAYLOAD_TYPE_REQUEST
	}
	h.mu.Unlock()

	// ---- 执行：Validate → Handle，与真中台的 Invoker 同序 ----
	client, err := DialPlugin(advertise)
	if err != nil {
		return invokeError(fmt.Sprintf("目标插件 %s 不可达: %v", req.GetPlugin(), err)), nil
	}
	defer client.Close()

	vresp, err := client.Validate(ctx, env)
	if err != nil {
		return invokeError(fmt.Sprintf("目标插件 %s 校验器调用失败: %v", req.GetPlugin(), err)), nil
	}
	if !vresp.GetValid() {
		return &hubv1.InvokeResponse{
			Outcome:   hubv1.InvokeOutcome_REJECTED,
			Issues:    vresp.GetIssues(),
			Reason:    "目标插件校验未通过，见 issues",
			ElapsedMs: elapsed(),
		}, nil
	}

	out, err := client.Handle(ctx, env)
	if err != nil {
		return invokeError(fmt.Sprintf("目标插件 %s 调用失败: %v", req.GetPlugin(), err)), nil
	}
	if out == nil {
		return invokeError(fmt.Sprintf("目标插件 %s 未返回信封", req.GetPlugin())), nil
	}
	return &hubv1.InvokeResponse{
		Outcome:   hubv1.InvokeOutcome_HANDLED,
		Envelope:  out,
		ElapsedMs: elapsed(),
	}, nil
}

// ---------------------------------------------------------------- 调用插件

// Client 是指向某个插件的运行时客户端，供 conformance 与调试工具复用。
type Client struct {
	conn *grpc.ClientConn
}

// DialPlugin 连上一个插件的 gRPC 地址（如 http://127.0.0.1:9000）。
func DialPlugin(addr string) (*Client, error) {
	target := addr
	for _, prefix := range []string{"http://", "https://"} {
		if len(target) > len(prefix) && target[:len(prefix)] == prefix {
			target = target[len(prefix):]
		}
	}

	conn, err := grpc.NewClient(target, grpc.WithTransportCredentials(insecure.NewCredentials()))
	if err != nil {
		return nil, fmt.Errorf("mockhub: 连接插件 %s 失败: %w", addr, err)
	}
	return &Client{conn: conn}, nil
}

// Close 关闭连接。
func (c *Client) Close() { _ = c.conn.Close() }

// Validate 调插件的校验器。
func (c *Client) Validate(ctx context.Context, env *hubv1.Envelope) (*hubv1.ValidateResponse, error) {
	return hubv1.NewPluginRuntimeClient(c.conn).Validate(ctx, &hubv1.ValidateRequest{Envelope: env})
}

// Handle 调插件的插件体。
func (c *Client) Handle(ctx context.Context, env *hubv1.Envelope) (*hubv1.Envelope, error) {
	resp, err := hubv1.NewPluginRuntimeClient(c.conn).Handle(ctx, &hubv1.HandleRequest{Envelope: env})
	if err != nil {
		return nil, err
	}
	if resp.GetEnvelope() == nil {
		return nil, fmt.Errorf("mockhub: 插件未返回信封")
	}
	return resp.GetEnvelope(), nil
}

// Describe 拉插件的 manifest。
func (c *Client) Describe(ctx context.Context) (*hubv1.PluginManifest, error) {
	return hubv1.NewPluginRuntimeClient(c.conn).Describe(ctx, &hubv1.DescribeRequest{})
}

// Health 探插件的健康。
func (c *Client) Health(ctx context.Context) (*hubv1.HealthResponse, error) {
	return hubv1.NewPluginRuntimeClient(c.conn).Health(ctx, &hubv1.HealthRequest{})
}
