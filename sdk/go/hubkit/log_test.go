package hubkit_test

import (
	"bytes"
	"context"
	"encoding/json"
	"log/slog"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/mahingbun-dev/anc-hub/sdk/go/hubkit"
	"github.com/mahingbun-dev/anc-hub/sdk/go/mockhub"
	"github.com/mahingbun-dev/anc-hub/sdk/go/proto/hubv1"
)

// syncBuffer 是可以边写边读的日志缓冲。
//
// 日志由插件自己的 goroutine 写：测试线程不能就这么读一个 bytes.Buffer，
// 那不是"偶尔读不到"，是数据竞争。
type syncBuffer struct {
	mu  sync.Mutex
	buf bytes.Buffer
}

func (b *syncBuffer) Write(p []byte) (int, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.buf.Write(p)
}

// lines 返回当前已落盘的原始日志行（slog 的 JSON handler 一条记录一行）。
func (b *syncBuffer) lines() []string {
	b.mu.Lock()
	defer b.mu.Unlock()
	var out []string
	for _, line := range strings.Split(b.buf.String(), "\n") {
		if strings.TrimSpace(line) != "" {
			out = append(out, line)
		}
	}
	return out
}

// records 把原始日志行按 JSON 解析开。
//
// 断言都走原始行 + 解析后的字段两条路：字段能验语义，原始行能验"人眼看到的是什么"
// ——被 JSON 转义过的换行在字段里已经还原了，只在原始行上露馅。
func (b *syncBuffer) records(t *testing.T) ([]string, []map[string]any) {
	t.Helper()
	raw := b.lines()
	parsed := make([]map[string]any, 0, len(raw))
	for _, line := range raw {
		var rec map[string]any
		if err := json.Unmarshal([]byte(line), &rec); err != nil {
			t.Fatalf("日志行不是合法 JSON：%v\n%s", err, line)
		}
		parsed = append(parsed, rec)
	}
	return raw, parsed
}

// byMsg 挑出指定 msg 的记录，连同它的原始行一起返回。
func byMsg(raw []string, parsed []map[string]any, msg string) ([]string, []map[string]any) {
	var outRaw []string
	var outParsed []map[string]any
	for i, rec := range parsed {
		if rec["msg"] == msg {
			outRaw = append(outRaw, raw[i])
			outParsed = append(outParsed, rec)
		}
	}
	return outRaw, outParsed
}

// waitForMsg 等到指定 msg 的记录攒够 n 条，或超时。
//
// 日志是插件自己的 goroutine 异步写出来的，"注册请求已被中台收到"不等于
// "这行日志已落盘"，所以必须等到条数而不是等到某个事件。也没法靠数总行数——
// 监听、启动、失败各占一行，那样等于把测试绑死在行序上。
func waitForMsg(t *testing.T, logs *syncBuffer, msg string, n int, timeout time.Duration) {
	t.Helper()

	count := func() int {
		got := 0
		for _, line := range logs.lines() {
			var rec map[string]any
			if json.Unmarshal([]byte(line), &rec) == nil && rec["msg"] == msg {
				got++
			}
		}
		return got
	}

	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		if count() >= n {
			return
		}
		time.Sleep(5 * time.Millisecond)
	}
	t.Fatalf("等到超时也没攒够 msg=%q 的日志：期望 %d 条，实际 %d 条\n%s",
		msg, n, count(), strings.Join(logs.lines(), "\n"))
}

// Test注册被拒时每条原因各占一行 锁住"不用任何工具就能读懂"这件事。
//
// 走的是真实路径：mock 中台拒绝注册 → registrar 循环 → 日志。直接调 logRegisterFailure
// 是测不到的——它是未导出的，而且那样就绕开了"这行日志真会这么打吗"。
func Test注册被拒时每条原因各占一行(t *testing.T) {
	logs := &syncBuffer{}
	hub := startHub(t, mockhub.Options{
		RejectRegister: func(*hubv1.RegisterRequest) []*hubv1.Rejection {
			return []*hubv1.Rejection{
				{
					Code:    hubv1.RejectCode_REJECT_CODE_BREAKING_CHANGE,
					Message: "相对版本 1.0.0 存在破坏性契约变更",
					Detail:  "wms.v1.Order 字段 sku（编号 1）已被删除",
				},
				{
					Code:    hubv1.RejectCode_REJECT_CODE_UNREACHABLE,
					Message: "插件地址 http://10.0.0.9:9000 不可达",
					// 刻意不给 detail：没有补充说明的那条也不该把行打残
				},
			}
		},
	})

	stop, _ := startPlugin(t, hub.Addr(), func(cfg *hubkit.Config) {
		cfg.Logger = slog.New(slog.NewJSONHandler(logs, nil))
	})
	defer stop()

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	// 两轮拒绝 = 4 条原因行 + 2 条收尾行，够断言且不必等太久
	if err := hub.WaitForRegistration(ctx, 2); err != nil {
		t.Fatal(err)
	}
	waitForMsg(t, logs, "中台拒绝了注册", 4, 3*time.Second)
	stop()

	raw, parsed := logs.records(t)
	reasonRaw, reasons := byMsg(raw, parsed, "中台拒绝了注册")

	if len(reasons) < 4 {
		t.Fatalf("两轮拒绝应有至少 4 条原因行，实际 %d 条\n%s", len(reasons), strings.Join(raw, "\n"))
	}

	for i, rec := range reasons {
		// 人眼这一层：一行就是一条原因，行里不能出现被转义的回车换行。
		// "\n" 这两个字符出现在原始行里，就说明有字段是多行文本硬塞进来的。
		if strings.Contains(reasonRaw[i], `\n`) || strings.Contains(reasonRaw[i], `\r`) {
			t.Errorf("原因行里出现了转义的换行，说明仍有多行文本被塞进字段:\n%s", reasonRaw[i])
		}
		for _, key := range []string{"code", "message", "detail"} {
			if _, ok := rec[key]; !ok {
				t.Errorf("原因行缺少 %s 字段:\n%s", key, reasonRaw[i])
			}
		}
		if got, _ := rec["message"].(string); strings.TrimSpace(got) == "" {
			t.Errorf("message 不该为空:\n%s", reasonRaw[i])
		}
	}

	// 两条原因各自独立成行：两条原因的内容不该挤在同一行里
	for _, code := range []string{"BREAKING_CHANGE", "UNREACHABLE"} {
		if !strings.Contains(strings.Join(reasonRaw, "\n"), code) {
			t.Errorf("原因行里应出现拒绝码 %s:\n%s", code, strings.Join(reasonRaw, "\n"))
		}
	}
	for _, line := range reasonRaw {
		if strings.Contains(line, "BREAKING_CHANGE") && strings.Contains(line, "UNREACHABLE") {
			t.Errorf("一条原因一行，这行里混进了两条:\n%s", line)
		}
	}

	var withDetail, withoutDetail int
	for _, rec := range reasons {
		if detail, _ := rec["detail"].(string); detail != "" {
			withDetail++
		} else {
			withoutDetail++
		}
	}
	if withDetail == 0 || withoutDetail == 0 {
		t.Errorf("带 detail 与不带 detail 的原因都应存在，实际 %d / %d", withDetail, withoutDetail)
	}

	// 收尾行：重试间隔必须是可读的时长，不是纳秒整数
	sumRaw, sums := byMsg(raw, parsed, "注册未通过，稍后重试")
	if len(sums) == 0 {
		t.Fatalf("被拒后应有一行说明接下来怎么办\n%s", strings.Join(raw, "\n"))
	}
	for i, rec := range sums {
		retry, ok := rec["retry_in"].(string)
		if !ok {
			t.Fatalf("retry_in 应是可读时长字符串，实际 %#v:\n%s", rec["retry_in"], sumRaw[i])
		}
		if retry != (20 * time.Millisecond).String() {
			t.Errorf("retry_in = %q，期望 %q", retry, (20 * time.Millisecond).String())
		}
		// 纯数字就是老毛病：5000000000 没人认得出是 5 秒
		if _, err := time.ParseDuration(retry); err != nil {
			t.Errorf("retry_in = %q 不是合法时长: %v", retry, err)
		}
	}
	// 原因行本身不该背着重试间隔——它是收尾行的事
	for _, line := range reasonRaw {
		if strings.Contains(line, "retry_in") {
			t.Errorf("原因行不该带 retry_in:\n%s", line)
		}
	}
}

// Test连不上中台时日志仍是单行 守住非 Rejected 的错误。
//
// 这类错误本来就只有一行，拆开或改格式只会把它改坏。
func Test连不上中台时日志仍是单行(t *testing.T) {
	logs := &syncBuffer{}

	// 挑一个没人监听的端口当中台：Register 会直接失败在连接上
	dead, err := mockhub.FreeAddr()
	if err != nil {
		t.Fatalf("挑端口失败: %v", err)
	}

	stop, _ := startPlugin(t, "http://"+dead, func(cfg *hubkit.Config) {
		cfg.Logger = slog.New(slog.NewJSONHandler(logs, nil))
	})
	defer stop()

	waitForMsg(t, logs, "注册未通过，稍后重试", 1, 5*time.Second)
	stop()

	raw, parsed := logs.records(t)
	_, failures := byMsg(raw, parsed, "注册未通过，稍后重试")
	if len(failures) == 0 {
		t.Fatalf("连不上中台也该报错\n%s", strings.Join(raw, "\n"))
	}

	rec := failures[0]
	if _, ok := rec["err"]; !ok {
		t.Fatalf("错误应原样带在 err 字段里:\n%s", strings.Join(raw, "\n"))
	}
	retry, ok := rec["retry_in"].(string)
	if !ok {
		t.Fatalf("retry_in 应是可读时长字符串，实际 %#v", rec["retry_in"])
	}
	if _, err := time.ParseDuration(retry); err != nil {
		t.Errorf("retry_in = %q 不是合法时长: %v", retry, err)
	}
	// 一行说完：没有"原因行"那样的多行拆分
	_, reasons := byMsg(raw, parsed, "中台拒绝了注册")
	if len(reasons) != 0 {
		t.Errorf("网络错误不该被当成拒绝原因:\n%s", strings.Join(raw, "\n"))
	}
}

// Test成功注册的日志保持原样 给"已经好用的那行"上一道保险。
func Test成功注册的日志保持原样(t *testing.T) {
	logs := &syncBuffer{}
	hub := startHub(t, mockhub.Options{})
	stop, _ := startPlugin(t, hub.Addr(), func(cfg *hubkit.Config) {
		cfg.Logger = slog.New(slog.NewJSONHandler(logs, nil))
	})
	defer stop()

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := hub.WaitForRegistration(ctx, 1); err != nil {
		t.Fatal(err)
	}
	// 成功那行可能还没 flush，等一等
	waitForMsg(t, logs, "已注册到中台", 1, 3*time.Second)
	stop()

	raw, parsed := logs.records(t)
	okRaw, okRecs := byMsg(raw, parsed, "已注册到中台")
	if len(okRecs) == 0 {
		t.Fatalf("成功注册应有日志\n%s", strings.Join(raw, "\n"))
	}
	if strings.Contains(okRaw[0], `\n`) {
		t.Errorf("成功日志不该有多行文本:\n%s", okRaw[0])
	}
	for _, key := range []string{"plugin", "version", "instance"} {
		if _, ok := okRecs[0][key]; !ok {
			t.Errorf("成功日志缺少 %s 字段:\n%s", key, okRaw[0])
		}
	}
}
