// Package hubkit 是 anc-hub 插件侧的服务端骨架。
//
// 插件作者只需要实现 [Plugin] 接口，然后调用 [Run]，骨架会处理掉其余一切：
// gRPC 服务、向中台自注册、心跳续期、被摘除后自动重新注册、优雅退出。
//
// 最小插件长这样：
//
//	type MyPlugin struct{ hubkit.Base }
//
//	func (p *MyPlugin) Manifest() *hubv1.PluginManifest { ... }
//	func (p *MyPlugin) Validate(ctx context.Context, env *hubv1.Envelope) (*hubv1.ValidateResponse, error) { ... }
//	func (p *MyPlugin) Handle(ctx context.Context, env *hubv1.Envelope) (*hubv1.Envelope, error) { ... }
//
//	func main() {
//		err := hubkit.Run(&MyPlugin{...}, hubkit.ConfigFromEnv())
//		if err != nil { log.Fatal(err) }
//	}
//
// 插件被强制无状态：实例内存不保证跨调用保留（见 docs/design.md），
// 需要跨调用保留的东西走中台的 HubState 接口。
package hubkit

import (
	"context"

	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/reflect/protodesc"
	"google.golang.org/protobuf/reflect/protoreflect"
	"google.golang.org/protobuf/types/descriptorpb"

	"github.com/mahingbun-dev/anc-hub/sdk/go/proto/hubv1"
)

// Plugin 是插件必须实现的接口。
//
// Manifest 与 Descriptor 是「我给中台什么」，Validate 与 Handle 是「我干什么」——
// 四者缺一不可：契约校验、编排连线、MCP 工具聚合都建立在它们之上。
type Plugin interface {
	// Manifest 声明插件的身份、契约（消费/生产哪些消息类型）与暴露给 agent 的工具。
	//
	// 同一版本号的 manifest 不可变更：中台会拒绝「同号不同契约」的注册，
	// 改了东西请升版本号。
	Manifest() *hubv1.PluginManifest

	// Descriptor 返回本插件的 FileDescriptorSet，中台据此建立契约基线并做字段级兼容检查。
	//
	// 通常不必手写——嵌入 [Base] 并给出生成代码里的 proto 文件变量即可。
	Descriptor() []byte

	// Validate 是数据校验规则。
	//
	// 中台在把数据交给 Handle 之前**一定**先调用它；返回 Valid=false 时链路短路，
	// Handle 不会被调用。校验规则与插件体同版本发布，因此规则不可能与实现漂移。
	Validate(ctx context.Context, env *hubv1.Envelope) (*hubv1.ValidateResponse, error)

	// Handle 是插件体：自主实现的数据输入输出。
	//
	// 输入信封的载荷用 [PayloadJSON] 取（直接调用场景），输出用 [WithPayloadJSON]
	// 或直接设置 Envelope.Payload。
	Handle(ctx context.Context, env *hubv1.Envelope) (*hubv1.Envelope, error)
}

// Base 提供 [Plugin] 里最枯燥的一部分：从生成的 proto 文件变量构造 FileDescriptorSet。
//
// 嵌入它，然后在构造时填 Files：
//
//	type MyPlugin struct{ hubkit.Base }
//
//	func New() *MyPlugin {
//		return &MyPlugin{Base: hubkit.Base{Files: []protoreflect.FileDescriptor{hubv1.File_hub_v1_envelope_proto}}}
//	}
//
// 注意：只需列出**本插件自己定义**的消息所在的文件；中台侧对 google.protobuf.* 这类
// well-known 类型有豁免，不必把它们也带上。
type Base struct {
	Files []protoreflect.FileDescriptor
}

// Descriptor 实现 [Plugin.Descriptor]。
func (b Base) Descriptor() []byte {
	return DescriptorOf(b.Files...)
}

// DescriptorOf 把生成的 proto 文件变量打包成 FileDescriptorSet。
//
// 中台要的是编译产物而不是源码——它需要拿 descriptor 做字段级兼容检查，
// 而不是去解析 .proto 文本。
//
// 只打包列出的文件，不含它们的 import：中台侧只关心本插件自己定义的消息，
// 跨文件引用的类型名仍会原样出现在字段描述里，兼容性判断不受影响。
func DescriptorOf(files ...protoreflect.FileDescriptor) []byte {
	set := &descriptorpb.FileDescriptorSet{}
	for _, file := range files {
		if file == nil {
			continue
		}
		set.File = append(set.File, protodesc.ToFileDescriptorProto(file))
	}

	raw, err := proto.Marshal(set)
	if err != nil {
		// 走到这里说明生成的代码有问题，不是使用者的锅；让它尽早炸而不是静默传空
		panic("hubkit: 序列化 FileDescriptorSet 失败: " + err.Error())
	}
	return raw
}
