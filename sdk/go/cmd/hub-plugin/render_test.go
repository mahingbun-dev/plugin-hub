package main

import (
	"strings"
	"testing"
)

func Test已知占位符被替换(t *testing.T) {
	got, err := render(
		"module @@package@@\n\nrequire @@sdk_module@@\n\n// @@name@@",
		scaffoldValues("order-reader", "example.com/order-reader", "example.com/sdk"),
	)
	if err != nil {
		t.Fatalf("渲染失败: %v", err)
	}

	want := "module example.com/order-reader\n\nrequire example.com/sdk\n\n// order-reader"
	if got != want {
		t.Errorf("渲染结果不对\n得到:\n%s\n期望:\n%s", got, want)
	}
}

func Test同一个占位符出现多次都被替换(t *testing.T) {
	// README 里 @@name@@ 出现十几次，只换第一处是最容易写出的那种 bug
	got, err := render("@@name@@-@@name@@-@@name@@", scaffoldValues("p", "m", "s"))
	if err != nil {
		t.Fatalf("渲染失败: %v", err)
	}
	if got != "p-p-p" {
		t.Errorf("期望 p-p-p，得到 %q", got)
	}
}

func Test没有占位符时原样返回(t *testing.T) {
	const raw = "package main\n\nfunc main() {}\n"
	got, err := render(raw, scaffoldValues("p", "m", "s"))
	if err != nil {
		t.Fatalf("渲染失败: %v", err)
	}
	if got != raw {
		t.Errorf("没有占位符时不该改动文本\n得到:\n%s", got)
	}
}

func Test未定义的占位符报错而不是留空(t *testing.T) {
	// 留空会生成一个 manifest 里插件名为空的工程，那种工程要等到编译或注册时才炸。
	// 这一条是这套定界符的核心约定，不能退化成「替换不掉就原样留着」。
	_, err := render("name: @@plugin_name@@", scaffoldValues("p", "m", "s"))
	if err == nil {
		t.Fatal("未定义的占位符必须报错")
	}
	if !strings.Contains(err.Error(), "plugin_name") {
		t.Errorf("错误信息应点出是哪个占位符没定义，实际: %v", err)
	}
}

func Test没有收尾的占位符报错(t *testing.T) {
	_, err := render("name: @@name", scaffoldValues("p", "m", "s"))
	if err == nil {
		t.Fatal("没有收尾定界符时必须报错，而不是把剩下的一整段当正文输出")
	}
}

// Test模板里出现的占位符都有取值 是这套约定的兜底。
//
// 模板里写了一个新占位符、却忘了加进 knownPlaceholders——这件事在
// 「渲染时才发现」就太晚了：那要等到某个用户执行 hub-plugin new 才会暴露。
// 在这里扫一遍嵌入的模板，把发现提前到测试阶段。
func Test模板里出现的占位符都有取值(t *testing.T) {
	known := make(map[string]bool, len(knownPlaceholders))
	for _, k := range knownPlaceholders {
		known[k] = true
	}

	for _, file := range scaffoldFiles {
		raw, err := templates.ReadFile(file.Template)
		if err != nil {
			t.Fatalf("读取模板 %s 失败: %v", file.Template, err)
		}

		for _, key := range placeholderKeysIn(string(raw)) {
			if !known[key] {
				t.Errorf("模板 %s 里有未定义的占位符 %s%s%s——"+
					"要么把它加进 knownPlaceholders 并给出取值，要么是打错了字",
					file.Template, placeholderOpen, key, placeholderClose)
			}
		}
	}
}

// Test共通层正文里的占位符也会被替换 守住「渲染是两趟」。
//
// 单趟渲染时，@@onboarding@@ 展开出来的正文不会被重新扫描，里面自己的 @@name@@
// 会原样留在产物里——而那个占位符出现在 AGENTS.md 的正文中间，不逐字读根本发现不了。
func Test共通层正文里的占位符也会被替换(t *testing.T) {
	// 这里直接验渲染器本身：给一段内含占位符的「共通层正文」，看它有没有被二次展开
	values := scaffoldValues("order-reader", "example.com/order-reader", "example.com/sdk")
	values = mergeKey(values, placeholderOnboarding, "插件是 @@name@@，包名 @@package@@。")

	got, err := render("头部\n@@onboarding@@\n尾部", values)
	if err != nil {
		t.Fatalf("渲染失败: %v", err)
	}

	want := "头部\n插件是 order-reader，包名 example.com/order-reader。\n尾部"
	if got != want {
		t.Errorf("共通层正文里的占位符没被展开\n得到:\n%s\n期望:\n%s", got, want)
	}
}

// placeholderKeysIn 取出文本里所有 @@key@@ 的 key。
//
// 只用于测试：实现侧不这么做，因为实现要能在遇到未定义占位符时报错，
// 而「先全扫出来再替换」会把错误的定位信息丢掉。
func placeholderKeysIn(s string) []string {
	var keys []string
	for {
		start := strings.Index(s, placeholderOpen)
		if start < 0 {
			return keys
		}
		s = s[start+len(placeholderOpen):]

		end := strings.Index(s, placeholderClose)
		if end < 0 {
			return keys
		}
		keys = append(keys, s[:end])
		s = s[end+len(placeholderClose):]
	}
}
