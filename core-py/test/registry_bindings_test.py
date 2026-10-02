"""core-py Registry 注册表生命周期绑定直测 —— ``clear_schemas`` 导出契约

进程级 schema 注册表的**测试隔离 / 动态重建**原语：清空 ``schemas`` + 注册顺序，
不动 ``require_context`` / ``profile`` 配置开关（与 ``clear_fns``「各清各的」对称）。
py-store 测试套件经此实现跨文件隔离（tests/conftest.py 的模块级夹具），
修复「收集期模块级注册互相污染」（test_ask T1 guardrails / test_workflow KeyError）。

运行：python -m pytest core-py/test/registry_bindings_test.py -q
"""

import pathlib
import sys

# 直接指向 cargo 产物目录（`rust_store_py.pyd`），免装 wheel（同 parity_test.py）
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent / "dist"))

from rust_store_py import Registry  # noqa: E402


def test_clear_schemas_drops_schemas_keeps_switches():
    reg = Registry()
    reg.register(
        {
            "name": "C",
            "collection": "cs",
            "timestamps": False,
            "fields": {"a": "string"},
            "relations": {},
        }
    )
    assert reg.has("C")
    assert reg.list() == ["C", "CDeleted"], "注册含自动派生归档表"

    reg.set_profile("text2query")
    reg.set_require_context(True)

    reg.clear_schemas()
    assert not reg.has("C")
    assert reg.list() == []
    # 配置开关不随 schema 集合重建丢失
    assert reg.profile() == "text2query"
    assert reg.require_context() is True

    # 清空后可重新注册（重建语义）
    reg.register(
        {
            "name": "C",
            "collection": "cs",
            "timestamps": False,
            "fields": {"a": "string"},
            "relations": {},
        }
    )
    assert reg.has("C")
