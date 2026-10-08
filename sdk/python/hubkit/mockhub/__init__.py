"""本地 mock 中台：给插件开发者的「L3 替身」。

    from hubkit import mockhub

    with mockhub.Hub() as hub:
        ...  # 把插件的 HUB_ADDR 指向 hub.addr
"""

from .hub import (
    MOCK_STATE_TOKEN,
    GatewayCall,
    Hub,
    Options,
    PluginClient,
    Registration,
    dial_plugin,
    free_addr,
)

__all__ = [
    "MOCK_STATE_TOKEN",
    "GatewayCall",
    "Hub",
    "Options",
    "PluginClient",
    "Registration",
    "dial_plugin",
    "free_addr",
]
