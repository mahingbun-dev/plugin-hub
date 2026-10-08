package hubkit

import (
	"context"
	"sync/atomic"
	"testing"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	"github.com/mahingbun-dev/plugin-hub/sdk/go/proto/hubv1"
)

// blockingStateClient 是一个永不返回、直到 ctx 结束的假 HubState 客户端。
//
// 用它而不是 mockhub：本用例验的是「客户端自己的超时」，需要一个会卡住的 peer，
// 而 mockhub 永远正常应答。实现接口比给 mockhub 加一个只有测试才用的「注入延迟」
// 旋钮更小，也不让 mock 多一个与真中台无关的开关。
type blockingStateClient struct {
	calls   atomic.Int32
	release chan struct{}

	// ctxCh 非 nil 时，block 会把每次调用收到的 ctx 投进来（供「原样透传」用例
	// 比对）。缓冲 1，非阻塞发送：只关心「最近一次」收到的 ctx。
	ctxCh chan context.Context
}

func newBlockingStateClient() *blockingStateClient {
	return &blockingStateClient{release: make(chan struct{})}
}

func (c *blockingStateClient) block(ctx context.Context) error {
	// 已死的 ctx 不该有 RPC 上路：真 gRPC 客户端在 ctx 已过期/取消时会直接失败，
	// 不会往线上放帧。所以先看 ctx、再计「到达对端的调用」——否则「过期 ctx 立刻
	// 失败」的用例里 calls 会记成 1，与真实链路的行为对不上。
	if err := ctx.Err(); err != nil {
		return status.FromContextError(err).Err()
	}

	c.calls.Add(1)
	if c.ctxCh != nil {
		select {
		case c.ctxCh <- ctx:
		default:
		}
	}

	select {
	case <-ctx.Done():
		// 与真实 gRPC 客户端一致：ctx 超时表现为 DeadlineExceeded 状态码，
		// 而不是裸的 context.DeadlineExceeded
		return status.FromContextError(ctx.Err()).Err()
	case <-c.release:
		return nil
	}
}

func (c *blockingStateClient) KvGet(ctx context.Context, _ *hubv1.KvGetRequest, _ ...grpc.CallOption) (*hubv1.KvGetResponse, error) {
	if err := c.block(ctx); err != nil {
		return nil, err
	}
	return &hubv1.KvGetResponse{}, nil
}

func (c *blockingStateClient) KvPut(ctx context.Context, _ *hubv1.KvPutRequest, _ ...grpc.CallOption) (*hubv1.KvPutResponse, error) {
	if err := c.block(ctx); err != nil {
		return nil, err
	}
	return &hubv1.KvPutResponse{}, nil
}

func (c *blockingStateClient) KvDelete(ctx context.Context, _ *hubv1.KvDeleteRequest, _ ...grpc.CallOption) (*hubv1.KvDeleteResponse, error) {
	if err := c.block(ctx); err != nil {
		return nil, err
	}
	return &hubv1.KvDeleteResponse{}, nil
}

func (c *blockingStateClient) KvScan(ctx context.Context, _ *hubv1.KvScanRequest, _ ...grpc.CallOption) (*hubv1.KvScanResponse, error) {
	if err := c.block(ctx); err != nil {
		return nil, err
	}
	return &hubv1.KvScanResponse{}, nil
}

func (c *blockingStateClient) Publish(ctx context.Context, _ *hubv1.PublishRequest, _ ...grpc.CallOption) (*hubv1.PublishResponse, error) {
	if err := c.block(ctx); err != nil {
		return nil, err
	}
	return &hubv1.PublishResponse{}, nil
}

// Test四个方法都受默认上限约束 是最要紧的一条：四个入口各自包一层，
// 漏掉任何一个都不会有编译期提示。
func Test四个方法都受默认上限约束(t *testing.T) {
	tests := []struct {
		name string
		call func(*StateClient, context.Context) error
	}{
		{"Get", func(c *StateClient, ctx context.Context) error {
			_, _, err := c.Get(ctx, "ns", "k")
			return err
		}},
		{"Put", func(c *StateClient, ctx context.Context) error {
			return c.Put(ctx, "ns", "k", []byte("v"), 0)
		}},
		{"Delete", func(c *StateClient, ctx context.Context) error {
			_, err := c.Delete(ctx, "ns", "k")
			return err
		}},
		{"Scan", func(c *StateClient, ctx context.Context) error {
			_, err := c.Scan(ctx, "ns", "", 10)
			return err
		}},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			fake := newBlockingStateClient()
			defer close(fake.release) // 放掉被卡住的调用，避免 -race 下留下悬挂 goroutine

			c := &StateClient{client: fake, callTimeout: 100 * time.Millisecond}

			start := time.Now()
			err := tt.call(c, context.Background())
			elapsed := time.Since(start)

			if status.Code(err) != codes.DeadlineExceeded {
				t.Fatalf("应超时并返回 DeadlineExceeded，实际 %v", err)
			}
			if elapsed > time.Second {
				t.Fatalf("应在上限附近返回，实际耗时 %v", elapsed)
			}
		})
	}
}

// Test父ctx的deadline更早时不延长 验「不延长」方向。
func Test父ctx的deadline更早时不延长(t *testing.T) {
	fake := newBlockingStateClient()
	defer close(fake.release)

	// 上限设得足够大，保证先撞到的一定是父 ctx 的 deadline
	c := &StateClient{client: fake, callTimeout: 30 * time.Second}
	ctx, cancel := context.WithTimeout(context.Background(), 50*time.Millisecond)
	defer cancel()

	start := time.Now()
	_, _, err := c.Get(ctx, "ns", "k")
	elapsed := time.Since(start)

	if status.Code(err) != codes.DeadlineExceeded {
		t.Fatalf("应返回 DeadlineExceeded，实际 %v", err)
	}
	if elapsed > time.Second {
		t.Fatalf("父 ctx 更早时不该被延长到 callTimeout，实际耗时 %v", elapsed)
	}
}

// Test父ctx的deadline更晚时按上限截断 验「截断」方向。
func Test父ctx的deadline更晚时按上限截断(t *testing.T) {
	fake := newBlockingStateClient()
	defer close(fake.release)

	c := &StateClient{client: fake, callTimeout: 100 * time.Millisecond}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	start := time.Now()
	_, _, err := c.Get(ctx, "ns", "k")
	elapsed := time.Since(start)

	if status.Code(err) != codes.DeadlineExceeded {
		t.Fatalf("应返回 DeadlineExceeded，实际 %v", err)
	}
	if elapsed > time.Second {
		t.Fatalf("父 ctx 更晚时应按 callTimeout 截断，实际耗时 %v", elapsed)
	}
}

// Test父ctx已过期时立刻失败 验已经过期的预算不该再等一个 callTimeout。
func Test父ctx已过期时立刻失败(t *testing.T) {
	fake := newBlockingStateClient()
	defer close(fake.release)

	c := &StateClient{client: fake, callTimeout: 30 * time.Second}
	ctx, cancel := context.WithDeadline(context.Background(), time.Now().Add(-time.Second))
	defer cancel()

	start := time.Now()
	_, _, err := c.Get(ctx, "ns", "k")
	elapsed := time.Since(start)

	if status.Code(err) != codes.DeadlineExceeded {
		t.Fatalf("已过期的 ctx 应返回 DeadlineExceeded，实际 %v", err)
	}
	if elapsed > 100*time.Millisecond {
		t.Fatalf("已过期的 ctx 应立刻失败，实际耗时 %v", elapsed)
	}
	// 名字里的「立刻失败」不只是快，更是**根本不发 RPC**：预算已经用完，
	// 没有理由再把一次注定死掉的调用放上线路。
	if got := fake.calls.Load(); got != 0 {
		t.Fatalf("已过期的 ctx 不该发起任何 RPC，实际调用 %d 次", got)
	}
}

// Test父ctx被取消时不按超时处理 验取消与超时是两种不同的错误。
func Test父ctx被取消时不按超时处理(t *testing.T) {
	fake := newBlockingStateClient()
	defer close(fake.release)

	c := &StateClient{client: fake, callTimeout: 30 * time.Second}
	ctx, cancel := context.WithCancel(context.Background())
	go func() {
		time.Sleep(50 * time.Millisecond)
		cancel()
	}()

	_, _, err := c.Get(ctx, "ns", "k")
	if status.Code(err) != codes.Canceled {
		t.Fatalf("父 ctx 取消应返回 Canceled 而不是超时，实际 %v", err)
	}
}

// Test超时不触发重注册 是关键的一条：DeadlineExceeded 不是 401，
// 不能把注册循环叫醒——否则中台一慢就会变成重注册风暴。
func Test超时不触发重注册(t *testing.T) {
	fake := newBlockingStateClient()
	defer close(fake.release)

	denied := make(chan struct{}, 1)
	c := &StateClient{client: fake, denied: denied, callTimeout: 50 * time.Millisecond}

	_, _, err := c.Get(context.Background(), "ns", "k")
	if status.Code(err) != codes.DeadlineExceeded {
		t.Fatalf("应超时，实际 %v", err)
	}

	select {
	case <-denied:
		t.Fatal("DeadlineExceeded 不该被当成 401 触发重新注册")
	default:
	}
}

// Test未设上限时原样透传ctx 覆盖 callTimeout <= 0 的分支：
// 测试里直接构造的客户端（以及未来可能的调用方）不该被强行截断。
//
// 名字里的「原样透传」是**真的**要求 ctx 被原封不动交给底层客户端，而不是
// 派生一个包了超时的新 ctx。判据用 Done 通道的同一性：WithTimeout 会派生出
// 一个新的 done 通道，所以「底层收到的 ctx 的 Done() 就是父 ctx 的 Done()」
// 只有原样透传才成立（值会被 WithTimeout 一并继承，单靠值区分不出来）。
func Test未设上限时原样透传ctx(t *testing.T) {
	fake := newBlockingStateClient()
	defer close(fake.release)

	type ctxMarkerKey struct{}
	parent := context.WithValue(context.Background(), ctxMarkerKey{}, "原样透传")
	parent, cancel := context.WithTimeout(parent, 50*time.Millisecond)
	defer cancel()

	fake.ctxCh = make(chan context.Context, 1)

	c := &StateClient{client: fake, callTimeout: 0}

	start := time.Now()
	_, _, err := c.Get(parent, "ns", "k")
	elapsed := time.Since(start)

	if status.Code(err) != codes.DeadlineExceeded {
		t.Fatalf("应返回 DeadlineExceeded，实际 %v", err)
	}
	if elapsed > time.Second {
		t.Fatalf("未设上限时应按父 ctx 的 deadline 结束，实际耗时 %v", elapsed)
	}

	select {
	case got := <-fake.ctxCh:
		if got.Value(ctxMarkerKey{}) != "原样透传" {
			t.Fatal("底层客户端收到的 ctx 丢了父 ctx 上的标记值")
		}
		if got.Done() != parent.Done() {
			t.Fatal("底层客户端收到的不是父 ctx 本身——callTimeout<=0 时不该派生新 ctx")
		}
	default:
		t.Fatal("底层客户端没有收到任何调用")
	}
}
