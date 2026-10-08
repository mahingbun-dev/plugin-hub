"""配置与信封助手。"""

from __future__ import annotations

import time

import pytest
from hubkit import (
    STRUCT_FQ_NAME,
    STRUCT_TYPE_URL,
    budget,
    config_from_env,
    deadline,
    expired,
    invalid,
    is_well_known_fq_name,
    issue,
    payload_json,
    valid,
    warning,
    with_payload_json,
)
from hubkit.config import (
    CALL_TIMEOUT,
    DEFAULT_LISTEN_ADDR,
    HEARTBEAT_FALLBACK_INTERVAL,
    REGISTER_RETRY_INTERVAL,
    Config,
)
from hubkit.proto.hubv1 import envelope_pb2, plugin_pb2


# ---------------------------------------------------------------- Config


def test_必填项缺失时报出变量名():
    with pytest.raises(ValueError) as err:
        Config(hub_addr="", advertise_addr="").validate()
    text = str(err.value)
    assert "HUB_ADDR" in text and "HUB_ADVERTISE_ADDR" in text


def test_只填了一个必填项时只报缺的那个():
    with pytest.raises(ValueError) as err:
        Config(hub_addr="http://x", advertise_addr="  ").validate()
    assert "HUB_ADVERTISE_ADDR" in str(err.value)
    assert "HUB_ADDR（" not in str(err.value)


def test_缺省值():
    cfg = Config(hub_addr="http://x", advertise_addr="http://y").with_defaults()
    assert cfg.listen_addr == DEFAULT_LISTEN_ADDR
    assert cfg.retry_interval == REGISTER_RETRY_INTERVAL
    assert cfg.call_timeout == CALL_TIMEOUT
    assert cfg.logger is not None
    # 实例标识缺省是「主机名-PID」：同一 ID 重复注册视为进程重启，所以必须每个进程不同
    assert cfg.instance_id.count("-") >= 1
    assert cfg.instance_id.rsplit("-", 1)[1].isdigit(), cfg.instance_id


def test_监听地址补齐成grpc认的写法():
    """``HUB_LISTEN_ADDR=:9000`` 是 Go 的写法，grpc-python 不认——要补主机名。

    不补的话报错是 ``add_insecure_port`` 返回 0，插件起不来，而看不出是地址格式问题。
    同一份部署清单要能套在两门语言上，所以这个转换必须在 SDK 里做。
    """
    cases = {
        ":9000": "[::]:9000",
        "9000": "[::]:9000",
        "0.0.0.0:9000": "[::]:9000",
        "127.0.0.1:9000": "127.0.0.1:9000",
        "[::]:9000": "[::]:9000",
    }
    for raw, expected in cases.items():
        cfg = Config(hub_addr="http://x", advertise_addr="http://y", listen_addr=raw)
        assert cfg.grpc_listen_target() == expected, raw


def test_中台地址去掉scheme():
    assert Config(hub_addr="http://127.0.0.1:8093").grpc_target() == "127.0.0.1:8093"
    assert Config(hub_addr="https://a.b:8094").grpc_target() == "a.b:8094"
    assert Config(hub_addr="127.0.0.1:8093").grpc_target() == "127.0.0.1:8093"
    assert Config(hub_addr="https://a.b:8094").use_tls()
    assert not Config(hub_addr="http://a.b:8093").use_tls()


def test_环境变量全读进来(monkeypatch):
    monkeypatch.setenv("HUB_ADDR", "http://hub:8093")
    monkeypatch.setenv("HUB_ADVERTISE_ADDR", "http://me:9000")
    monkeypatch.setenv("HUB_LISTEN_ADDR", ":9100")
    monkeypatch.setenv("HUB_INSTANCE_ID", "i-1")
    monkeypatch.setenv("HUB_LOG_LEVEL", "debug")

    cfg = config_from_env()
    assert cfg.hub_addr == "http://hub:8093"
    assert cfg.advertise_addr == "http://me:9000"
    assert cfg.listen_addr == ":9100"
    assert cfg.instance_id == "i-1"
    assert cfg.logger.level == "DEBUG"


# ---------------------------------------------------------------- 信封


def test_json载荷往返():
    env = with_payload_json(envelope_pb2.Envelope(message_id="m-1"), {"a": 1, "b": ["x", None, True]})
    assert env.payload.type_url == STRUCT_TYPE_URL
    assert payload_json(env) == ({"a": 1.0, "b": ["x", None, True]}, True)


def test_装载荷返回新信封不改原信封():
    """链路里可能有别的持有者，就地改会产生对上游可见的副作用。"""
    original = envelope_pb2.Envelope(message_id="m-1")
    out = with_payload_json(original, {"a": 1})

    assert not original.HasField("payload")
    assert out.message_id == "m-1"
    assert out.HasField("payload")


def test_载荷不是Struct时返回False():
    """业务类型的载荷必须能被识别出来，而不是被当成 JSON 硬解。"""
    env = envelope_pb2.Envelope()
    env.payload.Pack(plugin_pb2.PluginManifest(name="x"))

    assert payload_json(env) == (None, False)
    assert payload_json(envelope_pb2.Envelope()) == (None, False)
    assert payload_json(None) == (None, False)


def test_空JSON载荷返回True():
    """bool 回答的是「载荷是什么类型」，不是「载荷有没有内容」。

    这条区分是必须的：拿「载荷 == {}」当判据的话，一次真的没有输入的调用会被
    插件当成「载荷不是 JSON，去按业务类型解」，然后拿着一个业务消息去当 JSON 解。
    """
    env = with_payload_json(envelope_pb2.Envelope(), {})

    assert env.payload.type_url == STRUCT_TYPE_URL
    assert payload_json(env) == ({}, True)


def test_不是JSON对象时抛错():
    with pytest.raises(ValueError):
        with_payload_json(envelope_pb2.Envelope(), "不是对象")  # type: ignore[arg-type]


def test_数值只有float一种类型():
    """google.protobuf.Struct 没有整数——大单号要用字符串承载，这条要能被看见。"""
    env = with_payload_json(envelope_pb2.Envelope(), {"big": 9007199254740993})
    payload, is_json = payload_json(env)
    assert is_json
    assert isinstance(payload["big"], float)


def test_预算与截止时间():
    env = envelope_pb2.Envelope()
    assert deadline(env) is None
    assert budget(env) is None
    assert not expired(env)

    env.deadline_ms = int((time.time() + 30) * 1000)
    assert 28 < budget(env) <= 30
    assert not expired(env)

    env.deadline_ms = 1
    assert budget(env) == 0.0
    assert expired(env)


def test_校验响应的构造():
    assert valid().valid
    assert not valid().issues

    response = invalid(issue("payload.text", "缺少必填字段"))
    assert not response.valid
    assert response.issues[0].severity == plugin_pb2.SEVERITY_ERROR

    response = plugin_pb2.ValidateResponse(issues=[warning("payload.x", "能放行但记一笔")])
    assert response.issues[0].severity == plugin_pb2.SEVERITY_WARNING


def test_well_known判定():
    assert is_well_known_fq_name(STRUCT_FQ_NAME)
    assert is_well_known_fq_name("google.protobuf.Any")
    assert not is_well_known_fq_name("wms.v1.OrderCreated")
    assert not is_well_known_fq_name("")


def test_心跳兜底值比中台默认周期长():
    """兜底值是「中台没告诉我们」时的退化路径，它必须比中台的常规周期宽裕——
    否则一旦注册回执里没带周期，插件会打得比中台期望的更频繁。
    """
    assert HEARTBEAT_FALLBACK_INTERVAL >= 10
