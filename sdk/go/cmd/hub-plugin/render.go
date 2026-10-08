package main

import (
	"fmt"
	"strings"
)

// 模板占位符的定界符。
//
// 不用 Go 的 `{{.Key}}`：同一份模板中台侧（Rust）也要渲染，而 `{{`/`}}` 在 Node
// 模板与 C# 插值字符串里都可能撞；`@@key@@` 与任何语言的语法都不冲突，两侧各
// 三十行就能实现同一套规则。改了这里，中台的 crates/hub-templates 要跟着改。
const (
	placeholderOpen  = "@@"
	placeholderClose = "@@"
)

// 认得的占位符。集中列出来，模板里写错时这条清单就是唯一的对照表。
const (
	// placeholderName 是插件名，同时是 manifest.name、目录名与 MCP 工具前缀的来源。
	placeholderName = "name"
	// placeholderPackage 是包名 / module 路径。
	placeholderPackage = "package"
	// placeholderSDKModule 是 SDK 自身的 module 路径，生成的工程引用它。
	placeholderSDKModule = "sdk_module"
	// placeholderSDKPath 是包内 SDK 源码的目录，go.mod 的 replace 指向它。
	//
	// 与 placeholderSDKModule 分开是必须的：module 路径要进 import 语句，
	// 那里不能用相对路径；而 replace 的目标必须是相对的，否则在别人机器上
	// 就又是一个不存在的绝对路径。
	placeholderSDKPath = "sdk_path"
	// placeholderOnboarding 是共通层正文的挂载点，只有 AGENTS.md 里有。
	placeholderOnboarding = "onboarding"
	// placeholderHubAddr 是中台插件面地址。
	//
	// **两处填的值不同，这是刻意的**：中台渲染下载包时填**本中台**的真实地址
	// （没配就是一段可见的占位文字），而 `hub-plugin new` 是在人自己的机器上起工程，
	// 那时要连的就是本地的 mock 中台。同一个占位符表达的始终是「这份工程该连哪台
	// 中台」，两处的答案本来就不同。
	placeholderHubAddr = "hub_addr"
)

// localHubAddr 是 `hub-plugin new` 填给 @@hub_addr@@ 的值。
//
// 与中台侧渲染时填的不同——见 placeholderHubAddr 的说明。这里的 8093 与
// README 里那张「端口速查表」指的是同一件事。
const localHubAddr = "http://127.0.0.1:8093"

// knownPlaceholders 是全部认得的关键字。
//
// 实现与兜底测试**共用这一份**：分开写两份清单的话，会出现「测试认为合法、
// 实现认为非法」这种自相矛盾，而且没人会发现。
//
// 注意它不是 scaffoldValues 的键集合——`onboarding` 由 generate 自己注入
// （见那里的说明），不属于「调用方提供的取值」。
var knownPlaceholders = []string{
	placeholderName,
	placeholderPackage,
	placeholderSDKModule,
	placeholderSDKPath,
	placeholderHubAddr,
	placeholderOnboarding,
}

// scaffoldValues 给出渲染脚手架所需的全部占位符取值。
//
// 中台侧渲染同一份模板时用的是同一组键（见 crates/hub-templates）。两边对不上的
// 话，中台那边会以「未定义的占位符」报错，而不是悄悄生成一份残缺的工程——
// 这正是这套定界符要换来的一半价值。
func scaffoldValues(name, module, sdkModule string) map[string]string {
	return map[string]string{
		placeholderName:      name,
		placeholderPackage:   module,
		placeholderSDKModule: sdkModule,
		placeholderSDKPath:   "./" + sdkDirInProject,
		placeholderHubAddr:   localHubAddr,
	}
}

// render 把模板里的 @@key@@ 替换成 values[key]。
//
// **未定义的占位符直接报错、不留空**：留空会生成一个 manifest 里插件名为空、
// 或 import 路径残缺的工程，那种工程要等到编译或注册时才炸——而那时已经离
// 「生成」很远了，人不会想到回头怀疑模板。
//
// 分两趟：第一趟把 @@onboarding@@ 展开成共通层正文，第二趟替换**正文里自己的**
// 占位符。单趟做不到——扫描是自左向右的，刚插入的文本不会被重新扫一遍，
// 于是共通层里的 @@name@@ 会原样留在产物里。
func render(raw string, values map[string]string) (string, error) {
	once, err := renderOnce(raw, values)
	if err != nil {
		return "", err
	}
	if !strings.Contains(once, placeholderOpen) {
		return once, nil
	}
	return renderOnce(once, values)
}

func renderOnce(raw string, values map[string]string) (string, error) {
	var b strings.Builder
	b.Grow(len(raw))

	for rest := raw; ; {
		start := strings.Index(rest, placeholderOpen)
		if start < 0 {
			b.WriteString(rest)
			return b.String(), nil
		}
		b.WriteString(rest[:start])
		rest = rest[start+len(placeholderOpen):]

		end := strings.Index(rest, placeholderClose)
		if end < 0 {
			return "", fmt.Errorf(
				"模板里有一个没有收尾的 %s：%q",
				placeholderOpen, head(rest, 40),
			)
		}
		key := rest[:end]
		rest = rest[end+len(placeholderClose):]

		value, ok := values[key]
		if !ok {
			return "", fmt.Errorf(
				"模板里有未定义的占位符 %s%s%s（认得的只有 %s）",
				placeholderOpen, key, placeholderClose,
				strings.Join(knownPlaceholders, " / "),
			)
		}
		b.WriteString(value)
	}
}

// mergeKey 返回一份 values 的副本，并在其中加上一个键。
//
// 复制而不是直接改：调用方传进来的 map 可能是共享的，就地改会让「渲染」这种
// 只读动作产生副作用。
func mergeKey(values map[string]string, key, value string) map[string]string {
	merged := make(map[string]string, len(values)+1)
	for k, v := range values {
		merged[k] = v
	}
	merged[key] = value
	return merged
}

// head 截断一段文本用于报错，避免把整个模板正文甩进错误信息里。
func head(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "…"
}
