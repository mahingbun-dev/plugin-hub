package main

import "embed"

// templates 里是脚手架生成的工程文件。
//
// 单独放文件而不是写成 Go 字符串常量：模板本身含反引号（生成的 Go 代码里要用原始
// 字符串写 JSON Schema），塞进 Go 的原始字符串字面量里会打架。
//
// **必须是 `all:templates` 而不是 `templates/*`**：后者按 go:embed 的规则会跳过
// 以 `.` 或 `_` 开头的条目，而模板里有 `.gitignore.tmpl`（以及日后放共通素材的
// `_shared/`）。漏掉它们不会报错，只会在生成出来的工程里少文件。
//
//go:embed all:templates
var templates embed.FS

// scaffoldFile 一份模板与它的输出文件名。
//
// **约定：模板文件名去掉 `.tmpl` 后缀就是输出名**，所以需要前导点的文件在模板侧
// 就写成 `.gitignore.tmpl`。这条约定是为了让「输出名」只存在于一个地方——文件名里。
// 早先靠 Output 字段表达（`gitignore.tmpl` → `.gitignore`），但中台侧渲染同一批
// 模板时只会机械地去后缀，于是网页下载到的包里躺着一个叫 `gitignore` 的散落文件，
// 真正的 `.gitignore` 丢了。文件名即输出名之后，两边不可能再对不上。
//
// Output 保留下来是因为它仍然是「产物该叫什么」的唯一权威表述，只是现在它必须
// 与 Template 去后缀的结果一致——有不一致由 Test输出名与模板文件名一致 兜住。
type scaffoldFile struct {
	Template string
	Output   string
}

var scaffoldFiles = []scaffoldFile{
	{Template: "templates/go.mod.tmpl", Output: "go.mod"},
	{Template: "templates/main.go.tmpl", Output: "main.go"},
	{Template: "templates/plugin.go.tmpl", Output: "plugin.go"},
	{Template: "templates/plugin_test.go.tmpl", Output: "plugin_test.go"},
	{Template: "templates/README.md.tmpl", Output: "README.md"},
	{Template: "templates/AGENTS.md.tmpl", Output: "AGENTS.md"},
	{Template: "templates/Dockerfile.tmpl", Output: "Dockerfile"},
	{Template: "templates/.gitignore.tmpl", Output: ".gitignore"},
}

// sharedOnboardingPath 是接入指南共通层的位置。
//
// 它会被内联进 AGENTS.md 的 @@onboarding@@ 处。**寄居在 Go 的模板目录下**是刻意的：
// Go 的 //go:embed 不能向上取父目录，放到 sdk/shared/ 就得为 Go 侧另加一套生成与
// 校验脚本；而中台侧（Rust）读任何路径都不受限制。所以位置迁就了 Go 这一侧。
//
// 它不在 scaffoldFiles 里——它是内联素材，不是要发给开发者的独立文件。
//
// ⚠️ **改这里 = 改所有语言的模板**。这是它存在的全部意义：同一条「接入四道关」
// 不该在六份模板里各抄一遍，抄了就会改一处漏五处。所以它里面只写与语言无关的东西，
// 具体的构建与运行命令由各语言模板的语言层自己写。
//
// 也因此它**不能带维护者注释**——正文会被原样内联进开发者拿到的 AGENTS.md，
// 任何写给「维护模板的人」看的话都会跟着发出去。
const sharedOnboardingPath = "templates/_shared/onboarding.md"
