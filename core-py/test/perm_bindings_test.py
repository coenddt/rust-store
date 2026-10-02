"""core-py 权限绑定直测 —— ``readable_computes`` 导出契约

read 白名单计算列的判决函数 core 侧既有（``permission::get_readable_computes``，
与 fields/relations 三函数完全同构），此前绑定面漏导出 → Host（py-store 的
``describe_for_ai``）对配 read 的计算列只能 fail-secure 收窄。本测试锁定绑定面
三态契约（与 ``readable_fields`` 同姿态）：
  - ``ctx=None`` → ``None``（不裁剪，fail-open）；
  - 计算列配 ``read`` 白名单 → 按角色过滤（未命中剔除）；
  - 计算列未配 ``read`` → 全集放行；
出参形态对齐 ``readable_fields``：sorted_set（排序去重数组）。

运行：python -m pytest core-py/test/perm_bindings_test.py -q
"""

import pathlib
import sys

# 直接指向 cargo 产物目录（`rust_store_py.pyd`），免装 wheel（同 parity_test.py）
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent / "dist"))

from rust_store_py import Registry  # noqa: E402

_CTX_ADMIN = {"userId": "u1", "roles": ["admin"]}
_CTX_USER = {"userId": "u2", "roles": ["user"]}


def _registry():
    reg = Registry()
    reg.register(
        {
            "name": "PermDoc",
            "collection": "perm_docs",
            "timestamps": False,
            "fields": {"title": "string"},
            "relations": {},
            "computes": {
                "adminOnly": {"type": "string", "read": ["admin"], "fnRef": "sumAb"},
                "staff": {"type": "string", "read": ["admin", "user"], "fnRef": "mulQp"},
                "open": {"type": "string", "fnRef": "sumAb"},
            },
        }
    )
    return reg


def test_readable_computes_none_ctx_not_trimmed():
    assert _registry().readable_computes("PermDoc", None) is None
    # 同姿态对照：readable_fields / readable_relations 的 ctx=None 均为 None
    assert _registry().readable_fields("PermDoc", None) is None
    assert _registry().readable_relations("PermDoc", None) is None


def test_readable_computes_read_whitelist_filters():
    # admin 命中全部白名单 + 未配放行 → 全集
    assert _registry().readable_computes("PermDoc", _CTX_ADMIN) == [
        "adminOnly",
        "open",
        "staff",
    ]
    # user 未命中 adminOnly → 白名单剔除（staff 配了 user 角色仍在）
    assert _registry().readable_computes("PermDoc", _CTX_USER) == ["open", "staff"]


def test_readable_computes_no_read_config_returns_all():
    reg = Registry()
    reg.register(
        {
            "name": "OpenDoc",
            "collection": "open_docs",
            "timestamps": False,
            "fields": {"title": "string"},
            "relations": {},
            "computes": {
                "b": {"type": "int", "fnRef": "sumAb"},
                "a": {"type": "int", "fnRef": "mulQp"},
            },
        }
    )
    # 未配 read 一律放行 → 全集；且 sorted_set 出参（排序，与 readable_fields 同形态）
    assert reg.readable_computes("OpenDoc", _CTX_USER) == ["a", "b"]
    # 无 computes 的 schema → 空 set（非 None）
    reg.register(
        {
            "name": "BareDoc",
            "collection": "bare_docs",
            "timestamps": False,
            "fields": {"title": "string"},
            "relations": {},
        }
    )
    assert reg.readable_computes("BareDoc", _CTX_USER) == []
