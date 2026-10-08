"""让 ``scaffold.py`` 可被测试导入。

它在 SDK 根目录下（不是 ``hubkit`` 包的一部分——脚手架不该被装进插件运行时），
所以这里把它所在的那一层加进 ``sys.path``。
"""

from __future__ import annotations

import sys
from pathlib import Path

SDK_ROOT = Path(__file__).resolve().parents[1]
if str(SDK_ROOT) not in sys.path:
    sys.path.insert(0, str(SDK_ROOT))
