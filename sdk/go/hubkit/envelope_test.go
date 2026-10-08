package hubkit

import (
	"strings"
	"testing"
	"time"

	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/types/descriptorpb"
	"google.golang.org/protobuf/types/known/anypb"

	"github.com/mahingbun-dev/plugin-hub/sdk/go/proto/hubv1"
)

func TestPayloadJSONRoundTrip(t *testing.T) {
	payload := map[string]any{
		"orderId": "SO-2026-0001",
		"qty":     float64(12), // Struct 只有一种数值类型，取出即 float64
		"urgent":  true,
		"note":    nil,
		"items": []any{
			map[string]any{"sku": "A1"},
		},
	}

	env, err := WithPayloadJSON(&hubv1.Envelope{MessageId: "m-1"}, payload)
	if err != nil {
		t.Fatalf("打包失败: %v", err)
	}

	got, ok := PayloadJSON(env)
	if !ok {
		t.Fatal("应能取回 JSON 载荷")
	}
	if got["orderId"] != "SO-2026-0001" {
		t.Errorf("orderId = %v", got["orderId"])
	}
	if got["qty"] != float64(12) {
		t.Errorf("qty = %v（应为 float64(12)）", got["qty"])
	}
	if got["urgent"] != true {
		t.Errorf("urgent = %v", got["urgent"])
	}
}

func TestWithPayloadJSON不修改原信封(t *testing.T) {
	original := &hubv1.Envelope{MessageId: "m-1", TraceId: "t-1"}

	out, err := WithPayloadJSON(original, map[string]any{"a": 1})
	if err != nil {
		t.Fatalf("打包失败: %v", err)
	}

	if original.Payload != nil {
		t.Error("原信封不该被改动——链路里可能有别的持有者")
	}
	if out.MessageId != "m-1" || out.TraceId != "t-1" {
		t.Error("输出应保留原信封的追踪字段")
	}
}

func TestPayloadJSON对非Struct载荷返回false(t *testing.T) {
	// flow 内部传的是业务类型，插件应改用自己的类型去 Unmarshal
	env := &hubv1.Envelope{Payload: anypbWithType("type.googleapis.com/wms.v1.OrderCreated", []byte{1, 2, 3})}

	if _, ok := PayloadJSON(env); ok {
		t.Error("业务类型载荷不该被当成 JSON")
	}
}

func TestPayloadJSON对空载荷返回false(t *testing.T) {
	if _, ok := PayloadJSON(&hubv1.Envelope{}); ok {
		t.Error("没有载荷时应返回 false")
	}
	if _, ok := PayloadJSON(nil); ok {
		t.Error("nil 信封应返回 false 而不是 panic")
	}
}

func TestBudget与Expired(t *testing.T) {
	t.Run("未设置 deadline 时无预算", func(t *testing.T) {
		env := &hubv1.Envelope{DeadlineMs: 0}
		if _, ok := Budget(env); ok {
			t.Error("未设置 deadline 不该有预算")
		}
		if Expired(env) {
			t.Error("未设置 deadline 不算过期")
		}
	})

	t.Run("正常预算", func(t *testing.T) {
		env := &hubv1.Envelope{DeadlineMs: time.Now().Add(5 * time.Second).UnixMilli()}
		budget, ok := Budget(env)
		if !ok {
			t.Fatal("应有预算")
		}
		if budget <= 4*time.Second || budget > 5*time.Second {
			t.Errorf("预算约 5 秒，实际 %v", budget)
		}
		if Expired(env) {
			t.Error("未到期不该算过期")
		}
	})

	t.Run("已过期", func(t *testing.T) {
		env := &hubv1.Envelope{DeadlineMs: time.Now().Add(-time.Second).UnixMilli()}
		budget, ok := Budget(env)
		if !ok {
			t.Fatal("应识别出设置了 deadline")
		}
		if budget != 0 {
			t.Errorf("过期后预算应为 0，实际 %v", budget)
		}
		if !Expired(env) {
			t.Error("应判定为过期")
		}
	})
}

func Test校验响应构造(t *testing.T) {
	if !Valid().GetValid() {
		t.Error("Valid() 应构造通过")
	}

	resp := Invalid(Issue("payload.sku", "不能为空"), Warn("payload.note", "过长已截断"))
	if resp.GetValid() {
		t.Error("Invalid() 应构造不通过")
	}
	if len(resp.GetIssues()) != 2 {
		t.Fatalf("应有 2 条问题，实际 %d", len(resp.GetIssues()))
	}
	if resp.GetIssues()[0].GetSeverity() != hubv1.Severity_SEVERITY_ERROR {
		t.Error("Issue 应是错误级")
	}
	if resp.GetIssues()[1].GetSeverity() != hubv1.Severity_SEVERITY_WARNING {
		t.Error("Warn 应是警告级")
	}
	// path 要能定位到具体字段，agent 靠它改数据重试
	if resp.GetIssues()[0].GetPath() != "payload.sku" {
		t.Errorf("path = %q", resp.GetIssues()[0].GetPath())
	}
}

func TestDescriptorOf产出可解析的集合(t *testing.T) {
	raw := DescriptorOf(hubv1.File_hub_v1_envelope_proto)
	if len(raw) == 0 {
		t.Fatal("不该为空")
	}

	set := &descriptorpb.FileDescriptorSet{}
	if err := proto.Unmarshal(raw, set); err != nil {
		t.Fatalf("产出应能被解析回 FileDescriptorSet: %v", err)
	}
	if len(set.GetFile()) != 1 {
		t.Fatalf("应含 1 个文件，实际 %d", len(set.GetFile()))
	}
	if set.GetFile()[0].GetPackage() != "hub.v1" {
		t.Errorf("包名 = %q", set.GetFile()[0].GetPackage())
	}

	// 中台靠这份 descriptor 建立契约基线，消息必须真的在里面
	found := false
	for _, m := range set.GetFile()[0].GetMessageType() {
		if m.GetName() == "Envelope" {
			found = true
		}
	}
	if !found {
		t.Error("descriptor 里应有 Envelope 消息")
	}
}

func TestDescriptorOf跳过nil(t *testing.T) {
	raw := DescriptorOf(nil, hubv1.File_hub_v1_envelope_proto, nil)
	set := &descriptorpb.FileDescriptorSet{}
	if err := proto.Unmarshal(raw, set); err != nil {
		t.Fatalf("解析失败: %v", err)
	}
	if len(set.GetFile()) != 1 {
		t.Errorf("nil 应被跳过，实际 %d 个文件", len(set.GetFile()))
	}
}

func TestRegistrationRejected把拒绝原因排开(t *testing.T) {
	err := &RegistrationRejected{Rejections: []*hubv1.Rejection{
		{
			Code:    hubv1.RejectCode_REJECT_CODE_BREAKING_CHANGE,
			Message: "相对版本 1.0.0 存在破坏性契约变更",
			Detail:  "wms.v1.Order 字段 sku（编号 1）已被删除",
		},
		{
			Code:    hubv1.RejectCode_REJECT_CODE_UNREACHABLE,
			Message: "插件地址 http://10.0.0.9:9000 不可达",
		},
	}}

	text := err.Error()
	for _, want := range []string{
		"BREAKING_CHANGE",
		"相对版本 1.0.0 存在破坏性契约变更",
		"字段 sku（编号 1）已被删除",
		"UNREACHABLE",
		"http://10.0.0.9:9000",
	} {
		if !strings.Contains(text, want) {
			t.Errorf("拒绝信息里应包含 %q，实际:\n%s", want, text)
		}
	}
}

func TestRegistrationRejected无原因时也不空着(t *testing.T) {
	text := (&RegistrationRejected{}).Error()
	if !strings.Contains(text, "未给出原因") {
		t.Errorf("应说明没有原因，实际 %q", text)
	}
}

func TestRejectCodeName对未知码不崩(t *testing.T) {
	if got := RejectCodeName(hubv1.RejectCode(9999)); !strings.Contains(got, "未知") {
		t.Errorf("未知码应可读地表示，实际 %q", got)
	}
	if got := RejectCodeName(hubv1.RejectCode_REJECT_CODE_TOOL_CONFLICT); got != "TOOL_CONFLICT" {
		t.Errorf("已知码应去掉前缀，实际 %q", got)
	}
}

// anypbWithType 造一个指定 type_url 的 Any，用于验证「非 Struct 载荷」的分支。
func anypbWithType(typeURL string, value []byte) *anypb.Any {
	return &anypb.Any{TypeUrl: typeURL, Value: value}
}
