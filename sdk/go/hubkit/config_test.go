package hubkit_test

import (
	"strings"
	"testing"
	"time"

	"github.com/mahingbun-dev/plugin-hub/sdk/go/hubkit"
)

func Test状态调用上限有默认值(t *testing.T) {
	cfg := hubkit.Config{HubAddr: "http://x", AdvertiseAddr: "http://y"}.WithDefaults()
	if cfg.StateCallTimeout != hubkit.DefaultStateCallTimeout {
		t.Fatalf("StateCallTimeout 缺省应补成 %v，实际 %v",
			hubkit.DefaultStateCallTimeout, cfg.StateCallTimeout)
	}

	// 显式给的值不该被覆盖
	custom := 7 * time.Second
	cfg2 := hubkit.Config{
		HubAddr:          "http://x",
		AdvertiseAddr:    "http://y",
		StateCallTimeout: custom,
	}.WithDefaults()
	if cfg2.StateCallTimeout != custom {
		t.Fatalf("显式设置的值不该被覆盖，实际 %v", cfg2.StateCallTimeout)
	}
}

func TestTLS最高版本只接受1点2与1点3(t *testing.T) {
	base := func(v string) hubkit.Config {
		return hubkit.Config{HubAddr: "https://hub:8094", AdvertiseAddr: "http://p:9000", TLSMaxVersion: v}
	}
	for _, ok := range []string{"", "1.2", "1.3", " 1.2 "} {
		if err := base(ok).Validate(); err != nil {
			t.Fatalf("TLSMaxVersion=%q 应当通过校验，却报 %v", ok, err)
		}
	}
	for _, bad := range []string{"1.1", "TLS1.2", "2", "tls1.3"} {
		err := base(bad).Validate()
		if err == nil {
			t.Fatalf("TLSMaxVersion=%q 应当被拒", bad)
		}
		// 提示要能直接照做：既说明合法取值，也把收到的值回显出来
		if !strings.Contains(err.Error(), "1.2") || !strings.Contains(err.Error(), bad) {
			t.Fatalf("错误信息应给出合法取值并回显收到的值，实际：%v", err)
		}
	}
}
