"""core-py 绑定 parity 对拍

回放 4 套 fixtures（pipeline / commands / computes / fnfns）与其 JS 黄金基准，
逐条深比较 PyO3 绑定层的输出——语义与 `core-node/test/parity.test.js` 完全一致：
  - pipeline   : `build_pipeline` 的 tokens / ast / pipeline / projection
  - commands   : `plan_query` / `plan_query_with_count` / `resolve_page` /
                 `restore_sort_order` / `plan_insert` / `plan_exists` / `plan_count` /
                 `plan_aggregate`
  - computes   : `process_node` / `inject_depends` + `strip_dep_injected` / `permission.*`
  - fnfns      : 同步 fn 走 `set_fn` 回调桥；asyncFn 由 Host 执行
                 （`async_fn_refs` 取标识，`prepare_query` + `strip_query` 两段式）

运行：python -m pytest core-py/test/parity_test.py
"""

import json
import pathlib
import sys

# 直接指向 cargo 产物目录（`mongo_store_py.pyd`），免装 wheel
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent / "dist"))

from mongo_store_py import Registry  # noqa: E402

FIXTURES = pathlib.Path(__file__).resolve().parent.parent.parent / "fixtures"

# ─── Host 侧回调表（对齐 tools/test-fns.js） ────────────────


def _sum_ab(doc):
    return (doc.get("a") or 0) + (doc.get("b") or 0)


def _mul_qp(doc):
    return (doc.get("qty") or 0) * (doc.get("price") or 0)


def _label(doc):
    return "big" if (doc.get("v") or 0) > 10 else "small"


def _null_fn(_doc):
    return None


def _greet(doc):
    return "hi:" + (doc.get("name") or "")


def _fill_area(items, _ctx):
    for it in items:
        it["area"] = (it.get("w") or 0) * (it.get("h") or 0)


def _ctx_tag(items, ctx):
    t = "u:" + str(ctx["userId"]) if ctx and ctx.get("userId") else "anon"
    for it in items:
        it["tag"] = t


def _tag_skus(items, _ctx):
    for it in items:
        it["skuTag"] = "|".join(str(s.get("sku")) for s in (it.get("items") or []))


FNS = {
    "sum_ab": _sum_ab,
    "mul_qp": _mul_qp,
    "label": _label,
    "null_fn": _null_fn,
    "greet": _greet,
}

ASYNC_FNS = {
    "fill_area": _fill_area,
    "ctx_tag": _ctx_tag,
    "tag_skus": _tag_skus,
}


def load(name):
    with open(FIXTURES / name, encoding="utf-8") as f:
        return json.load(f)


def make_registry(fx):
    """建绑定实例并注册 schema / 全部同步 fn 回调（异步回调由 Host 侧直接调用）"""
    reg = Registry()
    for s in fx.get("schemas") or []:
        reg.register(s)
    for ref, fn in FNS.items():
        reg.set_fn(ref, fn)
    return reg


def run_async_fns(refs, items, ctx):
    """依次执行 Host 侧 asyncFn 回调（对齐 core 的 `run_async_fns`）"""
    for ref in refs:
        fn = ASYNC_FNS.get(ref)
        if fn is None:
            raise RuntimeError(f"未注册的异步测试函数: {ref}")
        fn(items, ctx)


# ─── 深比较 ─────────────────────────────────────────────────
# 与 JS `isDeepStrictEqual` 对齐：bool 与 int 不可混同（Python 中 True == 1），
# 其余数字按值比较（JSON 无 int/float 之分）。


def deep_equal(a, b):
    if isinstance(a, bool) or isinstance(b, bool):
        return isinstance(a, bool) and isinstance(b, bool) and a == b
    if a is None or b is None:
        return a is None and b is None
    if isinstance(a, (int, float)) and isinstance(b, (int, float)):
        return a == b
    if isinstance(a, str) and isinstance(b, str):
        return a == b
    if isinstance(a, list) and isinstance(b, list):
        return len(a) == len(b) and all(deep_equal(x, y) for x, y in zip(a, b))
    if isinstance(a, dict) and isinstance(b, dict):
        if set(a.keys()) != set(b.keys()):
            return False
        return all(deep_equal(a[k], b[k]) for k in a)
    return False


def fmt(v):
    return json.dumps(v, ensure_ascii=False, default=str)


def expect_deep_equal(failures, label, actual, want):
    if not deep_equal(actual, want):
        failures.append(f"  {label}\n    want: {fmt(want)}\n    got : {fmt(actual)}")


def has(o, k):
    return k in o


# ─── pipeline ───────────────────────────────────────────────


def test_pipeline():
    cases = load("pipeline/cases.json")
    goldens = load("expected/cases.json")
    assert len(cases) == len(goldens), "输入与黄金基准用例数不一致"

    failures = []
    for fx, g in zip(cases, goldens):
        assert fx["name"] == g["name"], "用例顺序不一致"

        out = None
        caught = None
        try:
            out = make_registry(fx).build_pipeline(
                fx.get("gql") or "", fx.get("params") or {}, fx.get("context")
            )
        except Exception as e:  # noqa: BLE001
            caught = e

        if fx.get("expect_error"):
            if caught is None:
                failures.append(f"[{fx['name']}] 期望报错但成功返回")
            continue
        if caught is not None:
            failures.append(f"[{fx['name']}] 绑定报错: {caught}")
            continue

        lines = []
        for k in ("tokens", "ast", "pipeline", "projection"):
            expect_deep_equal(lines, k, out.get(k), g.get(k) if has(g, k) else None)
        if lines:
            failures.append(f"[{fx['name']}] 结果不一致\n" + "\n".join(lines))

    assert not failures, f"pipeline 对拍失败 {len(failures)} 项:\n" + "\n".join(failures)


# ─── commands ───────────────────────────────────────────────

import re  # noqa: E402

_STEP_PH = re.compile(r"^\{\{step\.(\d+)\._id\}\}$")


def substitute_step_placeholders(value, resolved):
    """把命令里 `{{step.<N>._id}}` 占位符替换为第 N 步模拟执行的 `_id`"""
    if isinstance(value, str):
        m = _STEP_PH.match(value)
        if m:
            idx = int(m.group(1))
            if idx < len(resolved):
                return resolved[idx]
        return value
    if isinstance(value, list):
        return [substitute_step_placeholders(v, resolved) for v in value]
    if isinstance(value, dict):
        return {k: substitute_step_placeholders(v, resolved) for k, v in value.items()}
    return value


def replay_write_case(reg, fx):
    """写路径统一回放：Host 模拟执行命令并回喂结果（对齐 parity_write.rs）"""
    ctx = fx.get("context")
    now = int(fx.get("now") or 0)
    updated = fx.get("updatedDoc")

    def returns_of():
        return None if updated is None else reg.apply_write_defaults(fx["model"], updated)

    kind = fx["kind"]
    if kind == "insert_many":
        return reg.plan_insert_many(
            fx.get("model") or "", fx.get("docs") or [], now, fx.get("newIds") or [], ctx
        )

    if kind == "update":
        out = reg.plan_update(
            fx.get("model") or "",
            fx.get("condition"),
            fx.get("data"),
            fx.get("options"),
            now,
            ctx,
        )
        if "needsProbe" in out:
            raise RuntimeError("unexpected needsProbe")
        return {"commands": [out["command"]], "returns": returns_of()}

    if kind == "update_many":
        out = reg.plan_update_many(
            fx.get("model") or "", fx.get("condition"), fx.get("data"), now, ctx
        )
        return {"command": out["command"], "returns": {"modifiedCount": fx.get("modifiedCount") or 0}}

    if kind == "remove":
        plan = reg.plan_remove(fx.get("model") or "", fx.get("condition"), ctx)
        docs = fx.get("docs") or []
        commands = []
        archived_count = 0
        if plan.get("findCommand") is not None:
            commands.append(plan["findCommand"])
            if docs:
                commands.append(reg.plan_archive_docs(fx["model"], docs, now)["command"])
                archived_count = len(docs)
        commands.append(plan["deleteCommand"])
        return {
            "commands": commands,
            "returns": {"deletedCount": fx.get("deletedCount") or 0, "archivedCount": archived_count},
        }

    if kind == "upsert":
        out = reg.plan_upsert(
            fx.get("model") or "",
            fx.get("condition"),
            fx.get("data"),
            fx.get("options"),
            now,
            (fx.get("newIds") or [""])[0] or "",
            ctx,
        )
        return {"command": out["command"], "returns": returns_of()}

    if kind == "mutation":
        plan = reg.plan_mutation(
            fx.get("model") or "", fx.get("data"), now, fx.get("newIds") or [], ctx
        )
        resolved = []
        commands = []
        root_result = None
        for step in plan["steps"]:
            command = substitute_step_placeholders(step["command"], resolved)
            if command.get("kind") == "insertOne":
                result = command.get("doc")
            elif command.get("kind") == "findOneAndUpdate":
                result = updated
            else:
                raise RuntimeError(f"未支持的 mutation 步骤命令: {command.get('kind')}")
            if root_result is None:
                root_result = result
            resolved.append(result.get("_id") if result else None)
            commands.append(command)
        returns = None if root_result is None else reg.apply_write_defaults(fx["model"], root_result)
        return {"commands": commands, "returns": returns}

    raise ValueError(f"未知用例类型: {kind}")


def run_command_case(reg, fx):
    """按 `kind` 分派，产出与 parity_commands.rs 完全相同的比较结构"""
    ctx = fx.get("context")

    kind = fx["kind"]
    if kind == "query":
        return reg.plan_query(fx.get("gql") or "", fx.get("params") or {}, ctx)

    if kind == "query_with_count":
        return reg.plan_query_with_count(
            fx.get("gql") or "", fx.get("params") or {}, ctx, float(fx.get("total") or 0)
        )

    if kind == "resolve_page":
        return reg.resolve_page(fx.get("gql") or "", fx.get("params") or {})

    if kind == "restore_sort_order":
        return reg.restore_sort_order(
            fx.get("items") or [], fx.get("ids") or [], fx.get("sort")
        )

    if kind == "insert":
        out = reg.plan_insert(
            fx.get("model") or "",
            fx.get("data"),
            int(fx.get("now") or 0),
            fx.get("newId") or "",
            ctx,
        )
        command = out.get("command")
        return {
            "command": command,
            "returns": out.get("returns"),
            "newId": command.get("doc", {}).get("_id") if command else None,
        }

    if kind == "exists":
        command = reg.plan_exists(fx.get("model") or "", fx.get("condition") or {})
        return {
            "command": command,
            "found": fx.get("foundDoc") is not None,
            "collections": [command.get("collection")],
        }

    if kind == "count":
        return {"command": reg.plan_count(fx.get("model") or "", fx.get("filter"))}

    if kind == "aggregate":
        return {
            "command": reg.plan_aggregate(fx.get("model") or "", fx.get("pipeline") or [])
        }

    # 写路径用例走 replay_write_case（对齐 parity_write.rs 的 Host 模拟执行）
    return replay_write_case(reg, fx)


def test_commands():
    cases = load("commands/cases.json")
    goldens = load("commands/expected.json")
    assert len(cases) == len(goldens), "输入与黄金基准用例数不一致"

    failures = []
    for fx, g in zip(cases, goldens):
        assert fx["name"] == g["name"], "用例顺序不一致"

        actual = None
        caught = None
        try:
            actual = run_command_case(make_registry(fx), fx)
        except Exception as e:  # noqa: BLE001
            caught = e

        if fx.get("expect_error"):
            if caught is None:
                failures.append(f"[{fx['name']}] 期望报错，但绑定未报错: {fmt(actual)}")
            continue
        if caught is not None:
            failures.append(f"[{fx['name']}] 绑定报错: {caught}")
            continue

        want = {"error": True} if g.get("error") is True else (g.get("result") if has(g, "result") else None)
        expect_deep_equal(failures, fx["name"], actual, want)

    assert not failures, f"commands 对拍失败 {len(failures)} 项:\n" + "\n".join(failures)


# ─── computes ───────────────────────────────────────────────


def run_computes_case(reg, fx):
    """按 `kind` 分派，产出与 parity_computes.rs 完全相同的比较结构"""
    ctx = fx.get("context")

    kind = fx["kind"]
    if kind == "process_node":
        # 绑定返回 `{doc}`；黄金基准是文档本身
        return reg.process_node(fx.get("gql") or "", fx.get("doc"), ctx)["doc"]

    if kind == "inject_depends":
        out = reg.inject_depends(fx.get("gql") or "")
        stripped = (
            reg.strip_dep_injected(out["injectInfo"], fx["items"])["items"]
            if isinstance(fx.get("items"), list)
            else None
        )
        return {
            "relDeps": out["relDeps"],
            "ast": out["ast"],
            "injectInfo": out["injectInfo"],
            "stripped": stripped,
        }

    if kind == "permission":
        model = fx["model"]
        return {
            "canRead": reg.can_read(model, ctx),
            "canWrite": reg.can_write(model, ctx),
            "shouldInjectOwner": reg.should_inject_owner(model, ctx),
            "condition": reg.merge_owner_condition(model, ctx, fx.get("condition")),
            "readableFields": reg.readable_fields(model, ctx),
            "readableRelations": reg.readable_relations(model, ctx),
            "writableFields": reg.writable_fields(model, ctx),
            "filteredData": reg.filter_writable_data(model, ctx, fx.get("data")),
        }

    raise ValueError(f"未知用例类型: {kind}")


def strip_meta(g):
    return {k: v for k, v in g.items() if k not in ("name", "kind")}


def test_computes():
    cases = load("computes/cases.json")
    goldens = load("computes/expected.json")
    assert len(cases) == len(goldens), "输入与黄金基准用例数不一致"

    failures = []
    for fx, g in zip(cases, goldens):
        assert fx["name"] == g["name"], "用例顺序不一致"

        try:
            actual = run_computes_case(make_registry(fx), fx)
        except Exception as e:  # noqa: BLE001
            failures.append(f"[{fx['name']}] 绑定报错: {e}")
            continue
        expect_deep_equal(failures, fx["name"], actual, strip_meta(g))

    assert not failures, f"computes 对拍失败 {len(failures)} 项:\n" + "\n".join(failures)


# ─── fnfns（fnRef 回调桥） ──────────────────────────────────


def run_fnfns_case(reg, fx):
    """按 `kind` 分派，产出与 parity_fnfns.rs 完全相同的比较结构"""
    ctx = fx.get("context")

    kind = fx["kind"]
    if kind == "process_node":
        return reg.process_node(fx.get("gql") or "", fx.get("doc"), ctx)

    if kind == "async_fns":
        items = fx.get("items") or []
        run_async_fns(reg.async_fn_refs(fx["model"], ctx), items, ctx)
        return {"items": items}

    if kind == "postprocess":
        # 对齐 crud.query：先注入 asyncFn 依赖，再取 postprocess 信息
        inj = reg.inject_depends(fx.get("gql") or "")
        pp = {"ast": inj["ast"], "inject": inj["injectInfo"]}

        prepared = reg.prepare_query(pp, fx.get("docs") or [], ctx)
        run_async_fns(prepared["fnRefs"], prepared["items"], ctx)
        return {"items": reg.strip_query(pp, prepared["items"])["items"]}

    if kind == "insert":
        out = reg.plan_insert(
            fx.get("model") or "",
            fx.get("data"),
            int(fx.get("now") or 0),
            fx.get("newId") or "",
            ctx,
        )
        command = out.get("command")
        return {
            "command": command,
            "returns": out.get("returns"),
            "newId": command.get("doc", {}).get("_id") if command else None,
        }

    raise ValueError(f"未知用例类型: {kind}")


def test_fnfns():
    cases = load("fnfns/cases.json")
    goldens = load("fnfns/expected.json")
    assert len(cases) == len(goldens), "输入与黄金基准用例数不一致"

    failures = []
    for fx, g in zip(cases, goldens):
        assert fx["name"] == g["name"], "用例顺序不一致"

        try:
            actual = run_fnfns_case(make_registry(fx), fx)
        except Exception as e:  # noqa: BLE001
            failures.append(f"[{fx['name']}] 绑定报错: {e}")
            continue
        want = {"error": True} if g.get("error") is True else (g.get("result") if has(g, "result") else None)
        expect_deep_equal(failures, fx["name"], actual, want)

    assert not failures, f"fnfns 对拍失败 {len(failures)} 项:\n" + "\n".join(failures)


# ─── 同步回调桥的缺失实现 → Python 异常 ─────────────────────


def test_missing_fn_raises():
    reg = Registry()
    reg.register(
        {
            "name": "Post",
            "collection": "posts",
            "timestamps": False,
            "fields": {"a": {"type": "int"}},
            "computes": {"total": {"type": "int", "fn": True, "fnRef": "missing_fn"}},
            "relations": {},
        }
    )
    try:
        reg.process_node("Post{total}", {"_id": "1", "a": 1}, None)
    except Exception as e:  # noqa: BLE001
        assert "missing_fn" in str(e), f"异常信息不含 missing_fn: {e}"
        return
    raise AssertionError("期望抛出异常但成功返回")
