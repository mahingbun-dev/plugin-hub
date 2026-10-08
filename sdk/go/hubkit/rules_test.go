package hubkit_test

import (
	_ "embed"
	"encoding/json"
	"testing"

	"github.com/mahingbun-dev/plugin-hub/sdk/go/hubkit"
)

// 契约文件与 Rust 侧读的是同一份（crates/hub-grpc/tests/state_rules.rs 用 include_str!）。
// 放 testdata/ 是因为 //go:embed 要求文件在本包目录树内，而 Rust 的 include_str!
// 可以跨目录——反过来会让 Go 侧退化成运行时相对路径读取，工作目录一变就断。
//
//go:embed testdata/hub-rules.json
var rulesJSON []byte

type ruleCase struct {
	Rule  string `json:"rule"`
	Input string `json:"input"`
	Valid bool   `json:"valid"`
}

type ruleFile struct {
	StateTokenMetadata string `json:"stateTokenMetadata"`
	Gateway            struct {
		CallChainMeta        string `json:"callChainMeta"`
		MaxInvokeDepth       int    `json:"maxInvokeDepth"`
		InvokeQuotaKeyPrefix string `json:"invokeQuotaKeyPrefix"`
		InvokeQuotaPerMinute int    `json:"invokeQuotaPerMinute"`
	} `json:"gateway"`
	Rules map[string]struct {
		MaxBytes     int    `json:"maxBytes"`
		MinBytes     int    `json:"minBytes"`
		AllowedChars string `json:"allowedChars"`
	} `json:"rules"`
	Cases []ruleCase `json:"cases"`
}

// 每条规则的用例数下限。用下限而不是相等：加用例不必同时改两侧测试文件，
// 而删掉任何一条都会让计数掉到下界以下，从而判红。
const (
	minStateSegmentCases = 21
	minPluginNameCases   = 16
)

// loadRules 解析契约文件，并挡掉「文件被清空导致测试恒真」这种失效。
func loadRules(t *testing.T) ruleFile {
	t.Helper()

	var f ruleFile
	if err := json.Unmarshal(rulesJSON, &f); err != nil {
		t.Fatalf("契约文件无法解析: %v", err)
	}
	if len(f.Cases) == 0 {
		t.Fatal("契约文件里没有任何用例——测试会恒真，守卫形同虚设")
	}
	// rule 名只允许这两个。敲错的 rule（stateSegement、带空格、大小写不符）会让
	// 两侧的契约测试**同时**跳过该用例——checked 仍 > 0，一片绿，而这条用例从此
	// 不再守护任何东西。所以未知 rule 直接判红（也顺带挡住「加了新规则名却没实现」）。
	for _, c := range f.Cases {
		if c.Rule != "stateSegment" && c.Rule != "pluginName" {
			t.Fatalf("契约文件里出现未知 rule %q——名字敲错或新增规则未实现，"+
				"都会让该用例被两侧静默跳过", c.Rule)
		}
	}
	for _, name := range []string{"stateSegment", "pluginName"} {
		if _, ok := f.Rules[name]; !ok {
			t.Fatalf("契约文件缺少规则 %q", name)
		}
	}
	if f.StateTokenMetadata == "" {
		t.Fatal("契约文件缺少 stateTokenMetadata")
	}
	return f
}

func Test状态凭证键名与契约一致(t *testing.T) {
	f := loadRules(t)
	if hubkit.StateTokenMetadata != f.StateTokenMetadata {
		t.Fatalf("StateTokenMetadata = %q，契约文件写的是 %q——两侧必须一致",
			hubkit.StateTokenMetadata, f.StateTokenMetadata)
	}
}

// Test互调规则与契约一致 守住插件互调的三件事实：链的 meta 键、链深上限、
// 配额键前缀。SDK 不执行链校验（那是中台的事），但要往 meta 里带链、mock 要
// 按同一套规则做替身——键名或深度漂了，成环的请求就会一路绿灯打到下游。
func Test互调规则与契约一致(t *testing.T) {
	f := loadRules(t)

	if f.Gateway.CallChainMeta == "" {
		t.Fatal("契约文件缺少 gateway.callChainMeta")
	}
	if hubkit.CallChainMeta != f.Gateway.CallChainMeta {
		t.Fatalf("CallChainMeta = %q，契约文件写的是 %q——两侧必须一致",
			hubkit.CallChainMeta, f.Gateway.CallChainMeta)
	}
	if f.Gateway.MaxInvokeDepth <= 0 {
		t.Fatal("契约文件缺少 gateway.maxInvokeDepth")
	}
	if hubkit.MaxInvokeDepth != f.Gateway.MaxInvokeDepth {
		t.Fatalf("MaxInvokeDepth = %d，契约文件写的是 %d——两侧必须一致",
			hubkit.MaxInvokeDepth, f.Gateway.MaxInvokeDepth)
	}
	if f.Gateway.InvokeQuotaKeyPrefix == "" {
		t.Fatal("契约文件缺少 gateway.invokeQuotaKeyPrefix")
	}
}

func Test状态键规则符合契约(t *testing.T) {
	f := loadRules(t)

	checked := 0
	for _, c := range f.Cases {
		if c.Rule != "stateSegment" {
			continue
		}
		checked++
		if got := hubkit.ValidStateSegment(c.Input); got != c.Valid {
			t.Errorf("ValidStateSegment(%q) = %v，契约文件要求 %v", c.Input, got, c.Valid)
		}
	}
	if checked < minStateSegmentCases {
		t.Fatalf("stateSegment 用例数 = %d，少于下限 %d——删掉任何一条都会让守卫静默变弱",
			checked, minStateSegmentCases)
	}
}

func Test插件名规则符合契约(t *testing.T) {
	f := loadRules(t)

	checked := 0
	for _, c := range f.Cases {
		if c.Rule != "pluginName" {
			continue
		}
		checked++
		if got := hubkit.ValidPluginName(c.Input); got != c.Valid {
			t.Errorf("ValidPluginName(%q) = %v，契约文件要求 %v", c.Input, got, c.Valid)
		}
	}
	if checked < minPluginNameCases {
		t.Fatalf("pluginName 用例数 = %d，少于下限 %d——删掉任何一条都会让守卫静默变弱",
			checked, minPluginNameCases)
	}
}
