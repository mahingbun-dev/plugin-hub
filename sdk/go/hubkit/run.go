package hubkit

import (
	"context"
	"crypto/tls"
	"errors"
	"fmt"
	"log/slog"
	"net"
	"os"
	"os/signal"
	"strings"
	"sync"
	"syscall"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/status"

	"github.com/mahingbun-dev/plugin-hub/sdk/go/proto/hubv1"
)

// Run 启动插件，直到收到 SIGINT / SIGTERM。
//
// 它做四件事：起 gRPC 服务、向中台自注册、维持心跳、优雅退出时注销。
// 插件作者只需要实现 [Plugin]。
//
// 三个刻意的行为：
//   - **注册会一直重试**：中台可能比插件晚起来，插件先启动是常态。
//   - **被摘除后自动重新注册**：心跳响应里带 reregister_required 时重走注册流程，
//     这是实例掉线后能自愈的关键。
//   - **状态凭证失效后自动重新注册**：HubState 调用收到 UNAUTHENTICATED 时重走注册
//     流程换新凭证（中台重启轮换凭证、实例被摘除）。这是防御层，不是主恢复路径——
//     中台把凭证落了库，重启对插件是透明的。
func Run(plugin Plugin, cfg Config) error {
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	return RunContext(ctx, plugin, cfg)
}

// RunContext 与 [Run] 相同，但由调用方决定何时结束。
//
// 测试与嵌入式场景用它：给一个可取消的 ctx 就能把插件干净地停掉，不必真的发信号。
func RunContext(ctx context.Context, plugin Plugin, cfg Config) error {
	if err := cfg.Validate(); err != nil {
		return err
	}
	cfg = cfg.WithDefaults()
	log := cfg.Logger

	if err := checkManifest(plugin); err != nil {
		return err
	}

	listener, err := net.Listen("tcp", cfg.ListenAddr)
	if err != nil {
		return fmt.Errorf("hubkit: 监听 %s 失败: %w", cfg.ListenAddr, err)
	}

	// 连接提到 server 之前：状态客户端要挂在这条连接上，与注册共用。
	// 原先"dial 失败就 server.Stop()"的分支随之消失——此时 server 还没建；
	// 但监听已经开好了，失败时必须把它关掉，否则端口一直占着。
	conn, err := dial(cfg)
	if err != nil {
		_ = listener.Close()
		return err
	}
	defer func() { _ = conn.Close() }()

	server := grpc.NewServer()
	hubv1.RegisterPluginRuntimeServer(server, &runtimeService{plugin: plugin, log: log})

	serveErr := make(chan error, 1)
	go func() {
		if err := server.Serve(listener); err != nil {
			serveErr <- err
		}
	}()
	log.Info("插件 gRPC 已监听", "listen", listener.Addr().String(), "advertise", cfg.AdvertiseAddr)

	reg := &registrar{
		client: hubv1.NewPluginRegistryClient(conn),
		// denied 通道是状态/网关客户端与注册循环之间的那根线：插件侧撞上 401 时，
		// 心跳循环会收到它并立刻重走注册流程换新凭证。两面共用一个通道——
		// 凭证是同一份，中台重启时两面一起失效、一起换
		denied: make(chan struct{}, 1),
		plugin: plugin,
		cfg:    cfg,
		log:    log,
	}
	// 网关客户端与状态客户端挂在同一条注册用的连接上，callTimeout 只约束
	// 发现类调用（互调的预算是信封 deadline，见 GatewayClient 的说明）
	reg.state = &StateClient{
		client:      hubv1.NewHubStateClient(conn),
		denied:      reg.denied,
		callTimeout: cfg.StateCallTimeout,
	}
	reg.gateway = &GatewayClient{
		client:      hubv1.NewPluginGatewayClient(conn),
		denied:      reg.denied,
		callTimeout: cfg.StateCallTimeout,
	}
	regDone := make(chan struct{})
	go func() {
		defer close(regDone)
		reg.loop(ctx)
	}()

	log.Info("插件已启动", "hub", cfg.HubAddr, "instance", cfg.InstanceID)

	select {
	case <-ctx.Done():
		log.Info("收到退出信号，开始优雅退出")
	case err := <-serveErr:
		return fmt.Errorf("hubkit: gRPC 服务异常退出: %w", err)
	}

	// 主动注销：中台据此立刻摘掉实例，不必等心跳超时
	reg.unregister(cfg.InstanceID)
	<-regDone
	server.GracefulStop()
	log.Info("插件已退出")
	return nil
}

// checkManifest 在启动时就把 manifest 的明显问题挡住。
//
// 这些问题中台也会拒，但那是网络往返之后的事——本地先炸能省一轮排查。
func checkManifest(plugin Plugin) error {
	m := plugin.Manifest()
	if m == nil {
		return errors.New("hubkit: Manifest() 返回了 nil")
	}
	if strings.TrimSpace(m.GetName()) == "" {
		return errors.New("hubkit: manifest 缺少插件名（name）")
	}
	if strings.TrimSpace(m.GetVersion()) == "" {
		return errors.New("hubkit: manifest 缺少版本号（version）——flow 靠它锁定实例")
	}

	// 空 descriptor 本身合法：只用 google.protobuf.Struct 承载 JSON 的插件没有自己的 proto。
	// 但声明了自有类型的就必须提供，否则中台会拒（声明的类型找不到出处）。
	if len(plugin.Descriptor()) == 0 {
		for _, contract := range append(append([]*hubv1.MessageContract{}, m.GetProduces()...), m.GetConsumes()...) {
			if !IsWellKnownFQName(contract.GetFqName()) {
				return fmt.Errorf(
					"hubkit: manifest 声明了自有类型 %s，但 Descriptor() 返回空——"+
						"要么把它从 produces/consumes 里去掉，要么提供它的 proto",
					contract.GetFqName())
			}
		}
	}
	return nil
}

// tlsConfigFor 按配置构造与中台之间的 TLS 参数。
//
// 抽出来是为了让「TLSMaxVersion 确实作用到了握手参数上」这件事可被单测证明——
// 只测配置校验是测不出它有没有真的生效的。
func tlsConfigFor(cfg Config) *tls.Config {
	tc := &tls.Config{MinVersion: tls.VersionTLS12}
	// 受限网络的逃生口：有些路径上的中间设备会重置 Go 的 TLS 1.3 握手。
	// 详见 Config.TLSMaxVersion 的说明——留空即跟随 Go 默认，不改行为。
	switch strings.TrimSpace(cfg.TLSMaxVersion) {
	case "1.2":
		tc.MaxVersion = tls.VersionTLS12
	case "1.3":
		tc.MaxVersion = tls.VersionTLS13
	}
	return tc
}

func dial(cfg Config) (*grpc.ClientConn, error) {
	var creds credentials.TransportCredentials
	if strings.HasPrefix(cfg.HubAddr, "https://") {
		creds = credentials.NewTLS(tlsConfigFor(cfg))
	} else {
		creds = insecure.NewCredentials()
	}

	target := strings.TrimPrefix(strings.TrimPrefix(cfg.HubAddr, "https://"), "http://")
	conn, err := grpc.NewClient(target, grpc.WithTransportCredentials(creds))
	if err != nil {
		return nil, fmt.Errorf("hubkit: 连接中台 %s 失败: %w", cfg.HubAddr, err)
	}
	return conn, nil
}

// ---------------------------------------------------------------- gRPC 服务

type runtimeService struct {
	plugin Plugin
	log    *slog.Logger
}

func (s *runtimeService) Describe(context.Context, *hubv1.DescribeRequest) (*hubv1.PluginManifest, error) {
	return s.plugin.Manifest(), nil
}

func (s *runtimeService) Validate(ctx context.Context, req *hubv1.ValidateRequest) (*hubv1.ValidateResponse, error) {
	env := req.GetEnvelope()
	if env == nil {
		return Invalid(Issue("envelope", "缺少信封")), nil
	}

	resp, err := s.plugin.Validate(ctx, env)
	if err != nil {
		return nil, status.Errorf(codes.Internal, "校验器执行失败: %v", err)
	}
	return resp, nil
}

func (s *runtimeService) Handle(ctx context.Context, req *hubv1.HandleRequest) (*hubv1.HandleResponse, error) {
	env := req.GetEnvelope()
	if env == nil {
		return nil, status.Error(codes.InvalidArgument, "缺少信封")
	}

	out, err := s.plugin.Handle(ctx, env)
	if err != nil {
		// 插件自己的错误原样上报：中台会把它归到「插件调用失败」，调用方据此重试
		return nil, status.Errorf(codes.Internal, "插件处理失败: %v", err)
	}
	return &hubv1.HandleResponse{Envelope: out}, nil
}

func (s *runtimeService) HandleStream(*hubv1.HandleRequest, grpc.ServerStreamingServer[hubv1.HandleResponse]) error {
	return status.Error(codes.Unimplemented, "流式处理尚未实现（见 docs/design.md 的载荷边界，随 M3 落地）")
}

func (s *runtimeService) Health(context.Context, *hubv1.HealthRequest) (*hubv1.HealthResponse, error) {
	return &hubv1.HealthResponse{Healthy: true, Message: "ok"}, nil
}

// ---------------------------------------------------------------- 注册与心跳

type registrar struct {
	client  hubv1.PluginRegistryClient
	state   *StateClient
	gateway *GatewayClient
	plugin  Plugin
	cfg     Config
	log     *slog.Logger

	// denied 由状态/网关客户端共用，见 RunContext 里的构造处。
	denied chan struct{}

	mu       sync.Mutex
	interval time.Duration
	// lastRegister 是最近一次注册成功的时刻，用于给"denial 触发的强制重注册"设速率下限。
	// 见 [registrar.heartbeatLoop]。
	lastRegister time.Time
	// stateToken 是最近一次注册成功拿到的状态凭证，注销时用它向中台证明
	// "我是这一行的主人"。**注册没成功过就是空的**——空就意味着没有实例行可摘除，
	// 也就根本不该发注销（见 [registrar.unregister]）。
	stateToken string
}

// loop 维持「注册 → 心跳 → 被摘除则重新注册」的循环，直到 ctx 结束。
func (r *registrar) loop(ctx context.Context) {
	r.interval = HeartbeatFallbackInterval

	for ctx.Err() == nil {
		if err := r.registerOnce(ctx); err != nil {
			if ctx.Err() != nil {
				return
			}
			r.logRegisterFailure(err)
			if !sleepCtx(ctx, r.cfg.RetryInterval) {
				return
			}
			continue
		}

		r.heartbeatLoop(ctx)
	}
}

// logRegisterFailure 把一次注册失败讲成「看一眼就懂」的样子。
//
// 中台拒绝时给的是结构化原因，这里让每条原因各占一行。日志走的是 slog 的 JSON
// handler，一整段多行文本会被转义成 \n 塞进单个字段——一行里糊着 N 条原因，
// 得靠人脑反解析才看得出哪条是哪条。拆成 code / message / detail 三列之后，
// 每行本身就是完整的一条，读日志不需要 jq，也不需要任何工具。
//
// 重试间隔单独收尾一行，而不是跟在每条原因后面：它是「接下来会怎样」，
// 与「错在哪」不是一回事，逐条重复只会把原因行淹掉。reasons 是原因条数，
// 用来兜底——日志被截断时，一眼能看出还有几条没打出来。
func (r *registrar) logRegisterFailure(err error) {
	var rejected *RegistrationRejected
	if errors.As(err, &rejected) && len(rejected.Rejections) > 0 {
		for _, rej := range rejected.Rejections {
			r.log.Error("中台拒绝了注册",
				"code", RejectCodeName(rej.GetCode()),
				"message", rej.GetMessage(),
				"detail", rej.GetDetail(),
			)
		}
		r.log.Error("注册未通过，稍后重试",
			"reasons", len(rejected.Rejections),
			// Duration 的 String() 给的是 "5s"；直接打 Duration 会落成纳秒整数
			// 5000000000，没人认得出那是 5 秒
			"retry_in", r.cfg.RetryInterval.String(),
		)
		return
	}

	// 连不上中台、网络抖动这类错误本身就是单行的，和重试间隔打在同一行里正好，
	// 别为了跟上面的格式统一把它拆开——拆开只会多出一行没有信息量的收尾。
	r.log.Error("注册未通过，稍后重试", "err", err, "retry_in", r.cfg.RetryInterval.String())
}

func (r *registrar) registerOnce(ctx context.Context) error {
	manifest := r.plugin.Manifest()

	resp, err := r.client.Register(ctx, &hubv1.RegisterRequest{
		PluginName:    manifest.GetName(),
		Version:       manifest.GetVersion(),
		InstanceId:    r.cfg.InstanceID,
		AdvertiseAddr: r.cfg.AdvertiseAddr,
		Manifest:      manifest,
		DescriptorSet: r.plugin.Descriptor(),
	})
	if err != nil {
		return fmt.Errorf("调用中台注册接口失败: %w", err)
	}

	if !resp.GetAccepted() {
		return &RegistrationRejected{Rejections: resp.GetRejections()}
	}

	// 记下成功注册的时刻：denial 触发的强制重注册靠它做速率下限（见 heartbeatLoop）。
	// 凭证同一次临界区里更新——两者都只属于"最近一次成功注册"，分开写迟早对不上。
	r.mu.Lock()
	r.lastRegister = time.Now()
	r.stateToken = resp.GetStateToken()
	r.mu.Unlock()

	// 凭证随每次注册轮换，这里覆盖旧的（状态面与网关面是同一份凭证）
	r.state.setToken(resp.GetStateToken())
	r.gateway.setToken(resp.GetStateToken())
	if resp.GetStateToken() == "" {
		r.log.Warn("中台未下发状态凭证，HubState 将不可用")
	}
	// 每次注册后都注入一次：插件可能还持有上一个凭证时期的客户端引用
	if aware, ok := r.plugin.(StateAware); ok {
		aware.SetState(r.state)
	}
	if aware, ok := r.plugin.(GatewayAware); ok {
		aware.SetGateway(r.gateway)
	}

	if seconds := resp.GetHeartbeatIntervalSeconds(); seconds > 0 {
		r.mu.Lock()
		r.interval = time.Duration(seconds) * time.Second
		r.mu.Unlock()
	}

	for _, w := range resp.GetWarnings() {
		r.log.Warn("中台提示", "warning", w)
	}
	r.log.Info("已注册到中台",
		"plugin", manifest.GetName(),
		"version", manifest.GetVersion(),
		"instance", resp.GetInstanceId(),
	)
	return nil
}

// heartbeatLoop 按中台指定的周期续期，返回即表示需要重新注册（或 ctx 结束）。
//
// 触发返回的有三条：中台要求重注册、心跳被拒、以及插件侧状态调用撞上 401（denial）。
// 最后一条受 [Config.RetryInterval] 这个冷却窗口约束——窗口内到达的 denial 被忽略，
// 免得持续拒绝时把注册循环打成无限自旋。
func (r *registrar) heartbeatLoop(ctx context.Context) {
	r.mu.Lock()
	interval := r.interval
	r.mu.Unlock()

	ticker := time.NewTicker(interval)
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			return
		case <-r.denied:
			// 插件侧的状态调用被中台判了 401：凭证多半已被吊销或轮换。
			// 心跳本身可能一切正常（实例还在库里），不重注册就会一直哑下去。
			//
			// 但必须有速率下限：denial 可能连续不断（中台校验滞后、撤销尚未传播，
			// 或非凭证原因也回 401）。没有它，一次成功注册之后紧接着排空 denial
			// 就是零延迟，注册速率等于 Register RPC 的延迟——无限自旋，同时还在
			// 反复探测插件自己的地址。窗口内到达的 denial 直接丢掉。
			r.mu.Lock()
			since := time.Since(r.lastRegister)
			r.mu.Unlock()
			if since < r.cfg.RetryInterval {
				r.log.Warn("状态凭证被拒，但距上次注册不足冷却窗口，忽略本次",
					"since", since, "cooldown", r.cfg.RetryInterval)
				continue
			}
			r.log.Warn("状态凭证被拒，重新注册以换取新凭证")
			return
		case <-ticker.C:
			resp, err := r.client.Heartbeat(ctx, &hubv1.HeartbeatRequest{InstanceId: r.cfg.InstanceID})
			if err != nil {
				if ctx.Err() != nil {
					return
				}
				// 网络抖动不该让插件停止心跳，下一拍继续
				r.log.Warn("心跳失败", "err", err)
				continue
			}
			if !resp.GetAccepted() || resp.GetReregisterRequired() {
				r.log.Warn("中台要求重新注册（实例可能已被摘除）")
				return
			}
		}
	}
}

// unregister 主动注销。中台据此立刻摘掉实例，不必等心跳超时。
//
// **没拿到过凭证就整个跳过**。凭证只在注册成功时下发，为空说明本实例压根没进过注册表
// （注册被拒、或还没注册上就退出了），没有实例行可摘除。而 `instance_id` 是插件自报的、
// 可以跟别的插件撞（缺省「主机名-PID」，同一 host 网络下容器 PID 又都是 1）——此时发一次
// 不带身份的注销，中台若只按 `instance_id` 删行，删掉的正是**对方**那一行。中台侧现在
// 会拒（见 `UnregisterRequest.state_token`），插件侧这一道是别发这个注定被拒的请求。
//
// 这也决定了**插件先于中台升级**时的行为：旧中台的 `RegisterResponse` 没有这个字段，
// SDK 拿到的是空串，于是注销整个跳过——对旧中台也就不再有优雅注销，只能等它心跳超时
// 摘除。窗口是有界的（中台升级完就恢复），而反过来放行的代价是可能删掉别人的实例行。
func (r *registrar) unregister(instanceID string) {
	r.mu.Lock()
	token := r.stateToken
	r.mu.Unlock()

	if token == "" {
		r.log.Info("本实例没有注册凭证，跳过主动注销（没注册成功就没有可摘除的实例行）")
		return
	}

	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()

	if _, err := r.client.Unregister(ctx, &hubv1.UnregisterRequest{
		InstanceId: instanceID,
		Reason:     "插件优雅退出",
		StateToken: token,
	}); err != nil {
		r.log.Warn("注销失败（中台会在心跳超时后自行摘除）", "err", err)
	}
}

func sleepCtx(ctx context.Context, d time.Duration) bool {
	timer := time.NewTimer(d)
	defer timer.Stop()
	select {
	case <-ctx.Done():
		return false
	case <-timer.C:
		return true
	}
}

// RegistrationRejected 表示中台拒绝了注册。
//
// 拒绝原因是结构化的，Error() 会把它们逐条排开——插件作者照着改就行，
// 不必去翻中台的日志。
//
// Error() 保持多行文本是有意的：它是错误值的标准呈现，给 fmt.Errorf 包装、
// 错误链和 errors.As 之后的自行打印用，那里多行是合适的。日志是另一个呈现
// 渠道，走 [registrar.logRegisterFailure]，把同一条信息摊成结构化的多行。
type RegistrationRejected struct {
	Rejections []*hubv1.Rejection
}

func (e *RegistrationRejected) Error() string {
	var b strings.Builder
	b.WriteString("中台拒绝了注册")
	if len(e.Rejections) == 0 {
		b.WriteString("（未给出原因）")
		return b.String()
	}

	b.WriteString("：")
	for _, r := range e.Rejections {
		fmt.Fprintf(&b, "\n  - %s: %s", RejectCodeName(r.GetCode()), r.GetMessage())
		if detail := r.GetDetail(); detail != "" {
			fmt.Fprintf(&b, "\n      %s", detail)
		}
	}
	return b.String()
}

// RejectCodeName 把拒绝码翻成人话。
func RejectCodeName(code hubv1.RejectCode) string {
	name, ok := hubv1.RejectCode_name[int32(code)]
	if !ok {
		return fmt.Sprintf("未知拒绝码(%d)", int32(code))
	}
	return strings.TrimPrefix(name, "REJECT_CODE_")
}
