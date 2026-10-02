"""core-py RBAC 绑定直测 —— set_rbac 注入 + 查询面 + 与 plan 链路联动

三端 parity 基准（core-node/test/rbac.bindings.test.js 与 core-ffi 同断言）：
  1. enforce 受管角色：granted 动作 rbac_can=True，未授予动作 False；
  2. enforce 受管角色 + 未配置 model → default deny（False）；
  3. rbac_readable_fields 只返回 readFields ∩ 静态可读（+_id 豁免）；
  4. plan_query 产命令在 viewer ctx 下投影被裁（判决在 plan 链路生效）；
  5. ownerOnly grant 的 query 命令 filter 含 createdBy = userId；
  6. set_rbac(None) 后全部恢复直通。

运行：python -m pytest core-py/test/rbac_bindings_test.py -q
（前置：maturin build --manifest-path core-py/Cargo.toml --out dist 产出 dist/rust_store_py.pyd）
"""

import pathlib
import sys

# 直接指向 cargo 产物目录（`rust_store_py.pyd`），免装 wheel（同 parity_test.py）
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent / "dist"))

from rust_store_py import Registry  # noqa: E402

_POLICY = {
    "mode": "enforce",
    "roles": {"viewer": {}, "editor": {}},
    "grants": [
        {"role": "viewer", "model": "Post", "actions": ["read"], "readFields": ["title"]},
        {"role": "editor", "model": "Post", "actions": ["read", "insert"]},
        {"role": "editor", "model": "Comment", "actions": ["read"], "ownerOnly": True},
    ],
}
_CTX_EDITOR = {"userId": "u1", "roles": ["editor"]}
_CTX_VIEWER = {"userId": "u2", "roles": ["viewer"]}


def _registry(policy=_POLICY):
    reg = Registry()
    for defn in [
        {
            "name": "Post",
            "collection": "posts",
            "timestamps": False,
            "fields": {"title": "string", "body": "string", "secret": "string"},
            "relations": {},
        },
        {
            "name": "Comment",
            "collection": "comments",
            "timestamps": False,
            "fields": {"body": "string"},
            "relations": {},
        },
        {
            "name": "User",
            "collection": "users",
            "timestamps": False,
            "fields": {"name": "string"},
            "relations": {},
        },
    ]:
        reg.register(defn)
    if policy is not None:
        reg.set_rbac(policy)
    return reg


def test_rbac_can_grant_and_deny():
    reg = _registry()
    assert reg.rbac_can("Post", "insert", _CTX_EDITOR) is True
    assert reg.rbac_can("Post", "read", _CTX_EDITOR) is True
    assert reg.rbac_can("Post", "remove", _CTX_EDITOR) is False


def test_rbac_can_default_deny_unconfigured_model():
    reg = _registry()
    # enforce 受管角色对未配置 model 一律拒绝
    assert reg.rbac_can("User", "read", _CTX_EDITOR) is False


def test_rbac_readable_fields_intersect():
    reg = _registry()
    fields = reg.rbac_readable_fields("Post", _CTX_VIEWER)
    # 与静态 readable_fields 同语义（遍历 schema.fields，不含 _id；投影侧 _id 豁免是
    # build_projection 的独立行为）
    assert fields is not None and sorted(fields) == ["title"], fields
    # editor 无 readFields 声明 → 字段维度不收紧（base 集合原样；None 仅在 ctx=None）
    assert sorted(reg.rbac_readable_fields("Post", _CTX_EDITOR)) == ["body", "secret", "title"]
    assert reg.rbac_readable_fields("Post", None) is None


def test_plan_query_projection_trimmed_by_rbac():
    reg = _registry()
    plan = reg.plan_query("Post{ title body secret }", {}, _CTX_VIEWER)
    s = str(plan)
    assert "secret" not in _projection_keys(plan), "viewer 不得投影 secret"
    assert "title" in _projection_keys(plan)
    assert s  # plan 非空（形态断言见 _projection_keys）


def _projection_keys(plan):
    """提取首命令投影键（find 的 projection 键 / aggregate 的 $project 阶段）"""
    cmds = plan.get("commands") or []
    if not cmds:
        return []
    cmd = cmds[0]
    proj = cmd.get("projection")
    if proj:
        return list(proj.keys())
    for stage in cmd.get("pipeline") or []:
        if "$project" in stage:
            return list(stage["$project"].keys())
    return []


def test_owner_only_query_injects_created_by():
    reg = _registry()
    plan = reg.plan_query("Comment{ body }", {}, _CTX_EDITOR)
    s = str(plan)
    assert "createdBy" in s, f"ownerOnly 读应注入 createdBy 条件: {s}"
    assert "u1" in s, f"行条件应绑定 userId: {s}"


def test_clear_policy_restores_passthrough():
    reg = _registry()
    assert reg.rbac_can("User", "read", _CTX_EDITOR) is False
    assert reg.rbac_enabled() is True
    reg.set_rbac(None)
    assert reg.rbac_enabled() is False
    assert reg.rbac_can("User", "read", _CTX_EDITOR) is True


def test_invalid_action_rejected():
    reg = _registry()
    try:
        reg.rbac_can("Post", "drop", _CTX_EDITOR)
        raise AssertionError("非法 action 应显式报错")
    except Exception as e:  # noqa: BLE001 —— PyO3 抛 RuntimeError/ValueError
        assert "drop" in str(e), f"错误信息应指名非法 action: {e}"
