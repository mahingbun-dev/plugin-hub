package hubkit

import (
	"crypto/tls"
	"testing"
)

// 这一条测的是「上限确实作用到了握手参数上」。只测 Validate 是不够的——
// 参数校验通过并不代表 dial 真的用了它。
func TestTLS上限按配置作用到握手参数(t *testing.T) {
	cases := []struct {
		name string
		in   string
		want uint16
	}{
		{"留空则跟随 Go 默认", "", 0},
		{"压到 1.2", "1.2", tls.VersionTLS12},
		{"锁到 1.3", "1.3", tls.VersionTLS13},
		{"两侧空白忽略", " 1.2 ", tls.VersionTLS12},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			got := tlsConfigFor(Config{HubAddr: "https://hub:8094", TLSMaxVersion: c.in}).MaxVersion
			if got != c.want {
				t.Fatalf("TLSMaxVersion=%q 应得 MaxVersion=%#x，实际 %#x", c.in, c.want, got)
			}
			// 下限始终是 1.2：这是既有行为，别被这个改动带跑
			if mn := tlsConfigFor(Config{HubAddr: "https://hub:8094"}).MinVersion; mn != tls.VersionTLS12 {
				t.Fatalf("MinVersion 应始终是 TLS1.2，实际 %#x", mn)
			}
		})
	}
}
