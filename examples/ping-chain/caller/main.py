"""ping-caller 的入口。

启动后向 ``HUB_ADDR`` 指定的中台自注册并按周期发心跳；收到 SIGINT / SIGTERM
时注销并退出。配置全部来自环境变量，见本目录 README。
"""

from __future__ import annotations

import hubkit

import plugin


def main() -> None:
    hubkit.run(plugin.PingCaller(), hubkit.config_from_env())


if __name__ == "__main__":
    main()
