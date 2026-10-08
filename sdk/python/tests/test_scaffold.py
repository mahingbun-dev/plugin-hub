"""脚手架的测试。

重点不是「生成成功」，而是**生成失败时会失败**：留空占位符、漏拷 SDK、模板文件名与
输出名对不上——这几种缺陷的产物看上去都是一个「正常的工程」，要等编译或注册时才发现。
"""

from __future__ import annotations

import re

import pytest
import scaffold

# 生成工程里应该有的文件。**刻意逐条列出来**而不是断言「至少有这些」：
# 模板增删文件时这条清单会红，逼人确认「这个文件该不该出现在开发者手里」。
EXPECTED_FILES = {
    # `.dockerignore` 该出现在开发者手里：Docker 只认它、不认 .gitignore，而这个
    # 工程的 README 让人先在本地建 `.venv/` 跑一遍——没有它，构建上下文里会白传
    # 几百 MB（Dockerfile 的注释写了完整理由）。
    ".dockerignore",
    ".gitignore",
    "AGENTS.md",
    "Dockerfile",
    "README.md",
    "main.py",
    "plugin.py",
    "requirements.txt",
    "test_plugin.py",
}

PLACEHOLDER_RE = re.compile(r"@@([A-Za-z_][A-Za-z0-9_]*)@@")


def _generate(tmp_path, name="order-reader", **kwargs):
    target = tmp_path / name
    scaffold.generate(target, scaffold.scaffold_values(name, name, "http://127.0.0.1:8093"), **kwargs)
    return target


def test_生成的文件清单与输出名(tmp_path):
    target = _generate(tmp_path, with_sdk=False)
    assert {p.name for p in target.iterdir()} == EXPECTED_FILES

    # 文件名即输出名：需要前导点的文件在模板侧就写成 .gitignore.tmpl
    assert (target / ".gitignore").exists()
    assert not list(target.glob("*.tmpl")), "产物里不该留下 .tmpl 后缀"


def test_占位符全部被替换(tmp_path):
    """生成物里不该残留任何 ``@@key@@``。

    残留一种的表现是 manifest 里插件名空着、或 import 路径残缺——那种工程要等到
    编译或注册时才炸，而那时已经离「生成」很远了，人不会想到回头怀疑模板。
    """
    target = _generate(tmp_path, with_sdk=False)

    for path in target.iterdir():
        leftovers = PLACEHOLDER_RE.findall(path.read_text(encoding="utf-8"))
        assert not leftovers, f"{path.name} 里残留了占位符 {leftovers}"


def test_未定义的占位符直接报错():
    """留空会生成一个残缺但看起来正常的工程，所以必须报错。"""
    with pytest.raises(ValueError) as err:
        scaffold.render("hello @@nope@@", {"name": "x"})
    assert "未定义的占位符" in str(err.value)
    # 报错要给出「认得的只有哪些」，否则使用者只能去翻源码
    assert "sdk_module" in str(err.value)


def test_模板里用到的占位符都是认得的():
    """模板里出现新占位符而实现里没登记时，在这里就红——而不是等渲染时才报。

    这条测试与 ``KNOWN_PLACEHOLDERS`` **共用同一份清单**：分开写两份的话会出现
    「测试认为合法、实现认为非法」这种自相矛盾，而且没人会发现。
    """
    root = scaffold._template_root() / "plugin"
    for template in sorted(root.iterdir()):
        used = set(PLACEHOLDER_RE.findall(template.read_text(encoding="utf-8")))
        unknown = used - set(scaffold.KNOWN_PLACEHOLDERS)
        assert not unknown, f"{template.name} 用了未登记的占位符 {unknown}"


def test_模板文件名去后缀就是输出名():
    """Go 侧也是这条约定，而中台侧（Rust）渲染同一批模板时只会机械地去后缀。

    这条约定让「输出叫什么」只存在于一个地方——文件名里。分开写两份的话，
    网页上下载到的包里就会躺着一个叫 `gitignore` 的散落文件，真正的 `.gitignore` 丢了。
    """
    root = scaffold._template_root() / "plugin"
    for template in sorted(root.iterdir()):
        output = template.name[: -len(".tmpl")] if template.name.endswith(".tmpl") else template.name
        assert output, f"{template.name} 去后缀之后没有名字了"


def test_共通层被内联进AGENTS(tmp_path):
    """``@@onboarding@@`` 是共通层正文的挂载点——忘了内联的后果是 AGENTS.md 里
    出现一个没有内容的章节，而那种缺陷要靠人逐字读生成出来的文件才会发现。
    """
    target = _generate(tmp_path, with_sdk=False)
    agents = (target / "AGENTS.md").read_text(encoding="utf-8")

    # 共通层里的小标题（改共通层正文时这里要跟着改）
    assert "什么叫「接入成功」" in agents
    assert "L3 中台接受注册" in agents


def test_插件名出现在生成物的关键位置(tmp_path):
    target = _generate(tmp_path, name="my-plugin", with_sdk=False)

    assert 'name="my-plugin"' in (target / "plugin.py").read_text(encoding="utf-8")
    assert "my-plugin 收到" in (target / "plugin.py").read_text(encoding="utf-8")


def test_中台地址占位符出现在README(tmp_path):
    target = _generate(tmp_path, with_sdk=False)
    readme = (target / "README.md").read_text(encoding="utf-8")
    assert "HUB_ADDR=http://127.0.0.1:8093" in readme
    assert "@@hub_addr@@" not in readme


@pytest.mark.parametrize("name", ["order-reader", "a", "abc123", "a-b-c"])
def test_合法插件名(name):
    assert scaffold.is_valid_name(name)


@pytest.mark.parametrize("name", ["", "-abc", "abc-", "Order-Reader", "ord_er", "中文", "a" * 65])
def test_非法插件名(name):
    assert not scaffold.is_valid_name(name)


def test_目标目录已存在时报错(tmp_path):
    target = tmp_path / "taken"
    target.mkdir()
    with pytest.raises(FileExistsError):
        scaffold.generate(target, scaffold.scaffold_values("taken", "taken", "http://x"))


def test_随包SDK排除了脚手架自己与本地产物(tmp_path):
    """SDK 拷进生成工程时要排掉几样东西，每一样漏掉都有具体的坏结果。"""
    target = _generate(tmp_path, with_sdk=True)

    sdk = target / "sdk"
    assert (sdk / "hubkit" / "run.py").exists()
    assert (sdk / "pyproject.toml").exists(), "少了它 -e ./sdk 就装不上"

    # templates/ 不发出去 -> 带上 scaffold.py 会得到一个「点开就报找不到模板」的脚本
    assert not (sdk / "scaffold.py").exists(), "脚手架自己不该随包下发"
    assert not (sdk / "templates").exists()
    # tests/ 里的 echo_plugin 不在包里，带过去只会多一个跑不过的测试目录
    assert not (sdk / "tests").exists()
    # pip install -e . 会在 SDK 根目录里建出它，里面记的是**打包者机器上的路径**
    assert not list(sdk.glob("*.egg-info")), "*.egg-info 必须排除"
    assert not list(sdk.rglob("__pycache__"))
