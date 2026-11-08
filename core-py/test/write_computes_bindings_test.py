"""core-py ``apply_write_computes`` 导出契约

写路径（``find_one_and_update`` + ``return_document: AFTER``）返回的落库文档需要
叠加 ``computes`` 中的同步 ``fn`` 计算列；该导出**只叠计算列、不补默认值**
（D1：默认值只在 insert 物化一次，重复填充会让「落库 / returns / 读回」出现不同取值）。

断言覆盖：
  1. 无同步 ``fn`` 声明 ⇒ 原样返回，且不补默认值；
  2. 声明 ``fn: true`` 但未 ``set_fn`` 注册实现 ⇒ 显式报错（不静默跳过）；
  3. 落库文档能叠出计算列，且返回值的计算列 == 以同一落库文档读回后的计算列；
  4. 入参文档不被改写（返回值是副本）。

运行：python -m pytest core-py/test/write_computes_bindings_test.py -q
"""

import pathlib
import sys

import pytest

# 直接指向 cargo 产物目录（`rust_store_py.pyd`），免装 wheel（同 parity_test.py）
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent / "dist"))

from rust_store_py import Registry  # noqa: E402

PLAIN_SCHEMA = {
    "name": "Plain",
    "collection": "plains",
    "timestamps": False,
    "fields": {
        "_id": {"type": "string"},
        "a": {"type": "string"},
        "status": {"type": "string", "default": "new"},
    },
    "relations": {},
}

ORDER_SCHEMA = {
    "name": "Order",
    "collection": "orders",
    "timestamps": False,
    "fields": {
        "_id": {"type": "string"},
        "a": {"type": "string"},
        "b": {"type": "string"},
        "status": {"type": "string", "default": "new"},
    },
    "relations": {},
    "computes": {
        "total": {"type": "string", "fn": True, "fnRef": "total"},
    },
}


def test_no_fn_returns_doc_unchanged_without_defaults():
    reg = Registry()
    reg.register(PLAIN_SCHEMA)

    doc = {"_id": "p1", "a": "v"}
    out = reg.apply_write_computes("Plain", doc)

    assert out == doc
    assert "status" not in out, "不得补默认值（默认值只在 insert 物化一次）"
    assert doc == {"_id": "p1", "a": "v"}, "入参不得被改写"


def test_fn_declared_but_unregistered_raises():
    reg = Registry()
    reg.register(ORDER_SCHEMA)

    with pytest.raises(RuntimeError, match="未注册同步实现"):
        reg.apply_write_computes("Order", {"_id": "o1", "a": "x", "b": "y"})


def test_computed_column_applied_and_matches_read_back():
    reg = Registry()
    reg.register(ORDER_SCHEMA)
    reg.set_fn("total", lambda doc: f"{doc['a']}-{doc['b']}")

    # update 落库后的文档：只有被更新的字段，没有计算列、没有未参与更新的默认值
    stored = {"_id": "o1", "a": "x", "b": "y"}
    returns = reg.apply_write_computes("Order", stored)

    assert returns["total"] == "x-y", "返回值必须带上计算列"
    assert stored == {"_id": "o1", "a": "x", "b": "y"}, "计算列不得回写落库文档"

    # 读回路径同样以落库文档为基计算 ⇒ returns 与读回的计算列必须一致（D1 三值归一）
    read_back = reg.apply_write_computes("Order", dict(stored))
    assert returns["total"] == read_back["total"], "returns 与读回值不得出现两个计算值"

    assert "status" not in returns, "不得把 insert 默认值补进写路径返回值"
