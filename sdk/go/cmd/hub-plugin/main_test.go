package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func Test插件名校验(t *testing.T) {
	valid := []string{"a", "order-reader", "wms2", "a-b-c", "x1"}
	for _, name := range valid {
		if !isValidName(name) {
			t.Errorf("%q 应合法", name)
		}
	}

	invalid := []string{"", "-lead", "trail-", "UPPER", "有中文", "has space", "a_b"}
	for _, name := range invalid {
		if isValidName(name) {
			t.Errorf("%q 应非法", name)
		}
	}
}

func Test选项可以出现在插件名之后(t *testing.T) {
	// 这是踩过的坑：标准库 flag 遇到第一个位置参数就停止解析，
	// 于是 `new my-plugin --dir /tmp/x` 里的 --dir 会被静默忽略
	opts, rest, err := parseNewArgs([]string{"order-reader", "--dir", "/tmp/x"})
	if err != nil {
		t.Fatalf("解析失败: %v", err)
	}
	if opts.dir != "/tmp/x" {
		t.Errorf("dir = %q，期望 /tmp/x", opts.dir)
	}
	if len(rest) != 1 || rest[0] != "order-reader" {
		t.Errorf("rest = %v", rest)
	}
}

func Test选项也可以出现在插件名之前(t *testing.T) {
	opts, rest, err := parseNewArgs([]string{"--dir", "/tmp/x", "order-reader"})
	if err != nil {
		t.Fatalf("解析失败: %v", err)
	}
	if opts.dir != "/tmp/x" || len(rest) != 1 {
		t.Errorf("dir = %q, rest = %v", opts.dir, rest)
	}
}

func Test等号形式的选项(t *testing.T) {
	opts, rest, err := parseNewArgs([]string{"order-reader", "--dir=/tmp/x", "--module=example.com/foo"})
	if err != nil {
		t.Fatalf("解析失败: %v", err)
	}
	if opts.dir != "/tmp/x" {
		t.Errorf("dir = %q", opts.dir)
	}
	if opts.module != "example.com/foo" {
		t.Errorf("module = %q", opts.module)
	}
	if len(rest) != 1 {
		t.Errorf("rest = %v", rest)
	}
}

func Test缺少取值的选项报错(t *testing.T) {
	if _, _, err := parseNewArgs([]string{"order-reader", "--dir"}); err == nil {
		t.Error("--dir 缺取值应报错")
	}
}

func Test未知选项报错而不是被忽略(t *testing.T) {
	_, _, err := parseNewArgs([]string{"order-reader", "--nope"})
	if err == nil {
		t.Fatal("未知选项应报错——静默忽略会让人以为生效了")
	}
	if !strings.Contains(err.Error(), "nope") {
		t.Errorf("错误信息应点出是哪个选项，实际 %v", err)
	}
}

func Test生成完整的插件工程(t *testing.T) {
	target := filepath.Join(t.TempDir(), "my-plugin")

	if err := generate(target, scaffoldValues("my-plugin", "example.com/my-plugin", SDKModule)); err != nil {
		t.Fatalf("生成失败: %v", err)
	}

	for _, name := range []string{
		"go.mod", "main.go", "plugin.go", "plugin_test.go", "README.md", "Dockerfile", ".gitignore",
	} {
		if _, err := os.Stat(filepath.Join(target, name)); err != nil {
			t.Errorf("缺少 %s: %v", name, err)
		}
	}

	// 关键内容要真的被替换进去了
	goMod := readFile(t, filepath.Join(target, "go.mod"))
	if !strings.Contains(goMod, "module example.com/my-plugin") {
		t.Errorf("go.mod 的 module 没替换:\n%s", goMod)
	}
	if !strings.Contains(goMod, SDKModule) {
		t.Error("go.mod 应引用本 SDK")
	}

	plugin := readFile(t, filepath.Join(target, "plugin.go"))
	if !strings.Contains(plugin, `Name:        "my-plugin"`) {
		t.Error("plugin.go 里的插件名没替换")
	}

	readme := readFile(t, filepath.Join(target, "README.md"))
	if !strings.Contains(readme, "# my-plugin") {
		t.Error("README 标题没替换")
	}
}

// Test生成的README带接入四道关checklist 守住「脚手架一生成就有接入路径」这件事。
//
// 新开发者不知道插件接入要过四关、也不知道每关怎么验——工具其实都在仓库里，缺的是路径。
// README 是唯一会跟着工程被带走的那份说明，所以四道关与每关的命令必须真在它里面。
func Test生成的README带接入四道关checklist(t *testing.T) {
	target := filepath.Join(t.TempDir(), "my-plugin")

	if err := generate(target, scaffoldValues("my-plugin", "example.com/my-plugin", SDKModule)); err != nil {
		t.Fatalf("生成失败: %v", err)
	}

	readme := readFile(t, filepath.Join(target, "README.md"))

	// 四道关按顺序都在
	for _, want := range []string{
		"L1 契约自洽",
		"L2 运行时自检",
		"L3 中台接受注册",
		"L4 端到端调用",
	} {
		if !strings.Contains(readme, want) {
			t.Errorf("README 缺少 %q", want)
		}
	}

	// 最关键的认知：不写清楚，人就会拿 L1、L2 当交付判据
	if !strings.Contains(readme, "L1、L2 全绿不等于能接入") {
		t.Error("README 应点明 L1、L2 全绿不等于能接入")
	}

	// L3 的结论在日志里，所以成功与被拒两种日志样例都得给出
	if !strings.Contains(readme, `"msg":"已注册到中台"`) {
		t.Error("README 应给出 L3 成功时的日志样例")
	}
	if !strings.Contains(readme, "注册未通过，稍后重试") {
		t.Error("README 应给出 L3 被拒时的日志样例")
	}
	// 拒绝原因是逐条独立成行的（code/message/detail），不是塞进一个字段里转义成 \n。
	// 样例必须跟着 hubkit 的实际输出走，否则指南教的格式跟日志对不上。
	if !strings.Contains(readme, `"msg":"中台拒绝了注册"`) {
		t.Error("README 应给出 L3 逐条原因格式的日志样例")
	}
	// 样例里的 detail 必须是实测抓的那一条：它把出路（按原编号原类型改回）也写在里面。
	// 旧的短文本只到「字段 sku（编号 1）已被删除」，跟 hubkit 实际打印的对不上。
	if !strings.Contains(readme, "基线版本里已有的字段不能删、改类型或改编号，只能新增") {
		t.Error("L3 被拒日志样例的 detail 应与 hubkit 的实际输出一致（含「基线版本里已有的字段不能删…」）")
	}
	if strings.Contains(readme, `"retry_in":5000000000`) {
		t.Error("README 里是旧的 retry_in 纳秒整数格式，应改为 \"5s\" 字符串")
	}

	// 最大的坑要能自查，而不只是一句警告
	if !strings.Contains(readme, "HUB_ADVERTISE_ADDR") {
		t.Error("README 应讲清 HUB_ADVERTISE_ADDR")
	}

	// BREAKING_CHANGE 那条最容易被写成听上去合理的错话：它只在新版本号上才会触发
	// （同号不一致是 VERSION_CONFLICT 拦的），所以「升版本号」在这里是死路。
	// 锁住正确说法，免得日后被「改回去」。
	if !strings.Contains(readme, "按原编号、原类型改回") {
		t.Error("BREAKING_CHANGE 的出路应是「把字段按原编号原类型改回」，不是升版本号")
	}
	if strings.Contains(readme, "有意删的就升版本号") {
		t.Error("README 里又出现了被证伪的建议：撞上 BREAKING_CHANGE 时版本号已经是新的了")
	}

	// L4 要能一眼看出过关与假绿的区别：200 + 非 JSON 也算失败
	if !strings.Contains(readme, "端到端跑通了") {
		t.Error("README 应给出 L4 跑通的输出样例")
	}
	if !strings.Contains(readme, "响应不是 JSON") {
		t.Error("README 应点明 L4 的假绿规则：200 但响应不是 JSON 判失败")
	}

	// L4 的命令签名是硬约定，且插件名位置参数要随工程变化
	if !strings.Contains(readme, "hubprobe e2e https://hub.example.com:8081/hub-api my-plugin") {
		t.Error("README 的 e2e 命令应带上本工程的插件名")
	}

	// hubprobe 在 SDK 里，命令要按 go.mod 的 replace 写成能直接跑的形态
	if !strings.Contains(readme, SDKModule+"/cmd/hubprobe") {
		t.Errorf("README 应给出 go run %s/cmd/hubprobe 形式的命令", SDKModule)
	}

	// 本地中台的 HTTP 面是 8092，不是 8095。8095 只在 UAT 主机内部成立（8092 在 UAT 上
	// 被 anc 平台后端占了），把它当命令参数写出来会让读者从自己机器上连自己、直接
	// connection refused——这条已经踩过一次，锁住。
	// 只禁「当成命令参数用」：正文里拿它当反面例子提醒「别这么写」是允许的。
	if strings.Contains(readme, "e2e http://127.0.0.1:8095") {
		t.Error("README 的 e2e 示例把 UAT 主机内部的 8095 当成了本机可连地址；本地 HTTP 面是 127.0.0.1:8092")
	}
	if !strings.Contains(readme, "http://127.0.0.1:8092") {
		t.Error("README 应给出本地中台 HTTP 面的地址 http://127.0.0.1:8092")
	}
	// UAT 得按调用者视角写成 nginx 的域名形式，而不是回环地址
	if !strings.Contains(readme, "https://hub.example.com:8081/hub-api") {
		t.Error("README 应给出 UAT 的调用者地址（经 nginx 的 https://hub.example.com:8081/hub-api）")
	}

	// 占位符没渲染干净会留下 @@key@@，这种 README 比没有更糟。
	// 逐个列出认得的键，而不是笼统地找 "@@"——正文里出现 "@@" 完全可能（比如邮箱），
	// 那样的断言会误报，而误报几次之后就会被当成噪音删掉。
	for _, leftover := range []string{"@@name@@", "@@package@@", "@@sdk_module@@"} {
		if strings.Contains(readme, leftover) {
			t.Errorf("README 里残留了没渲染的占位符 %s", leftover)
		}
	}
}

func Test已存在的目录不覆盖(t *testing.T) {
	target := t.TempDir() // 目录已存在

	err := generate(target, scaffoldValues("x", "x", SDKModule))
	if err == nil {
		t.Fatal("目标目录已存在时应报错，而不是覆盖用户的东西")
	}
	if !strings.Contains(err.Error(), "已存在") {
		t.Errorf("错误信息应说清楚，实际 %v", err)
	}
}

// Test输出名与模板文件名一致 守住「文件名即输出名」这条约定。
//
// 中台侧渲染同一批模板时只会机械地把 `.tmpl` 去掉，它拿不到 scaffoldFiles 里的
// Output。两者一旦不一致，网页下载到的工程就与 CLI 生成的不是同一批文件——
// 真实发生过一次：`gitignore.tmpl` 配 Output `.gitignore`，网页那边发出去的包
// 里躺着一个叫 `gitignore` 的散落文件，真正的 `.gitignore` 丢了。
func Test输出名与模板文件名一致(t *testing.T) {
	for _, f := range scaffoldFiles {
		want := strings.TrimSuffix(strings.TrimPrefix(f.Template, "templates/"), ".tmpl")
		if f.Output != want {
			t.Errorf("模板 %s 的输出名写的是 %q，但按约定（去掉 .tmpl 后缀）应是 %q——"+
				"两者不一致时，中台打包出来的文件名会与 CLI 生成的差一个",
				f.Template, f.Output, want)
		}
	}
}

// Test生成的README的假绿判据与工具一致 守住「文档别落后于工具」。
//
// hubprobe e2e 判通过的条件是**三条**：状态码 200、响应是 JSON 对象、且里面的 plugin
// 是非空字符串。第三条是后加的——第一版只查到「是 JSON 对象」为止，于是中间层把一份
// 错误响应配成 200 转发就能骗过验收脚本。工具改了之后，这份随每个生成工程带走的
// README 没跟上，而「文档落后于工具」是没人会主动发现的：读文档的人不会去读工具源码。
func Test生成的README的假绿判据与工具一致(t *testing.T) {
	target := filepath.Join(t.TempDir(), "my-plugin")

	if err := generate(target, scaffoldValues("my-plugin", "example.com/my-plugin", SDKModule)); err != nil {
		t.Fatalf("生成失败: %v", err)
	}

	readme := readFile(t, filepath.Join(target, "README.md"))

	// 只锁语义要点，不锁 markdown 排版——排版改一次就红的话，这条测试会被
	// 当成噪音删掉，那就等于没写
	for _, want := range []string{
		"非空字符串", // 第三条判据：plugin 不能是空的
		"`plugin`", // 判据挂在哪个字段上
	} {
		if !strings.Contains(readme, want) {
			t.Errorf("生成的 README 里找不到 %q——它的假绿判据必须与 hubprobe e2e 的"+
				"实际判据（200 + JSON 对象 + plugin 非空）一致，否则读文档的人会漏掉"+
				"第三条，把中间层的 200 错误页当成验收通过", want)
		}
	}
}

// Test共通层素材里没有维护者注释 守住「发出去的东西里不该有内部话」。
//
// 共通层的正文会被**原样内联**进开发者拿到的 AGENTS.md。第一版我在这里写了一段
// 「改这里 = 改所有语言的模板」——那是写给维护模板的人看的，却跟着每个生成的工程
// 一起发了出去。这类东西不逐字读生成产物是发现不了的，所以要一条断言盯着。
func Test共通层素材里没有维护者注释(t *testing.T) {
	raw, err := templates.ReadFile(sharedOnboardingPath)
	if err != nil {
		t.Fatalf("读取共通层素材 %s 失败: %v", sharedOnboardingPath, err)
	}

	text := string(raw)
	if strings.Contains(text, "<!--") || strings.Contains(text, "-->") {
		t.Error("共通层素材里有 HTML 注释——它会被原样内联进开发者拿到的 AGENTS.md，" +
			"写给维护者看的话不该发出去。要留说明请写在 templates.go 里那个常量的注释上")
	}
}

func readFile(t *testing.T, path string) string {
	t.Helper()
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("读取 %s 失败: %v", path, err)
	}
	return string(raw)
}
