"""core-py asyncFn 选取绑定契约 —— ``async_fn_refs`` / ``all_async_fn_keys``

背景：core 6.0.2 起 asyncFn 计算列按查询**显式请求的字段**执行
（``select_async_fns`` 新增 ``requested``）。绑定层 ``async_fn_refs(model, ctx, fields)``：
``fields`` 缺省时仍列出全部（与前版本行为一致，宿主无需改动），给了则与
``prepare_query`` 的 ``fnRefs`` 同口径；``all_async_fn_keys(model, ctx)`` 是
「列出全部键」的显式入口。

运行：python -m pytest core-py/test/async_fn_refs_bindings_test.py -q
"""

import pathlib
import sys

# 与其它绑定测试一致：优先 cargo 产物目录，未构建则回退已安装 wheel
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent / "dist"))

from rust_store_py import Registry  # noqa: E402


def registry():
    reg = Registry()
    reg.register(
        {
            "name": "Listing",
            "collection": "listings",
            "timestamps": False,
            "fields": {"_id": "string", "title": "string"},
            "relations": {},
            "computes": {
                "favoriteCount": {"type": "int", "asyncFn": True, "fnRef": "live_favorite"},
                "views": {"type": "int", "asyncFn": True, "fnRef": "live_views"},
            },
        }
    )
    return reg


def test_async_fn_refs_defaults_to_all_for_back_compat():
    reg = registry()
    assert sorted(reg.async_fn_refs("Listing")) == ["live_favorite", "live_views"]


def test_async_fn_refs_filters_by_requested_fields():
    reg = registry()
    assert reg.async_fn_refs("Listing", None, ["favoriteCount"]) == ["live_favorite"]
    assert reg.async_fn_refs("Listing", None, ["title"]) == [], (
        "请求非计算列字段时不应返回任何 fnRef"
    )


def test_all_async_fn_keys_ignores_requested_fields():
    reg = registry()
    assert sorted(reg.all_async_fn_keys("Listing")) == ["favoriteCount", "views"]
