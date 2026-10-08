package conformance_test

import (
	"context"
	"strings"
	"testing"

	"google.golang.org/protobuf/reflect/protoreflect"

	"github.com/mahingbun-dev/anc-hub/sdk/go/conformance"
	"github.com/mahingbun-dev/anc-hub/sdk/go/hubkit"
	"github.com/mahingbun-dev/anc-hub/sdk/go/mockhub"
	"github.com/mahingbun-dev/anc-hub/sdk/go/proto/hubv1"
)

// goodPlugin 是一份合规的插件：契约自洽、工具名合法、声明的类型都在 descriptor 里。
type goodPlugin struct {
	hubkit.Base
}

func newGoodPlugin() *goodPlugin {
	return &goodPlugin{Base: hubkit.Base{
		Files: []protoreflect.FileDescriptor{hubv1.File_hub_v1_envelope_proto},
	}}
}

func (p *goodPlugin) Manifest() *hubv1.PluginManifest {
	return &hubv1.PluginManifest{
		Name:    "good-plugin",
		Version: "1.0.0",
		// envelope.proto 里确实有 Envelope 这个消息
		Produces: []*hubv1.MessageContract{{FqName: "hub.v1.Envelope"}},
		Consumes: []*hubv1.MessageContract{{FqName: hubkit.StructFQName}},
		Tools:    []*hubv1.ToolDecl{{Name: "do_thing", Description: "干点什么"}},
	}
}

func (p *goodPlugin) Validate(context.Context, *hubv1.Envelope) (*hubv1.ValidateResponse, error) {
	return hubkit.Valid(), nil
}

func (p *goodPlugin) Handle(_ context.Context, env *hubv1.Envelope) (*hubv1.Envelope, error) {
	return env, nil
}

func Test合规插件本地检查全过(t *testing.T) {
	report := conformance.Local(newGoodPlugin())
	if !report.Passed() {
		t.Fatalf("应全部通过:\n%s", report)
	}
	if report.Subject != "good-plugin@1.0.0" {
		t.Errorf("Subject = %q", report.Subject)
	}
}

func Test本地检查能抓出各类问题(t *testing.T) {
	tests := []struct {
		name      string
		mutate    func(*hubv1.PluginManifest)
		wantCheck string
		wantIn    string
	}{
		{
			name:      "声明了 descriptor 里没有的类型",
			mutate:    func(m *hubv1.PluginManifest) { m.Produces = []*hubv1.MessageContract{{FqName: "wms.v1.不存在"}} },
			wantCheck: "声明的类型都在 descriptor 中",
			wantIn:    "wms.v1.不存在",
		},
		{
			name: "消息改名后忘了同步 manifest",
			mutate: func(m *hubv1.PluginManifest) {
				m.Produces = []*hubv1.MessageContract{{FqName: "hub.v1.Envelop"}} // 少一个 e
			},
			wantCheck: "声明的类型都在 descriptor 中",
			wantIn:    "hub.v1.Envelop",
		},
		{
			name: "既没有 produces 也没有 consumes",
			mutate: func(m *hubv1.PluginManifest) {
				m.Produces = nil
				m.Consumes = nil
			},
			wantCheck: "声明了契约",
			wantIn:    hubkit.StructFQName,
		},
		{
			name:      "插件名含非法字符",
			mutate:    func(m *hubv1.PluginManifest) { m.Name = "有中文的名字" },
			wantCheck: "插件名合法",
			wantIn:    "只允许字母数字",
		},
		{
			name:      "缺少版本号",
			mutate:    func(m *hubv1.PluginManifest) { m.Version = "  " },
			wantCheck: "版本号存在",
			wantIn:    "版本号",
		},
		{
			name: "工具重名",
			mutate: func(m *hubv1.PluginManifest) {
				m.Tools = []*hubv1.ToolDecl{{Name: "dup"}, {Name: "dup"}}
			},
			wantCheck: "工具声明合法",
			wantIn:    "重复声明",
		},
		{
			name: "工具名含非法字符",
			mutate: func(m *hubv1.PluginManifest) {
				m.Tools = []*hubv1.ToolDecl{{Name: "查询订单"}}
			},
			wantCheck: "工具声明合法",
			wantIn:    "非法字符",
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			plugin := &mutatedPlugin{goodPlugin: newGoodPlugin(), mutate: tc.mutate}
			report := conformance.Local(plugin)

			if report.Passed() {
				t.Fatal("应检查出问题")
			}
			failures := report.Failures()
			if failures[0].Name != tc.wantCheck {
				t.Errorf("失败的检查项 = %q，期望 %q\n%s", failures[0].Name, tc.wantCheck, report)
			}
			if !strings.Contains(failures[0].Detail, tc.wantIn) {
				t.Errorf("原因里应包含 %q，实际 %q", tc.wantIn, failures[0].Detail)
			}
		})
	}
}

// mutatedPlugin 把 goodPlugin 的 manifest 按需改坏。
type mutatedPlugin struct {
	goodPlugin *goodPlugin
	mutate     func(*hubv1.PluginManifest)
}

func (p *mutatedPlugin) Manifest() *hubv1.PluginManifest {
	m := p.goodPlugin.Manifest()
	p.mutate(m)
	return m
}

func (p *mutatedPlugin) Descriptor() []byte { return p.goodPlugin.Descriptor() }

func (p *mutatedPlugin) Validate(ctx context.Context, env *hubv1.Envelope) (*hubv1.ValidateResponse, error) {
	return p.goodPlugin.Validate(ctx, env)
}

func (p *mutatedPlugin) Handle(ctx context.Context, env *hubv1.Envelope) (*hubv1.Envelope, error) {
	return p.goodPlugin.Handle(ctx, env)
}

func Test没有自有proto的插件也能通过本地检查(t *testing.T) {
	// 只用 google.protobuf.Struct 承载 JSON 的插件没有自己的 .proto——
	// 这是直接响应 agent 调用的最简形态，必须能通过
	plugin := &structOnlyPlugin{}

	report := conformance.Local(plugin)
	if !report.Passed() {
		t.Fatalf("只用 well-known 载荷的插件应通过:\n%s", report)
	}
}

func Test空descriptor却声明自有类型被指出(t *testing.T) {
	plugin := &structOnlyPlugin{declareOwnType: true}

	report := conformance.Local(plugin)
	if report.Passed() {
		t.Fatal("声明了自有类型却没有 descriptor 应被指出")
	}

	failures := report.Failures()
	if failures[0].Name != "声明的类型都在 descriptor 中" {
		t.Errorf("失败项 = %q\n%s", failures[0].Name, report)
	}
	if !strings.Contains(failures[0].Detail, "wms.v1.不存在") {
		t.Errorf("原因里应点出是哪个类型，实际 %q", failures[0].Detail)
	}
}

// structOnlyPlugin 模拟脚手架生成的那种插件：没有自有 proto。
type structOnlyPlugin struct {
	declareOwnType bool
}

func (p *structOnlyPlugin) Manifest() *hubv1.PluginManifest {
	m := &hubv1.PluginManifest{
		Name:    "struct-only",
		Version: "0.1.0",
		Consumes: []*hubv1.MessageContract{
			{FqName: hubkit.StructFQName},
		},
	}
	if p.declareOwnType {
		m.Produces = []*hubv1.MessageContract{{FqName: "wms.v1.不存在"}}
	}
	return m
}

func (p *structOnlyPlugin) Descriptor() []byte { return hubkit.DescriptorOf() }

func (p *structOnlyPlugin) Validate(context.Context, *hubv1.Envelope) (*hubv1.ValidateResponse, error) {
	return hubkit.Valid(), nil
}

func (p *structOnlyPlugin) Handle(_ context.Context, env *hubv1.Envelope) (*hubv1.Envelope, error) {
	return env, nil
}

func Test运行时检查对真实插件通过(t *testing.T) {
	hub, err := mockhub.Start(mockhub.Options{})
	if err != nil {
		t.Fatalf("启动 mock 中台失败: %v", err)
	}
	defer hub.Close()

	addr, err := mockhub.FreeAddr()
	if err != nil {
		t.Fatalf("挑端口失败: %v", err)
	}

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	done := make(chan struct{})
	go func() {
		defer close(done)
		_ = hubkit.RunContext(ctx, newGoodPlugin(), hubkit.Config{
			HubAddr:       hub.Addr(),
			AdvertiseAddr: "http://" + addr,
			ListenAddr:    addr,
		})
	}()

	if err := hub.WaitForRegistration(ctx, 1); err != nil {
		t.Fatal(err)
	}

	report := conformance.Runtime(ctx, "http://"+addr)
	if !report.Passed() {
		t.Fatalf("运行时检查应通过:\n%s", report)
	}

	cancel()
	<-done
}

func Test运行时检查对不可达地址如实报错(t *testing.T) {
	report := conformance.Runtime(context.Background(), "http://127.0.0.1:1")
	if report.Passed() {
		t.Fatal("不可达应报错")
	}
	// gRPC 的拨号是惰性的，连不上要到第一次 RPC 才暴露——所以首个失败项是 Health
	if got := report.Failures()[0].Name; got != "Health 可应答" {
		t.Errorf("首个失败项 = %q\n%s", got, report)
	}
}
