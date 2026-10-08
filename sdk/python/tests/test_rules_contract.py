"""规则判定与跨语言契约文件的一致性。

``rules.py`` 的文档说 Python 侧「没有自动化的漂移检测」，理由是插件开发者手上不会
同时有中台仓库——那份 testdata 长在 ``sdk/go/hubkit/testdata/hub-rules.json``。
这里补的正是那条缺失的检测，但**找不到文件就 skip**：单拎一份 sdk/python 出来
（插件开发者的常态）跳过，在仓库里跑（SDK 维护者的常态）则逐条对齐契约文件。
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

import hubkit

CONTRACT = Path(__file__).resolve().parents[2] / "go" / "hubkit" / "testdata" / "hub-rules.json"

pytestmark = pytest.mark.skipif(
    not CONTRACT.exists(),
    reason=f"没有中台仓库里的契约文件（{CONTRACT}）——单独一份 SDK 里这是正常的",
)


@pytest.fixture(scope="module")
def contract() -> dict:
    return json.loads(CONTRACT.read_text(encoding="utf-8"))


def _check(rule: str, text: str) -> bool:
    if rule == "stateSegment":
        return hubkit.valid_state_segment(text)
    if rule == "pluginName":
        return hubkit.valid_plugin_name(text)
    raise AssertionError(f"契约文件里有本 SDK 还不认识的规则：{rule}")


def test_契约文件里的每条case都成立(contract):
    cases = contract["cases"]
    assert cases, "契约文件里不该没有用例"

    mismatched = [
        (case["rule"], case["input"], case["valid"], _check(case["rule"], case["input"]))
        for case in cases
        if _check(case["rule"], case["input"]) != case["valid"]
    ]
    assert not mismatched, f"与契约文件不一致（规则, 输入, 期望, 实际）：{mismatched}"


def test_状态凭证的metadata键名与契约一致(contract):
    """这个键写错的表现是「中台判成无凭证」，而插件侧只看到一个 401——很难查。"""
    assert hubkit.STATE_TOKEN_METADATA == contract["stateTokenMetadata"]


def test_键名长度上限与契约一致(contract):
    limit = contract["rules"]["stateSegment"]["maxBytes"]
    assert hubkit.valid_state_segment("a" * limit)
    assert not hubkit.valid_state_segment("a" * (limit + 1))


def test_插件名长度上限与契约一致(contract):
    limit = contract["rules"]["pluginName"]["maxBytes"]
    assert hubkit.valid_plugin_name("a" * limit)
    assert not hubkit.valid_plugin_name("a" * (limit + 1))
