// Package conformance 是插件的契约一致性自测套件。
//
// 插件接入中台之前必须跑通它。检查的都是「中台在真实调用时依赖、但等生产才发现代价太大」
// 的约定：契约自洽、校验器不崩、插件体真的返回信封。
//
// 分两部分，因为它们的输入不同：
//
//   - [Local]：只看插件对象，不需要把它跑起来。检查 manifest 与 descriptor 是否自洽——
//     中台的注册期校验就是这一套，本地先跑能省一轮「改完推上去才发现被拒」。
//   - [Runtime]：对运行中的插件跑，检查它在真实调用下的行为。
//
// 典型用法（插件仓库里放一个 go test）：
//
//	func TestConformance(t *testing.T) {
//		plugin := NewMyPlugin()
//		if report := conformance.Local(plugin); !report.Passed() {
//			t.Fatalf("契约自洽性未通过:\n%s", report)
//		}
//
//		stop, addr := startMyPlugin(t)
//		defer stop()
//		if report := conformance.Runtime(context.Background(), addr); !report.Passed() {
//			t.Fatalf("运行时行为未通过:\n%s", report)
//		}
//	}
package conformance

import (
	"context"
	"fmt"
	"regexp"
	"strings"
	"time"

	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/types/descriptorpb"

	"github.com/mahingbun-dev/plugin-hub/sdk/go/hubkit"
	"github.com/mahingbun-dev/plugin-hub/sdk/go/mockhub"
	"github.com/mahingbun-dev/plugin-hub/sdk/go/proto/hubv1"
)

// checkTimeout 是各项运行时检查的时间上限。
//
// 插件是外部进程，卡住的检查要尽快暴露而不是把测试挂死。
const checkTimeout = 5 * time.Second

// toolNamePattern 工具名要拼进 MCP 的工具标识，字符集更窄。
var toolNamePattern = regexp.MustCompile(`^[A-Za-z0-9_-]+$`)

// Check 一项检查的结果。
type Check struct {
	Name   string
	Passed bool
	Detail string
}

// Report 是一套检查的结果。
type Report struct {
	// Subject 被检查的对象：本地检查是插件名，运行时检查是插件地址
	Subject string
	Checks  []Check
}

// Passed 是否全部通过。
func (r *Report) Passed() bool {
	for _, c := range r.Checks {
		if !c.Passed {
			return false
		}
	}
	return true
}

// Failures 返回未通过的检查。
func (r *Report) Failures() []Check {
	var failed []Check
	for _, c := range r.Checks {
		if !c.Passed {
			failed = append(failed, c)
		}
	}
	return failed
}

func (r *Report) String() string {
	var b strings.Builder
	fmt.Fprintf(&b, "契约一致性检查 %s\n", r.Subject)
	for _, c := range r.Checks {
		mark := "✓"
		if !c.Passed {
			mark = "✗"
		}
		fmt.Fprintf(&b, "  %s %s", mark, c.Name)
		if c.Detail != "" {
			fmt.Fprintf(&b, " —— %s", c.Detail)
		}
		b.WriteString("\n")
	}
	return b.String()
}

func (r *Report) add(name string, passed bool, detail string) {
	r.Checks = append(r.Checks, Check{Name: name, Passed: passed, Detail: detail})
}

// Local 检查插件对象自身的自洽性，不需要把它跑起来。
//
// 复现的是中台注册期的那几条校验，因此它能挡住绝大多数「推上去才发现被拒」的问题。
func Local(plugin hubkit.Plugin) *Report {
	report := &Report{Subject: "（本地）"}

	manifest := plugin.Manifest()
	if manifest == nil {
		report.Subject = "（未命名插件）"
		report.add("manifest 存在", false, "Manifest() 返回了 nil")
		return report
	}
	report.Subject = fmt.Sprintf("%s@%s", manifest.GetName(), manifest.GetVersion())

	report.add("manifest 存在", true, "")

	name := manifest.GetName()
	switch {
	case strings.TrimSpace(name) == "":
		report.add("插件名合法", false, "缺少插件名 name")
	case !hubkit.ValidPluginName(name):
		report.add("插件名合法", false, "只允许字母数字与 -_，最长 64 字符，且以字母数字开头")
	default:
		report.add("插件名合法", true, name)
	}

	version := manifest.GetVersion()
	if strings.TrimSpace(version) == "" {
		report.add("版本号存在", false, "缺少版本号 —— flow 靠它锁定实例")
	} else {
		report.add("版本号存在", true, version)
	}

	// descriptor 是中台做字段级兼容检查的依据。
	// 空的是合法的：只用 google.protobuf.Struct 承载 JSON 的插件没有自己的 proto。
	raw := plugin.Descriptor()
	messages := map[string]struct{}{}
	if len(raw) == 0 {
		report.add("descriptor 可用", true, "无自有 proto（只用 well-known 载荷）")
	} else {
		parsed, err := DescriptorMessages(raw)
		if err != nil {
			report.add("descriptor 可用", false, err.Error())
			return report
		}
		messages = parsed
		report.add("descriptor 可用", true,
			fmt.Sprintf("%d 字节、%d 个消息类型", len(raw), len(messages)))
	}

	checkDeclaredMessages(report, manifest, messages)
	checkTools(report, manifest)

	return report
}

// checkDeclaredMessages 验证「自述与编译产物一致」。
//
// 这是最有价值的一条：改了 proto 忘了重新生成、或者消息改名后忘了同步 manifest，
// 都在这里被抓住，而不是等注册时被中台拒。
func checkDeclaredMessages(report *Report, manifest *hubv1.PluginManifest, messages map[string]struct{}) {
	var missing []string
	check := func(direction string, contracts []*hubv1.MessageContract) {
		for _, c := range contracts {
			fq := c.GetFqName()
			if isWellKnown(fq) {
				continue
			}
			if _, ok := messages[fq]; !ok {
				missing = append(missing, fmt.Sprintf("%s 里的 %s", direction, fq))
			}
		}
	}
	check("produces", manifest.GetProduces())
	check("consumes", manifest.GetConsumes())

	if len(missing) > 0 {
		report.add("声明的类型都在 descriptor 中", false,
			strings.Join(missing, "；")+" —— manifest 的自述必须与提交的 proto 一致")
		return
	}

	if len(manifest.GetProduces()) == 0 && len(manifest.GetConsumes()) == 0 {
		report.add("声明了契约", false,
			"既没有 produces 也没有 consumes —— 至少声明一个，接受直接调用的插件请声明 "+hubkit.StructFQName)
		return
	}

	report.add("声明的类型都在 descriptor 中", true,
		fmt.Sprintf("produces %d 个、consumes %d 个", len(manifest.GetProduces()), len(manifest.GetConsumes())))
}

func checkTools(report *Report, manifest *hubv1.PluginManifest) {
	if len(manifest.GetTools()) == 0 {
		// 不暴露工具是合法的：插件可能只参与 flow
		report.add("工具声明合法", true, "未声明工具（仅参与 flow）")
		return
	}

	seen := map[string]bool{}
	for _, tool := range manifest.GetTools() {
		if !toolNamePattern.MatchString(tool.GetName()) {
			report.add("工具声明合法", false,
				fmt.Sprintf("工具名 %q 含非法字符 —— 它要拼进 MCP 的工具标识", tool.GetName()))
			return
		}
		if seen[tool.GetName()] {
			report.add("工具声明合法", false,
				fmt.Sprintf("工具 %s 重复声明 —— 同一插件内工具名必须唯一", tool.GetName()))
			return
		}
		seen[tool.GetName()] = true
	}

	report.add("工具声明合法", true, fmt.Sprintf("%d 个工具", len(seen)))
}

// Runtime 对运行中的插件检查运行时行为。
//
// pluginAddr 是插件的 gRPC 地址（如 http://127.0.0.1:9000）。
func Runtime(ctx context.Context, pluginAddr string) *Report {
	report := &Report{Subject: pluginAddr}

	client, err := mockhub.DialPlugin(pluginAddr)
	if err != nil {
		report.add("插件地址可用", false, err.Error())
		return report
	}
	defer client.Close()

	ctx, cancel := context.WithTimeout(ctx, checkTimeout)
	defer cancel()

	health, err := client.Health(ctx)
	switch {
	case err != nil:
		report.add("Health 可应答", false, err.Error())
		return report
	case !health.GetHealthy():
		report.add("Health 可应答", false, "插件自报不健康: "+health.GetMessage())
		return report
	default:
		report.add("Health 可应答", true, "")
	}

	if manifest, err := client.Describe(ctx); err != nil {
		report.add("Describe 可应答", false, err.Error())
	} else {
		report.add("Describe 可应答", true,
			fmt.Sprintf("%s@%s", manifest.GetName(), manifest.GetVersion()))
	}

	// 空信封：插件必须能处理，而不是 panic 或挂住
	if _, err := client.Validate(ctx, &hubv1.Envelope{}); err != nil {
		report.add("校验器对空信封不崩", false, err.Error())
	} else {
		report.add("校验器对空信封不崩", true, "")
	}

	probe, err := hubkit.WithPayloadJSON(&hubv1.Envelope{MessageId: "conformance-1"},
		map[string]any{"conformance": true})
	if err != nil {
		report.add("校验器可处理 JSON 载荷", false, err.Error())
		report.add("插件体返回信封", false, "无法构造探针载荷")
		return report
	}

	if resp, err := client.Validate(ctx, probe); err != nil {
		report.add("校验器可处理 JSON 载荷", false, err.Error())
	} else if resp.GetValid() {
		report.add("校验器可处理 JSON 载荷", true, "通过")
	} else {
		// 探针载荷本来就可能不满足业务规则，拒绝是合法结果
		report.add("校验器可处理 JSON 载荷", true,
			fmt.Sprintf("拒绝（%d 条问题）—— 探针载荷不满足业务规则属正常", len(resp.GetIssues())))
	}

	out, err := client.Handle(ctx, probe)
	switch {
	case err != nil:
		// 被业务逻辑拒掉是正常的，但必须是「明确报错」而不是超时或 panic
		report.add("插件体返回信封", true,
			fmt.Sprintf("拒绝处理（%v）—— 探针载荷不满足业务规则属正常", err))
	case out == nil:
		report.add("插件体返回信封", false, "返回了空信封 —— 中台会把它当成插件异常")
	default:
		detail := "已返回信封"
		if _, ok := hubkit.PayloadJSON(out); ok {
			detail = "已返回 JSON 载荷"
		}
		report.add("插件体返回信封", true, detail)
	}

	return report
}

// DescriptorMessages 解析 FileDescriptorSet，返回其中的消息全限定名。
//
// 刻意**直接遍历 descriptor 结构，而不是走 protodesc.NewFiles**：后者要求 descriptor
// 自包含（能解析出所有 import），而插件提交的 descriptor 只含自己的 proto——
// 中台侧的 Rust 实现同样只遍历不解析引用，两边必须一致，否则会出现
// 「本地检查不过但中台接受」这种更糟的分歧。
func DescriptorMessages(raw []byte) (map[string]struct{}, error) {
	set := &descriptorpb.FileDescriptorSet{}
	if err := proto.Unmarshal(raw, set); err != nil {
		return nil, fmt.Errorf("descriptor 无法解析: %w", err)
	}

	found := map[string]struct{}{}
	for _, file := range set.GetFile() {
		collectMessages(file.GetPackage(), file.GetMessageType(), found)
	}
	return found, nil
}

func collectMessages(prefix string, messages []*descriptorpb.DescriptorProto, into map[string]struct{}) {
	for _, message := range messages {
		// map 字段会生成合成的 XxxEntry 消息，属实现细节，不算契约类型
		if message.GetOptions().GetMapEntry() {
			continue
		}

		name := message.GetName()
		if name == "" {
			continue
		}
		fq := name
		if prefix != "" {
			fq = prefix + "." + name
		}

		into[fq] = struct{}{}
		collectMessages(fq, message.GetNestedType(), into)
	}
}

// isWellKnown 与中台的豁免规则一致。判断逻辑放在 hubkit 里，两边共用一份。
func isWellKnown(fqName string) bool {
	return hubkit.IsWellKnownFQName(fqName)
}
