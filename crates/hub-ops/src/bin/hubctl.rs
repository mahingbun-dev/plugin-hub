//! `hubctl`：主机面运维通道的命令行入口。
//!
//! 用法（在中台容器内或挂载了 socket 的宿主机上）：
//!
//! ```text
//! hubctl status
//! hubctl list-plugins
//! hubctl list-instances
//! hubctl remove-instance <instance_id>
//! hubctl remove-version <plugin> <version> [--yes]
//! hubctl delete-plugin <name> [--yes]
//! ```
//!
//! socket 路径取 `HUB_OPS_SOCKET`，默认 `/run/anc-hub/ops.sock`。

use std::path::PathBuf;
use std::process::ExitCode;

use hub_ops::{OpsRequest, OpsResponse};

const DEFAULT_SOCKET: &str = "/run/anc-hub/ops.sock";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let request = match parse(&args) {
        Ok(Some(request)) => request,
        // --help
        Ok(None) => {
            print_usage();
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("参数错误: {message}");
            print_usage();
            return ExitCode::from(2);
        }
    };

    let socket = socket_path();
    match hub_ops::request(&socket, &request).await {
        Ok(response) => print_response(&response),
        Err(err) => {
            eprintln!("{err}");
            eprintln!();
            eprintln!("排查提示：确认中台在跑、socket 路径正确（当前 {socket:?}），");
            eprintln!("以及当前用户对 socket 所在目录有访问权限（该通道只对属主开放）。");
            ExitCode::FAILURE
        }
    }
}

fn socket_path() -> PathBuf {
    std::env::var("HUB_OPS_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_SOCKET))
}

/// 解析参数。`Ok(None)` 表示只是要看帮助。
fn parse(args: &[String]) -> Result<Option<OpsRequest>, String> {
    let confirm = args.iter().any(|a| a == "--yes" || a == "-y");
    let positional: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with('-'))
        .collect();

    let Some(command) = positional.first().copied() else {
        return Ok(None);
    };

    let request = match command {
        "help" | "--help" | "-h" => return Ok(None),
        "status" => OpsRequest::Status,
        "list-plugins" => OpsRequest::ListPlugins,
        "list-instances" => OpsRequest::ListInstances,
        "remove-instance" => OpsRequest::RemoveInstance {
            instance_id: positional
                .get(1)
                .ok_or("remove-instance 需要 instance_id")?
                .to_string(),
        },
        "remove-version" => OpsRequest::RemoveVersion {
            plugin: positional
                .get(1)
                .ok_or("remove-version 需要插件名")?
                .to_string(),
            version: positional
                .get(2)
                .ok_or("remove-version 需要版本号")?
                .to_string(),
        },
        "delete-plugin" => OpsRequest::DeletePlugin {
            name: positional
                .get(1)
                .ok_or("delete-plugin 需要插件名")?
                .to_string(),
        },
        other => return Err(format!("未知指令 {other}")),
    };

    // 破坏性操作必须显式确认：这条通道没有撤销
    if request.is_destructive() && !confirm {
        return Err(format!(
            "{} 是不可逆操作，确认请加 --yes",
            request.command_name()
        ));
    }

    Ok(Some(request))
}

fn print_usage() {
    println!(
        "\
hubctl —— anc-hub 主机面运维通道

用法:
  hubctl status                              中台自检（数据库、插件/版本/实例计数、配置摘要）
  hubctl list-plugins                        插件目录
  hubctl list-instances                      在线实例
  hubctl remove-instance <instance_id>       摘掉卡住的实例
  hubctl remove-version <plugin> <version> --yes    删除一个插件版本（不可逆）
  hubctl delete-plugin <name> --yes                 删除整个插件（不可逆）

环境变量:
  HUB_OPS_SOCKET   运维 socket 路径，默认 {DEFAULT_SOCKET}

这条通道是管理面全插件化架构下的唯一逃生口：当 auth 插件自己坏了、HTTP 面进不去时，
靠它把中台救回来。信任边界是文件系统权限，不是鉴权。"
    );
}

fn print_response(response: &OpsResponse) -> ExitCode {
    if response.ok {
        match &response.data {
            Some(data) => println!(
                "{}",
                serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string())
            ),
            None => println!("ok"),
        }
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "失败: {}",
            response.error.as_deref().unwrap_or("(无错误信息)")
        );
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn 无参数时打印帮助() {
        assert!(parse(&[]).expect("应成功").is_none());
    }

    #[test]
    fn 只读指令无需确认() {
        assert_eq!(
            parse(&args(&["status"])).expect("应成功"),
            Some(OpsRequest::Status)
        );
        assert_eq!(
            parse(&args(&["list-plugins"])).expect("应成功"),
            Some(OpsRequest::ListPlugins)
        );
    }

    #[test]
    fn 破坏性指令缺_yes_时被拒() {
        let err = parse(&args(&["delete-plugin", "auth"])).expect_err("应被拒");
        assert!(err.contains("--yes"), "应提示如何确认，实际 {err}");
        assert!(err.contains("不可逆"));
    }

    #[test]
    fn 破坏性指令带_yes_时放行() {
        assert_eq!(
            parse(&args(&["delete-plugin", "auth", "--yes"])).expect("应成功"),
            Some(OpsRequest::DeletePlugin {
                name: "auth".to_string()
            })
        );
    }

    #[test]
    fn 参数缺失时报错而不是发出空指令() {
        assert!(parse(&args(&["remove-version", "auth"])).is_err());
        assert!(parse(&args(&["remove-instance"])).is_err());
    }

    #[test]
    fn 未知指令被拒() {
        let err = parse(&args(&["drop-everything"])).expect_err("应被拒");
        assert!(err.contains("未知指令"));
    }

    #[test]
    fn 参数顺序无关() {
        let a = parse(&args(&["remove-version", "auth", "1.0.0", "--yes"])).expect("应成功");
        let b = parse(&args(&["--yes", "remove-version", "auth", "1.0.0"])).expect("应成功");
        assert_eq!(a, b);
    }
}
