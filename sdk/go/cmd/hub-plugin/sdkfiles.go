package main

import (
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"runtime"
	"strings"
)

// sdkDirInProject 是随包下发的 SDK 在生成工程里的目录名。
//
// 它与 go.mod 里 replace 的目标、以及中台侧打包时用的目录名必须一致——
// 中台那边在 crates/hub-templates/build.rs 的 SDK_SOURCES 里，这边是这一行。
// **两处对不上时**冒烟脚本会红（它逐字节比对中台打包的与 CLI 生成的），
// 所以不必靠人记住。
const sdkDirInProject = "sdk"

// sdkExcluded 是不随包下发的条目。**必须与 build.rs 的 SDK_SOURCES 一致。**
//
// 写的是**相对 SDK 根的路径**，可以带斜杠（如 cmd/hub-plugin）。判断按路径前缀做，
// 不能只看第一段——只看第一段时 `cmd/hub-plugin` 会因为第一段是 `cmd` 而漏掉，
// 于是脚手架自己被拷进生成工程，而它带 `//go:embed all:templates`，编译直接失败。
//
//   - vendor：15MB，且它是 SDK 自己的 vendored 依赖。新工程是一个新 module，
//     用不上别人的 vendor——实测会被 Go 判为 inconsistent vendoring 直接拒绝。
//   - cmd/hub-plugin：脚手架自己，见上面的说明。
var sdkExcluded = []string{"vendor", "cmd/hub-plugin"}

// excluded 判断一个相对路径是否该跳过。
func excluded(rel string) bool {
	for _, skip := range sdkExcluded {
		if rel == skip || strings.HasPrefix(rel, skip+"/") {
			return true
		}
	}
	return false
}

// locateSDKRoot 找到 SDK 模块的根目录。
//
// 用 runtime.Caller 取本文件**编译时**的路径再往上走两级。这对
// `go run ./cmd/hub-plugin` 是准确的；对已经被移走的二进制，那个路径可能
// 不复存在——那时**明确报错**，而不是照着一个错路径去拷、生成出一个
// replace 指不到东西的工程（那种工程要等开发者 go build 时才发现）。
func locateSDKRoot() (string, error) {
	_, file, _, ok := runtime.Caller(0)
	if !ok {
		return "", fmt.Errorf("取不到本文件的编译期路径")
	}

	// 本文件在 <root>/cmd/hub-plugin/ 下
	root := filepath.Clean(filepath.Join(filepath.Dir(file), "..", ".."))

	// 认一个必须有、且不随包下发的标志物，确认真的是 SDK 根而不是别的目录
	if _, err := os.Stat(filepath.Join(root, "hubkit", "run.go")); err != nil {
		return "", fmt.Errorf(
			"按编译期路径推出的 SDK 根目录 %s 里没有 hubkit/run.go。\n"+
				"多半是这个二进制被移到了别处，而它还记着构建时的路径。\n"+
				"从 SDK 检出里用 `go run ./cmd/hub-plugin` 跑就不会有这个问题",
			root,
		)
	}
	return root, nil
}

// copySDK 把 SDK 源码拷进生成工程。
//
// 为什么要拷：生成的工程用 replace 指到 SDK，而那个路径在别人机器上不存在
// （内网没有私有 Go module 代理）。带上源码，replace 指向包内的相对路径，
// 工程拿到手就能编。
func copySDK(root, target string) error {
	// 先把包内 SDK 的根建出来。少了这一步，顶层文件（go.mod / README.md 这些）
	// 写进去时父目录还不存在——而 WalkDir 是先访问根、再访问子项的，
	// 根那一步 rel == "." 直接返回，不会走到建目录的分支。
	if err := os.MkdirAll(filepath.Join(target, sdkDirInProject), 0o755); err != nil {
		return err
	}

	return filepath.WalkDir(root, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}

		rel, err := filepath.Rel(root, path)
		if err != nil {
			return err
		}
		if rel == "." {
			return nil
		}

		// 按**路径前缀**判断，不是只看第一段：排除项里 `cmd/hub-plugin` 的第一段
		// 是 `cmd`，只看第一段就漏了。
		slashRel := filepath.ToSlash(rel)
		if excluded(slashRel) {
			if d.IsDir() {
				return fs.SkipDir
			}
			return nil
		}

		dest := filepath.Join(target, sdkDirInProject, rel)
		if d.IsDir() {
			return os.MkdirAll(dest, 0o755)
		}

		data, err := os.ReadFile(path)
		if err != nil {
			return err
		}
		return os.WriteFile(dest, data, 0o644)
	})
}
