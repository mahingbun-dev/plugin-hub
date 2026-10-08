// plugin-hub 插件 SDK（Go）。
//
// 生成代码在 proto/hubv1/，由 generate.sh 从 crates/hub-proto 的 proto 产出并提交，
// 插件团队不需要装 protoc 就能用。
module github.com/mahingbun-dev/plugin-hub/sdk/go

// 放低语言版本下限，插件团队不必跟着我们的工具链走
go 1.25.0

require (
	google.golang.org/grpc v1.83.2
	google.golang.org/protobuf v1.36.12
)

require (
	golang.org/x/net v0.58.0 // indirect
	golang.org/x/sys v0.47.0 // indirect
	golang.org/x/text v0.41.0 // indirect
	google.golang.org/genproto/googleapis/rpc v0.0.0-20260526163538-3dc84a4a5aaa // indirect
)
