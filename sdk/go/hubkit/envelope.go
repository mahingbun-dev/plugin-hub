package hubkit

import (
	"crypto/rand"
	"fmt"
	"math/big"
	"strings"
	"time"

	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/types/known/anypb"
	"google.golang.org/protobuf/types/known/structpb"

	"github.com/mahingbun-dev/anc-hub/sdk/go/proto/hubv1"
)

// StructTypeURL 是「直接调用」载荷的类型标识。
//
// agent 经 MCP、外部系统经 HTTP 调用插件时，中台把 JSON 对象包成
// google.protobuf.Struct 放进信封；flow 内部传递的则是业务类型。
// 插件两种都可能收到，用 [PayloadJSON] 区分。
const StructTypeURL = "type.googleapis.com/google.protobuf.Struct"

// StructFQName 是上面那个载荷的全限定消息名。
//
// 在 manifest 里声明 `Consumes: []*hubv1.MessageContract{{FqName: hubkit.StructFQName}}`
// 表示「本插件接受直接调用的 JSON 载荷」。它是 well-known 类型，中台不要求它出现在
// 插件自己的 descriptor 里。
const StructFQName = "google.protobuf.Struct"

// IsWellKnownFQName 判断是否属于 protobuf 平台提供的 well-known 类型。
//
// 中台对 google.protobuf.* 豁免「声明必须出现在自己的 descriptor 里」这条检查；
// 插件侧的自测套件用同一个判断，避免两边规则漂移。
func IsWellKnownFQName(fqName string) bool {
	return strings.HasPrefix(fqName, "google.protobuf.")
}

// PayloadJSON 取出信封里的 JSON 载荷。
//
// 载荷不是 Struct（例如 flow 内部传的业务类型）时返回 ok=false，
// 此时插件应改为按自己的业务类型去 Unmarshal。
//
// 注意数值一律是 float64——google.protobuf.Struct 只有一种数值类型。
// 大单号这类超出 2^53 的整数请用字符串承载，别指望 JSON 数字。
func PayloadJSON(env *hubv1.Envelope) (map[string]any, bool) {
	payload := env.GetPayload()
	if payload == nil || payload.GetTypeUrl() != StructTypeURL {
		return nil, false
	}

	s := &structpb.Struct{}
	if err := payload.UnmarshalTo(s); err != nil {
		return nil, false
	}
	return s.AsMap(), true
}

// WithPayloadJSON 把 JSON 对象装进信封的载荷。
//
// 返回新信封，原信封不被修改——链路里可能有别的持有者。
func WithPayloadJSON(env *hubv1.Envelope, payload map[string]any) (*hubv1.Envelope, error) {
	s, err := structpb.NewStruct(payload)
	if err != nil {
		return nil, fmt.Errorf("hubkit: 载荷不是合法的 JSON 对象: %w", err)
	}

	packed, err := anypb.New(s)
	if err != nil {
		return nil, fmt.Errorf("hubkit: 打包载荷失败: %w", err)
	}

	out := cloneEnvelope(env)
	out.Payload = packed
	return out, nil
}

// WithPayload 把业务类型装进信封的载荷（flow 内部传递用）。
func WithPayload(env *hubv1.Envelope, message proto.Message) (*hubv1.Envelope, error) {
	packed, err := anypb.New(message)
	if err != nil {
		return nil, fmt.Errorf("hubkit: 打包载荷失败: %w", err)
	}
	out := cloneEnvelope(env)
	out.Payload = packed
	return out, nil
}

// Deadline 返回信封的绝对截止时间。未设置时 ok=false。
//
// deadline 逐跳递减：插件应据此提前放弃，而不是把时间耗光后让上层的超时兜底。
func Deadline(env *hubv1.Envelope) (time.Time, bool) {
	ms := env.GetDeadlineMs()
	if ms <= 0 {
		return time.Time{}, false
	}
	return time.UnixMilli(ms), true
}

// Budget 返回距离截止时间还剩多久。未设置时 ok=false；已过期时返回 0。
func Budget(env *hubv1.Envelope) (time.Duration, bool) {
	deadline, ok := Deadline(env)
	if !ok {
		return 0, false
	}
	left := time.Until(deadline)
	if left < 0 {
		left = 0
	}
	return left, true
}

// Expired 判断信封是否已过截止时间。
func Expired(env *hubv1.Envelope) bool {
	budget, ok := Budget(env)
	return ok && budget == 0
}

// Valid 构造「校验通过」的响应。
func Valid() *hubv1.ValidateResponse {
	return &hubv1.ValidateResponse{Valid: true}
}

// Invalid 构造「校验不通过」的响应。
//
// 每个 issue 的 path 要能定位到具体字段（例如 payload.items[2].sku），
// 中台会原样把它回给调用方，agent 靠它改数据重试。
func Invalid(issues ...*hubv1.ValidationIssue) *hubv1.ValidateResponse {
	return &hubv1.ValidateResponse{Valid: false, Issues: issues}
}

// Issue 构造一条错误级校验问题。
func Issue(path, message string) *hubv1.ValidationIssue {
	return &hubv1.ValidationIssue{
		Path:     path,
		Message:  message,
		Severity: hubv1.Severity_SEVERITY_ERROR,
	}
}

// Warn 构造一条警告级校验问题。
//
// 警告不会让校验失败——用它标记「能放行但值得记一笔」的情况。
func Warn(path, message string) *hubv1.ValidationIssue {
	return &hubv1.ValidationIssue{
		Path:     path,
		Message:  message,
		Severity: hubv1.Severity_SEVERITY_WARNING,
	}
}

func cloneEnvelope(env *hubv1.Envelope) *hubv1.Envelope {
	if env == nil {
		return &hubv1.Envelope{}
	}
	return proto.Clone(env).(*hubv1.Envelope)
}

// ulidEncoding 是 Crockford Base32 字母表（ULID 标准，I 与 L、O 与 0 这类
// 易混字符被剔除）。中台侧（Rust 的 ulid crate）生成的 id 也是这个形状。
const ulidEncoding = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"

var ulidRadix = big.NewInt(32)

// NewULID 生成一个 ULID（48 位毫秒时间戳 + 80 位随机数，26 字符）。
//
// message_id 与 trace_id 都用它：message_id 同时是总线的幂等键（at-least-once
// 下去重靠它），trace_id 串起一次调用经过的所有插件——两者都要「同一毫秒内
// 也不重复」，所以时间戳之外还掺了 crypto/rand 的随机位。
//
// 熵源坏了（rand.Read 失败）时 panic 而不是返回错误：拿不到随机数就意味着
// 生成的 id 可能撞车、幂等去重可能误伤，带着这种 id 上路比立刻失败更糟。
func NewULID() string {
	var raw [16]byte
	ms := time.Now().UnixMilli()
	// 时间戳占高 48 位（大端），低 80 位交给随机数
	raw[0], raw[1], raw[2] = byte(ms>>40), byte(ms>>32), byte(ms>>24)
	raw[3], raw[4], raw[5] = byte(ms>>16), byte(ms>>8), byte(ms)
	if _, err := rand.Read(raw[6:]); err != nil {
		panic("hubkit: 生成 ULID 的熵源不可用: " + err.Error())
	}

	n := new(big.Int).SetBytes(raw[:])
	out := make([]byte, 26)
	// 128 位对 26 个字符（每个 5 位）会余出 2 个零位，落在最高字符上——
	// 从低位往回填正好把它空出来。DivMod 一步拿到「商和余数」：先 Mod 再 Rsh
	// 是错的——Mod 会把 n 就地改成余数，随后的移位就在余数上做了
	var rem big.Int
	for i := 25; i >= 0; i-- {
		n.DivMod(n, ulidRadix, &rem)
		out[i] = ulidEncoding[rem.Int64()]
	}
	return string(out)
}

// NewEnvelope 构造一个带全新 message_id / trace_id 的空信封。
//
// 自己发起一条数据流（Publish、或手动装配 Invoke 的信封）时用它起步，
// 载荷再用 [WithPayloadJSON] / [WithPayload] 装。type 留空不替调用方决定语义：
// 触发路径各自知道这次携带的是请求还是事件。
func NewEnvelope() *hubv1.Envelope {
	return &hubv1.Envelope{MessageId: NewULID(), TraceId: NewULID()}
}
