// Command hub-plugin 生成一个可运行的插件工程骨架。
//
//	hub-plugin new <name> [--dir <path>] [--module <go module path>]
//
// 生成的插件**零 proto 工具链依赖**：它用 google.protobuf.Struct 承载 JSON 载荷，
// 也就是「直接调用」那条路（agent 经 MCP、外部系统经 HTTP）。需要类型化契约的插件
// 可以在这个基础上再加自己的 .proto。
package main

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

const (
	// SDKModule 是本 SDK 的 module 路径。生成的工程默认引用它。
	SDKModule = "github.com/mahingbun-dev/plugin-hub/sdk/go"

	exitOK    = 0
	exitUsage = 2
	exitFail  = 1
)

func main() {
	os.Exit(run(os.Args[1:]))
}

func run(args []string) int {
	if len(args) == 0 {
		usage()
		return exitUsage
	}

	switch args[0] {
	case "new":
		return runNew(args[1:])
	case "help", "-h", "--help":
		usage()
		return exitOK
	default:
		fmt.Fprintf(os.Stderr, "未知子命令 %q\n\n", args[0])
		usage()
		return exitUsage
	}
}

func usage() {
	fmt.Print(`hub-plugin —— plugin-hub 插件脚手架

用法:
  hub-plugin new <name> [选项]

选项:
  --dir <path>       生成到哪个目录（默认 ./<name>）
  --module <path>    生成工程的 go module 路径（默认 <name>）

生成的插件零 proto 工具链依赖，用 JSON 载荷承载业务数据；
执行 go run . 即可启动，它会连 HUB_ADDR 指定的中台并自注册。
`)
}

func runNew(args []string) int {
	opts, rest, err := parseNewArgs(args)
	if err != nil {
		fmt.Fprintf(os.Stderr, "参数错误: %v\n", err)
		usage()
		return exitUsage
	}

	if len(rest) == 0 {
		fmt.Fprintln(os.Stderr, "缺少插件名")
		return exitUsage
	}

	name := rest[0]
	if !isValidName(name) {
		fmt.Fprintf(os.Stderr,
			"插件名 %q 非法：只允许小写字母、数字与连字符，且以字母开头\n", name)
		return exitUsage
	}

	target := opts.dir
	if target == "" {
		target = "./" + name
	}
	mod := opts.module
	if mod == "" {
		mod = name
	}

	if err := generate(target, scaffoldValues(name, mod, SDKModule)); err != nil {
		fmt.Fprintf(os.Stderr, "生成失败: %v\n", err)
		return exitFail
	}

	fmt.Printf(`已生成插件工程 %s

下一步:

  cd %s
  go mod tidy
  go test ./...                      # 契约一致性 + 单元测试
  HUB_ADDR=http://127.0.0.1:8093 \
  HUB_ADVERTISE_ADDR=http://127.0.0.1:9000 \
  go run .

注意 HUB_ADVERTISE_ADDR 必须是**中台能拨通**的地址，不是本机视角的 localhost——
中台在注册时会连它做可达性探测，这是接入时最容易踩的坑。

Dockerfile 已一并生成，按「独立容器」的方式部署插件（见 docs/design.md）。
`, target, target)
	return exitOK
}

type newOptions struct {
	dir    string
	module string
}

// parseNewArgs 手工解析参数，允许选项出现在插件名之后。
//
// 不用标准库的 flag：它遇到第一个非选项参数就停止解析，
// 于是「hub-plugin new my-plugin --dir /tmp/x」里的 --dir 会被静默忽略——
// 这种「什么都没发生但也没报错」的行为最费人时间。
func parseNewArgs(args []string) (newOptions, []string, error) {
	var opts newOptions
	var rest []string

	for i := 0; i < len(args); i++ {
		arg := args[i]

		key, value, hasValue := strings.Cut(arg, "=")
		takeValue := func() (string, error) {
			if hasValue {
				return value, nil
			}
			i++
			if i >= len(args) {
				return "", fmt.Errorf("%s 缺少取值", key)
			}
			return args[i], nil
		}

		switch key {
		case "--dir", "-dir":
			v, err := takeValue()
			if err != nil {
				return opts, nil, err
			}
			opts.dir = v
		case "--module", "-module":
			v, err := takeValue()
			if err != nil {
				return opts, nil, err
			}
			opts.module = v
		default:
			if strings.HasPrefix(arg, "-") {
				return opts, nil, fmt.Errorf("未知选项 %s", arg)
			}
			rest = append(rest, arg)
		}
	}

	return opts, rest, nil
}

func isValidName(name string) bool {
	if name == "" || len(name) > 64 {
		return false
	}
	for i, r := range name {
		switch {
		case r >= 'a' && r <= 'z', r >= '0' && r <= '9':
		case r == '-' && i > 0 && i < len(name)-1:
		default:
			return false
		}
	}
	return true
}

// generate 把模板渲染进 target 目录。values 由 scaffoldValues 给出。
func generate(target string, values map[string]string) error {
	if _, err := os.Stat(target); err == nil {
		return fmt.Errorf("目录 %s 已存在", target)
	}
	if err := os.MkdirAll(target, 0o755); err != nil {
		return err
	}

	// 共通层素材由这里注入，**调用方不必知道它**：忘了传的后果是 AGENTS.md 里出现
	// 一个没有内容的章节，而那种缺陷要靠人逐字读生成出来的文件才会发现。
	shared, err := templates.ReadFile(sharedOnboardingPath)
	if err != nil {
		return fmt.Errorf("读取共通层素材 %s 失败: %w", sharedOnboardingPath, err)
	}
	// 复制一份再写，不改调用方传进来的 map
	values = mergeKey(values, placeholderOnboarding, string(shared))

	for _, file := range scaffoldFiles {
		raw, err := templates.ReadFile(file.Template)
		if err != nil {
			return fmt.Errorf("读取模板 %s 失败: %w", file.Template, err)
		}

		rendered, err := render(string(raw), values)
		if err != nil {
			// 报出是哪个模板——同一个占位符可能出现在多个文件里，
			// 不指名道姓就得逐个文件去找
			return fmt.Errorf("渲染模板 %s 失败: %w", file.Template, err)
		}

		path := filepath.Join(target, file.Output)
		if err := os.WriteFile(path, []byte(rendered), 0o644); err != nil {
			return err
		}
	}

	// 随包把 SDK 源码拷进去。生成的工程用 replace 指到它——不带的话，那个
	// replace 在别人机器上就是一个不存在的路径，工程下载下来也编不过。
	sdkRoot, err := locateSDKRoot()
	if err != nil {
		return err
	}
	if err := copySDK(sdkRoot, target); err != nil {
		return fmt.Errorf("拷贝 SDK 源码失败: %w", err)
	}

	return nil
}
