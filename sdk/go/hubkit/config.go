package hubkit

import (
	"fmt"
	"log/slog"
	"os"
	"strings"
	"time"
)

// 默认值。导出成常量便于插件在自己的启动逻辑里做校验或打印。
const (
	DefaultListenAddr = ":9000"

	// RegisterRetryInterval 注册失败后的重试间隔。
	//
	// 注册会一直重试而不是放弃：中台可能比你晚起来，而插件进程先于中台启动是常态。
	RegisterRetryInterval = 5 * time.Second

	// HeartbeatFallbackInterval 中台没告诉我们心跳周期时的兜底值。
	HeartbeatFallbackInterval = 10 * time.Second

	// DefaultStateCallTimeout 是单次状态调用的默认时间上限。
	//
	// 远小于信封预算（HTTP 面缺省 30s），让状态调用先于业务调用放弃：
	// 中台一次卡顿最坏能吃掉 10s（PG 取连接）+ 查询 + 5s（Redis 响应超时），
	// 不设上限时这些时间全部从调用方的预算里扣，随后的业务调用会拿到已过期的 ctx。
	//
	// 取 2s 而不是 5s：客户端应**短于**中台侧的 Redis 响应超时（5s），
	// 由客户端先放弃，插件才有机会走 fail-open。
	DefaultStateCallTimeout = 2 * time.Second
)

// Config 是骨架的运行参数。
type Config struct {
	// HubAddr 中台插件面地址。
	//
	// 生产形如 https://hub.example.com:8094（经 nginx 的 TLS 终结）。
	// 中台与插件不要求同机——插件可以部署在任何能连上这个地址的地方。
	HubAddr string

	// AdvertiseAddr 中台可达的本插件地址，例如 http://10.0.0.5:9000。
	//
	// 中台在注册时会连它做**可达性探测**，所以必须是「从中台那边拨得通」的地址，
	// 而不是本机视角的 localhost——这是插件接入时最容易踩的坑。
	AdvertiseAddr string

	// ListenAddr 本插件 gRPC 的监听地址，缺省 [DefaultListenAddr]。
	ListenAddr string

	// TLSMaxVersion 限制与中台之间 TLS 的**最高**版本：可取值 "1.2" 或 "1.3"，
	// 留空则跟随 Go 的默认（当前会协商到 1.3）。
	//
	// 存在的理由是一个真实撞上的互操作问题：某些网络路径上的中间设备会重置
	// **Go 客户端**的 TLS 1.3 握手——表现为 `connection reset by peer`，而错误信息里
	// 完全看不出是网络设备干的（curl 与 openssl 在同一条路径上却正常，所以很容易
	// 误判成服务端问题）。把上限压到 "1.2" 即可绕开。
	//
	// 它是**逃生口，不是默认配置**：只有在确认服务端没问题、且换 TLS 版本就能通之后
	// 才该用它。压到 1.2 是有代价的——1.3 的握手更短、前向保密更强。
	TLSMaxVersion string

	// InstanceID 实例标识，缺省 `主机名-PID`。
	//
	// 同一 ID 重复注册视为进程重启（中台会刷新地址与心跳），不产生重复实例。
	InstanceID string

	// RetryInterval 注册失败后的重试间隔，缺省 [RegisterRetryInterval]。
	//
	// 测试里会调到毫秒级；生产一般不必改。
	RetryInterval time.Duration

	// StateCallTimeout 是单次 HubState 调用的时间上限，缺省 [DefaultStateCallTimeout]。
	//
	// 它是**上限而非承诺**：父 ctx 的 deadline 更早时按父的来。
	//
	// SDK 只能保证「一次状态调用不会吃光整个预算」，保证不了「给业务留多少」
	// ——SDK 不知道调用方的总预算意图。需要更紧的策略（比如只吃剩余预算的一半）
	// 请在插件侧自己派生 ctx。
	StateCallTimeout time.Duration

	// Logger 结构化日志，缺省打到 stderr。
	Logger *slog.Logger
}

// Validate 检查必填项，并给出能直接照做的提示。
func (c Config) Validate() error {
	var missing []string
	if strings.TrimSpace(c.HubAddr) == "" {
		missing = append(missing, "HUB_ADDR（中台插件面地址）")
	}
	if strings.TrimSpace(c.AdvertiseAddr) == "" {
		missing = append(missing, "HUB_ADVERTISE_ADDR（中台可达的本插件地址）")
	}
	if len(missing) > 0 {
		return fmt.Errorf("hubkit: 缺少必填配置 %s", strings.Join(missing, "、"))
	}
	switch v := strings.TrimSpace(c.TLSMaxVersion); v {
	case "", "1.2", "1.3":
	default:
		return fmt.Errorf("hubkit: HUB_TLS_MAX_VERSION 只接受 \"1.2\" 或 \"1.3\"，收到 %q", v)
	}
	return nil
}

// WithDefaults 补齐缺省值。
func (c Config) WithDefaults() Config {
	if c.ListenAddr == "" {
		c.ListenAddr = DefaultListenAddr
	}
	if c.InstanceID == "" {
		host, err := os.Hostname()
		if err != nil {
			host = "unknown-host"
		}
		c.InstanceID = fmt.Sprintf("%s-%d", host, os.Getpid())
	}
	if c.RetryInterval <= 0 {
		c.RetryInterval = RegisterRetryInterval
	}
	if c.StateCallTimeout <= 0 {
		c.StateCallTimeout = DefaultStateCallTimeout
	}
	if c.Logger == nil {
		c.Logger = slog.New(slog.NewJSONHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelInfo}))
	}
	return c
}

// ConfigFromEnv 从环境变量读取配置；缺省值由 [Config.WithDefaults] 补齐。
//
//	HUB_ADDR            中台插件面地址（必填）
//	HUB_ADVERTISE_ADDR  本插件对外可达地址（必填）
//	HUB_LISTEN_ADDR     本插件监听地址（缺省 :9000）
//	HUB_INSTANCE_ID     实例标识（缺省 主机名-PID）
//	HUB_TLS_MAX_VERSION TLS 最高版本，1.2 或 1.3（缺省跟随 Go；见 [Config.TLSMaxVersion]）
//	HUB_LOG_LEVEL       debug / info / warn / error（缺省 info）
func ConfigFromEnv() Config {
	return Config{
		HubAddr:       os.Getenv("HUB_ADDR"),
		AdvertiseAddr: os.Getenv("HUB_ADVERTISE_ADDR"),
		ListenAddr:    os.Getenv("HUB_LISTEN_ADDR"),
		InstanceID:    os.Getenv("HUB_INSTANCE_ID"),
		TLSMaxVersion: os.Getenv("HUB_TLS_MAX_VERSION"),
		Logger:        loggerFromEnv(),
	}
}

func loggerFromEnv() *slog.Logger {
	var level slog.Level
	switch strings.ToLower(strings.TrimSpace(os.Getenv("HUB_LOG_LEVEL"))) {
	case "debug":
		level = slog.LevelDebug
	case "warn":
		level = slog.LevelWarn
	case "error":
		level = slog.LevelError
	default:
		level = slog.LevelInfo
	}
	return slog.New(slog.NewJSONHandler(os.Stderr, &slog.HandlerOptions{Level: level}))
}
