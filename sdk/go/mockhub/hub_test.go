package mockhub

import (
	"bytes"
	"context"
	"strings"
	"testing"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"

	"github.com/mahingbun-dev/anc-hub/sdk/go/hubkit"
	"github.com/mahingbun-dev/anc-hub/sdk/go/proto/hubv1"
)

// newStateClient 起一个 mock 中台，返回它、状态面客户端，以及一个已带上正确
// 凭证的 context。
//
// 键名取 hubkit.StateTokenMetadata 而不是再抄一份字面量：独立的字面量核对已经
// 上移进 hubkit/testdata/hub-rules.json（Rust 侧读同一份），同源即同判。
func newStateClient(t *testing.T, opts Options) (*Hub, hubv1.HubStateClient, context.Context) {
	t.Helper()

	hub, err := Start(opts)
	if err != nil {
		t.Fatalf("起 mock 失败: %v", err)
	}
	t.Cleanup(hub.Close)

	conn, err := grpc.NewClient(
		strings.TrimPrefix(hub.Addr(), "http://"),
		grpc.WithTransportCredentials(insecure.NewCredentials()),
	)
	if err != nil {
		t.Fatalf("连 mock 失败: %v", err)
	}
	t.Cleanup(func() { _ = conn.Close() })

	ctx := metadata.AppendToOutgoingContext(context.Background(), hubkit.StateTokenMetadata, hub.StateToken())
	return hub, hubv1.NewHubStateClient(conn), ctx
}

func kvKey(ns, key string) *hubv1.KvKey {
	return &hubv1.KvKey{Namespace: ns, Key: key}
}

func Test注册响应带状态凭证(t *testing.T) {
	hub, err := Start(Options{})
	if err != nil {
		t.Fatalf("起 mock 失败: %v", err)
	}
	defer hub.Close()

	conn, err := grpc.NewClient(
		strings.TrimPrefix(hub.Addr(), "http://"),
		grpc.WithTransportCredentials(insecure.NewCredentials()),
	)
	if err != nil {
		t.Fatalf("连 mock 失败: %v", err)
	}
	defer conn.Close()

	resp, err := hubv1.NewPluginRegistryClient(conn).Register(
		context.Background(),
		&hubv1.RegisterRequest{
			PluginName:    "state-test",
			Version:       "1.0.0",
			InstanceId:    "i-1",
			AdvertiseAddr: "http://127.0.0.1:9000",
		},
	)
	if err != nil {
		t.Fatalf("注册调用失败: %v", err)
	}
	if !resp.GetAccepted() {
		t.Fatalf("注册应被接受: %v", resp.GetRejections())
	}
	if resp.GetStateToken() == "" {
		t.Fatal("mock 中台必须下发状态凭证，否则插件侧的 StateClient 用不了")
	}
	if resp.GetStateToken() != hub.StateToken() {
		t.Fatal("响应里的凭证要与 StateToken() 一致，否则测试没法断言后续请求")
	}
}

func Test凭证经metadata透传且无效凭证被拒(t *testing.T) {
	hub, err := Start(Options{})
	if err != nil {
		t.Fatalf("起 mock 失败: %v", err)
	}
	defer hub.Close()

	conn, err := grpc.NewClient(
		strings.TrimPrefix(hub.Addr(), "http://"),
		grpc.WithTransportCredentials(insecure.NewCredentials()),
	)
	if err != nil {
		t.Fatalf("连 mock 失败: %v", err)
	}
	defer conn.Close()

	client := hubv1.NewHubStateClient(conn)
	key := &hubv1.KvKey{Namespace: "s", Key: "k"}

	// 不带凭证
	if _, err := client.KvGet(context.Background(), &hubv1.KvGetRequest{Key: key}); status.Code(err) != codes.Unauthenticated {
		t.Fatalf("无凭证应返回 Unauthenticated，实际 %v", err)
	}

	// 编造的凭证。
	//
	// 值必须是可打印 ASCII：gRPC 在客户端就校验 metadata（grpc/stream.go 的
	// ValidatePair → "non-printable ASCII characters"），非 ASCII 的凭证根本发不
	// 出去，会先在客户端炸成一个 Internal，测不到服务端的判定。
	badCtx := metadata.AppendToOutgoingContext(context.Background(), hubkit.StateTokenMetadata, "forged-state-token")
	if _, err := client.KvGet(badCtx, &hubv1.KvGetRequest{Key: key}); status.Code(err) != codes.Unauthenticated {
		t.Fatalf("无效凭证应返回 Unauthenticated，实际 %v", err)
	}

	// 正确凭证：能写能读
	ctx := metadata.AppendToOutgoingContext(context.Background(), hubkit.StateTokenMetadata, hub.StateToken())
	if _, err := client.KvPut(ctx, &hubv1.KvPutRequest{Key: key, Value: []byte("v")}); err != nil {
		t.Fatalf("正确凭证应能写入: %v", err)
	}
	got, err := client.KvGet(ctx, &hubv1.KvGetRequest{Key: key})
	if err != nil {
		t.Fatalf("正确凭证应能读取: %v", err)
	}
	if !got.GetFound() || string(got.GetValue()) != "v" {
		t.Fatalf("往返失败: found=%v value=%q", got.GetFound(), got.GetValue())
	}
}

// 下面几个用例对着真中台（crates/hub-grpc/src/state.rs）的语义逐条核对——
// mock 一旦比真中台宽松，问题就会推迟到上真环境才爆。
func Test状态面语义与中台一致(t *testing.T) {
	_, c, ctx := newStateClient(t, Options{})

	t.Run("读不存在的键：found=false 且不是错误", func(t *testing.T) {
		got, err := c.KvGet(ctx, &hubv1.KvGetRequest{Key: kvKey("s", "nope")})
		if err != nil {
			t.Fatalf("读不存在的键不该报错: %v", err)
		}
		if got.GetFound() {
			t.Fatal("键不存在时 found 必须是 false")
		}
	})

	t.Run("空值与键不存在是两回事", func(t *testing.T) {
		if _, err := c.KvPut(ctx, &hubv1.KvPutRequest{Key: kvKey("s", "empty"), Value: []byte{}}); err != nil {
			t.Fatalf("写入空值失败: %v", err)
		}
		got, err := c.KvGet(ctx, &hubv1.KvGetRequest{Key: kvKey("s", "empty")})
		if err != nil {
			t.Fatalf("读取失败: %v", err)
		}
		if !got.GetFound() || len(got.GetValue()) != 0 {
			t.Fatalf("空值必须 found=true：found=%v value=%q", got.GetFound(), got.GetValue())
		}
	})

	t.Run("删不存在的键：deleted=false 且不是错误", func(t *testing.T) {
		got, err := c.KvDelete(ctx, &hubv1.KvDeleteRequest{Key: kvKey("s", "nope")})
		if err != nil {
			t.Fatalf("删不存在的键不该报错: %v", err)
		}
		if got.GetDeleted() {
			t.Fatal("删不存在的键 deleted 必须是 false")
		}
	})

	t.Run("删除已存在的键：deleted=true 且之后读不到", func(t *testing.T) {
		if _, err := c.KvPut(ctx, &hubv1.KvPutRequest{Key: kvKey("s", "gone"), Value: []byte("v")}); err != nil {
			t.Fatalf("写入失败: %v", err)
		}
		got, err := c.KvDelete(ctx, &hubv1.KvDeleteRequest{Key: kvKey("s", "gone")})
		if err != nil {
			t.Fatalf("删除失败: %v", err)
		}
		if !got.GetDeleted() {
			t.Fatal("删已存在的键 deleted 必须是 true")
		}
		read, err := c.KvGet(ctx, &hubv1.KvGetRequest{Key: kvKey("s", "gone")})
		if err != nil {
			t.Fatalf("读取失败: %v", err)
		}
		if read.GetFound() {
			t.Fatal("删除后不该还能读到")
		}
	})

	t.Run("value 超过 1MB 拒绝", func(t *testing.T) {
		_, err := c.KvPut(ctx, &hubv1.KvPutRequest{
			Key:   kvKey("s", "big"),
			Value: bytes.Repeat([]byte("a"), 1024*1024+1),
		})
		if status.Code(err) != codes.InvalidArgument {
			t.Fatalf("超限 value 应返回 InvalidArgument，实际 %v", err)
		}
	})

	t.Run("ttl 为负拒绝", func(t *testing.T) {
		_, err := c.KvPut(ctx, &hubv1.KvPutRequest{Key: kvKey("s", "negttl"), Value: []byte("v"), TtlSeconds: -1})
		if status.Code(err) != codes.InvalidArgument {
			t.Fatalf("负 ttl 应返回 InvalidArgument，实际 %v", err)
		}
	})

	t.Run("namespace/key 非法字符拒绝", func(t *testing.T) {
		// 冒号破坏前缀结构，星号让扫描变成跨命名空间的模式匹配——都是漏洞不是功能
		cases := []struct {
			name string
			key  *hubv1.KvKey
		}{
			{"namespace 为空", kvKey("", "k")},
			{"key 为空", kvKey("s", "")},
			{"namespace 含冒号", kvKey("a:b", "k")},
			{"key 含星号", kvKey("s", "a*b")},
			{"key 含空格", kvKey("s", "a b")},
			{"key 含中文", kvKey("s", "键")},
		}
		for _, tc := range cases {
			if _, err := c.KvGet(ctx, &hubv1.KvGetRequest{Key: tc.key}); status.Code(err) != codes.InvalidArgument {
				t.Errorf("%s：应返回 InvalidArgument，实际 %v", tc.name, err)
			}
		}
	})

	t.Run("scan 只返回本命名空间且键名不带前缀", func(t *testing.T) {
		for k, v := range map[string]string{"p1": "v1", "p2": "v2", "other": "v3"} {
			if _, err := c.KvPut(ctx, &hubv1.KvPutRequest{Key: kvKey("scan", k), Value: []byte(v)}); err != nil {
				t.Fatalf("写入 %s 失败: %v", k, err)
			}
		}
		// 另一个命名空间的同名键不该被扫出来
		if _, err := c.KvPut(ctx, &hubv1.KvPutRequest{Key: kvKey("scan-other", "p1"), Value: []byte("邻居")}); err != nil {
			t.Fatalf("写入邻居命名空间失败: %v", err)
		}

		got, err := c.KvScan(ctx, &hubv1.KvScanRequest{Namespace: "scan", Prefix: "p", Limit: 10})
		if err != nil {
			t.Fatalf("扫描失败: %v", err)
		}
		if len(got.GetEntries()) != 2 {
			t.Fatalf("前缀 p 应命中 2 项，实际 %d 项: %v", len(got.GetEntries()), got.GetEntries())
		}
		want := []struct{ key, value string }{{"p1", "v1"}, {"p2", "v2"}}
		for i, w := range want {
			entry := got.GetEntries()[i]
			if entry.GetKey() != w.key || string(entry.GetValue()) != w.value {
				t.Fatalf("第 %d 项不对：得到 %q=%q，期望 %q=%q", i, entry.GetKey(), entry.GetValue(), w.key, w.value)
			}
		}

		// prefix 为空 = 扫整个命名空间
		all, err := c.KvScan(ctx, &hubv1.KvScanRequest{Namespace: "scan", Limit: 10})
		if err != nil {
			t.Fatalf("扫描失败: %v", err)
		}
		if len(all.GetEntries()) != 3 {
			t.Fatalf("空前缀应命中 3 项，实际 %d 项", len(all.GetEntries()))
		}

		// 没命中不是错误
		none, err := c.KvScan(ctx, &hubv1.KvScanRequest{Namespace: "scan", Prefix: "zzz", Limit: 10})
		if err != nil {
			t.Fatalf("扫不到不该报错: %v", err)
		}
		if len(none.GetEntries()) != 0 {
			t.Fatalf("不该命中任何项，实际 %d 项", len(none.GetEntries()))
		}

		// limit 是硬上限
		limited, err := c.KvScan(ctx, &hubv1.KvScanRequest{Namespace: "scan", Limit: 1})
		if err != nil {
			t.Fatalf("扫描失败: %v", err)
		}
		if len(limited.GetEntries()) != 1 {
			t.Fatalf("limit=1 应只返回 1 项，实际 %d 项", len(limited.GetEntries()))
		}
	})

	t.Run("scan 参数越界拒绝", func(t *testing.T) {
		if _, err := c.KvScan(ctx, &hubv1.KvScanRequest{Namespace: "scan", Limit: 0}); status.Code(err) != codes.InvalidArgument {
			t.Errorf("limit=0 应返回 InvalidArgument，实际 %v", err)
		}
		if _, err := c.KvScan(ctx, &hubv1.KvScanRequest{Namespace: "scan", Limit: 1001}); status.Code(err) != codes.InvalidArgument {
			t.Errorf("limit=1001 应返回 InvalidArgument，实际 %v", err)
		}
		if _, err := c.KvScan(ctx, &hubv1.KvScanRequest{Namespace: "坏命名空间", Limit: 10}); status.Code(err) != codes.InvalidArgument {
			t.Errorf("非法 namespace 应返回 InvalidArgument，实际 %v", err)
		}
		if _, err := c.KvScan(ctx, &hubv1.KvScanRequest{Namespace: "scan", Prefix: "a*", Limit: 10}); status.Code(err) != codes.InvalidArgument {
			t.Errorf("含通配符的 prefix 应返回 InvalidArgument，实际 %v", err)
		}
	})

	t.Run("Publish 明确未实现", func(t *testing.T) {
		_, err := c.Publish(ctx, &hubv1.PublishRequest{Target: "flow"})
		if status.Code(err) != codes.Unimplemented {
			t.Fatalf("Publish 应返回 Unimplemented（与中台一致），实际 %v", err)
		}
		// 中台对 Publish 根本不看凭证，mock 也不能更严——否则插件会以为
		// "不带凭证调 Publish 会先撞 401"，而上真环境才发现不是
		if _, err := c.Publish(context.Background(), &hubv1.PublishRequest{Target: "flow"}); status.Code(err) != codes.Unimplemented {
			t.Fatalf("无凭证的 Publish 也应返回 Unimplemented，实际 %v", err)
		}
	})
}

func Test写入的键带TTL到期后不再可见(t *testing.T) {
	hub, c, ctx := newStateClient(t, Options{})

	if _, err := c.KvPut(ctx, &hubv1.KvPutRequest{Key: kvKey("s", "ttl"), Value: []byte("v"), TtlSeconds: 1}); err != nil {
		t.Fatalf("写入失败: %v", err)
	}
	got, err := c.KvGet(ctx, &hubv1.KvGetRequest{Key: kvKey("s", "ttl")})
	if err != nil {
		t.Fatalf("读取失败: %v", err)
	}
	if !got.GetFound() {
		t.Fatal("有效期内的键必须读得到")
	}

	time.Sleep(1200 * time.Millisecond)

	got, err = c.KvGet(ctx, &hubv1.KvGetRequest{Key: kvKey("s", "ttl")})
	if err != nil {
		t.Fatalf("读取失败: %v", err)
	}
	if got.GetFound() {
		t.Fatal("过期的键必须读不到——mock 忽略 TTL 就比真中台宽松了")
	}
	scanned, err := c.KvScan(ctx, &hubv1.KvScanRequest{Namespace: "s", Limit: 10})
	if err != nil {
		t.Fatalf("扫描失败: %v", err)
	}
	if len(scanned.GetEntries()) != 0 {
		t.Fatalf("过期的键不该被扫出来，实际 %d 项", len(scanned.GetEntries()))
	}
	del, err := c.KvDelete(ctx, &hubv1.KvDeleteRequest{Key: kvKey("s", "ttl")})
	if err != nil {
		t.Fatalf("删除失败: %v", err)
	}
	if del.GetDeleted() {
		t.Fatal("过期等同不存在，删除应返回 deleted=false")
	}
	if snap := hub.StateKV(); len(snap) != 0 {
		t.Fatalf("过期的键不该留在快照里: %v", snap)
	}
}

func TestDenyStateToken时状态面一律拒绝(t *testing.T) {
	hub, c, ctx := newStateClient(t, Options{DenyStateToken: true})

	// 只拒绝校验，不是不下发：凭证照常给，插件的行为与"凭证被中台吊销"一致
	if hub.StateToken() == "" {
		t.Fatal("DenyStateToken 只影响校验，凭证仍应下发")
	}
	if _, err := c.KvGet(ctx, &hubv1.KvGetRequest{Key: kvKey("s", "k")}); status.Code(err) != codes.Unauthenticated {
		t.Fatalf("即便凭证正确也必须 Unauthenticated，实际 %v", err)
	}
	if _, err := c.KvPut(ctx, &hubv1.KvPutRequest{Key: kvKey("s", "k"), Value: []byte("v")}); status.Code(err) != codes.Unauthenticated {
		t.Fatalf("写入也必须 Unauthenticated，实际 %v", err)
	}
}

func TestStateKV返回副本(t *testing.T) {
	hub, c, ctx := newStateClient(t, Options{})

	if _, err := c.KvPut(ctx, &hubv1.KvPutRequest{Key: kvKey("s", "k"), Value: []byte("v")}); err != nil {
		t.Fatalf("写入失败: %v", err)
	}

	// mock 里插件名固定，键是 "插件名\x00命名空间\x00键"
	snap := hub.StateKV()
	const wantKey = "mock-plugin\x00s\x00k"
	if len(snap) != 1 || string(snap[wantKey]) != "v" {
		t.Fatalf("快照不对：%v", snap)
	}

	snap[wantKey][0] = 'X'
	if again := hub.StateKV(); string(again[wantKey]) != "v" {
		t.Fatal("StateKV 必须返回副本，改动不能影响 mock 内部状态")
	}
}
