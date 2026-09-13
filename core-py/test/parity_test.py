"""core-py 绑定 parity 对拍

回放 5 套 fixtures（pipeline / commands / computes / fnfns / federation）
与其黄金基准，逐条深比较 PyO3 绑定层的输出——语义与 `core-node/test/parity.test.js` 完全一致：
  - pipeline   : `build_pipeline` 的 tokens / ast / pipeline / projection
  - commands   : `plan_query` / `plan_query_with_count` / `resolve_page` /
                 `restore_sort_order` / `plan_insert` / `plan_exists` / `plan_count`
  - computes   : `process_node` / `inject_depends` + `strip_dep_injected` / `permission.*`
  - fnfns      : 同步 fn 走 `set_fn` 回调桥；asyncFn 由 Host 执行
                 （`async_fn_refs` 取标识，`prepare_query` + `strip_query` 两段式）
  - federation : `plan_federated` 拆源摘要（sources / edges / degraded）与
                 `merge_federated` 合并结果

运行：python -m pytest core-py/test/parity_test.py
"""

import json
import pathlib
import sys

import pytest

# 直接指向 cargo 产物目录（`rust_store_py.pyd`），免装 wheel
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent / "dist"))

from rust_store_py import Registry  # noqa: E402

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


# ─── federation（跨库联邦） ─────────────────────────────────


def project_plan(plan):
    """把联邦计划投影成「结构性摘要」（对齐 Rust 侧 project_plan）"""
    sources = [
        {
            "key": s.get("key"),
            "source": s.get("source"),
            "namespace": s.get("namespace"),
            "model": s.get("model"),
            "mode": s.get("mode"),
        }
        for s in (plan.get("sources") or [])
    ]
    edges = (plan.get("join") or {}).get("edges") or []
    degraded = [d.get("code") for d in (plan.get("degraded") or []) if "code" in d]
    return {
        "v": plan.get("v"),
        "kind": plan.get("kind"),
        "root": plan.get("root"),
        "sources": sources,
        "edges": edges,
        "degraded": degraded,
        "hasPostprocess": plan.get("postprocess") is not None,
    }


def run_federation_plan(reg, fx):
    plan = reg.plan_federated(
        fx.get("gql") or "",
        fx.get("params") or {},
        fx.get("context"),
        fx.get("dsConfig"),
    )

    # 契约：postprocess 必须与单库 plan_query 同形状
    if fx.get("parity_with_query"):
        single = reg.plan_query(fx.get("gql") or "", fx.get("params") or {}, fx.get("context"))
        want = single.get("postprocess")
        got = plan.get("postprocess")
        if not deep_equal(want, got):
            raise RuntimeError(
                f"postprocess 与单库 plan_query 不一致\n    want: {fmt(want)}\n    got : {fmt(got)}"
            )

    return project_plan(plan)


def test_federation():
    cases = load("federation/cases.json")
    goldens = load("federation/expected.json")
    assert len(cases) == len(goldens), "输入与黄金基准用例数不一致"

    failures = []
    for fx, g in zip(cases, goldens):
        assert fx["name"] == g["name"], "用例顺序不一致"

        actual = None
        caught = None
        try:
            reg = make_registry(fx)
            if fx["kind"] == "plan":
                actual = run_federation_plan(reg, fx)
            else:
                actual = reg.merge_federated(fx.get("plan"), fx.get("results") or [])
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

    assert not failures, f"federation 对拍失败 {len(failures)} 项:\n" + "\n".join(failures)


# ─── dialect（Mongo 命令 → 关系型 SQL） ─────────────────────
#
# 与 core-node 用例逐条对应：
#   - core/tests/parity_dialect.rs          ：跨后端 SQL 归一化 / restore / introspect / overlay
#   - core-node/test/dialect.smoke.test.js  ：find / insert / count / $lookup 端到端用例输入
# 断言 core-py 四方法（dialect_translate / restore_rows / schema_from_rows / merge_schema）
# 的绑定输出，与 Rust 侧语义一致（仅差标识符引号与占位符风格）。

_DIALECT_SCHEMAS = [
    {"name": "Post", "collection": "posts", "timestamps": False,
     "fields": {"title": {"type": "string"}, "status": {"type": "string"}, "views": {"type": "int"}},
     "relations": {}},
    {"name": "Order", "collection": "orders", "timestamps": False,
     "fields": {"code": {"type": "string"}, "amount": {"type": "float"}},
     "relations": {"items": {"model": "OrderItem", "type": "many",
                             "localField": "_id", "foreignField": "orderId"}}},
    {"name": "OrderItem", "collection": "order_items", "timestamps": False,
     "fields": {"orderId": {"type": "string"}, "sku": {"type": "string"}, "qty": {"type": "int"}},
     "relations": {}},
]

_DIALECT_BACKENDS = ("mysql", "postgres", "sqlite")


def dialect_registry():
    reg = Registry()
    for s in _DIALECT_SCHEMAS:
        reg.register(s)
    return reg


def normalize_sql(sql):
    """去标识符引号、`$n` → `?`、折叠空白并小写（仅用于跨后端语义对比）"""
    s = sql.replace("`", "").replace('"', "")
    out = []
    i = 0
    while i < len(s):
        if s[i] == "$" and i + 1 < len(s) and s[i + 1].isdigit():
            while i < len(s) and (s[i] == "$" or s[i].isdigit()):
                i += 1
            out.append("?")
            continue
        out.append(s[i])
        i += 1
    return " ".join("".join(out).split()).lower()


def test_dialect():
    reg = dialect_registry()
    failures = []

    # 1) 跨后端归一化 SQL 一致（对应 parity_dialect.rs 的 assert_sql_parity 用例）
    cases = [
        ("find-simple", {"kind": "find", "collection": "posts",
                         "filter": {"status": "draft"}, "projection": {"title": 1, "status": 1}}),
        ("find-comparison", {"kind": "find", "collection": "posts",
                             "filter": {"views": {"$gte": 10}, "status": "draft"}, "projection": None}),
        ("count", {"kind": "countDocuments", "collection": "posts", "filter": {"status": "draft"}}),
        ("insert-one", {"kind": "insertOne", "collection": "posts",
                        "doc": {"title": "hello", "status": "draft", "views": 5}}),
        ("insert-many", {"kind": "insertMany", "collection": "posts",
                         "docs": [{"title": "a", "status": "draft", "views": 1},
                                  {"title": "b", "status": "draft", "views": 2}]}),
        ("aggregate-lookup", {"kind": "aggregate", "collection": "orders", "pipeline": [
            {"$match": {"amount": {"$gt": 0}}},
            {"$lookup": {"from": "order_items", "localField": "_id",
                         "foreignField": "orderId", "as": "items"}},
            {"$sort": {"code": 1}},
        ]}),
    ]
    for name, cmd in cases:
        base = None
        for backend in _DIALECT_BACKENDS:
            out = reg.dialect_translate(backend, cmd)
            if out.get("backend") != backend:
                failures.append(f"[{name}/{backend}] backend 字段回显错误: {out.get('backend')}")
            stmts = out.get("stmts") or []
            if len(stmts) != 1:
                failures.append(f"[{name}/{backend}] 应恰 1 条语句, got {len(stmts)}")
                continue
            norm = normalize_sql(stmts[0].get("text") or "")
            if base is None:
                base = norm
            elif norm != base:
                failures.append(f"[{name}] 跨后端 SQL 不一致\n    base: {base}\n    {backend}: {norm}")

    # 2) find：值参数化 + rowShape 投影列（对应 dialect.smoke.test.js find 用例）
    out = reg.dialect_translate("sqlite", {
        "kind": "find", "collection": "posts", "filter": {"status": "draft"},
        "projection": {"_id": 1, "title": 1, "status": 1, "views": 1}})
    st = out["stmts"][0]
    expect_deep_equal(failures, "find.params", st.get("params"), ["draft"])
    expect_deep_equal(failures, "find.rowShape.aliases",
                      sorted(c["alias"] for c in (st.get("rowShape") or {}).get("columns") or []),
                      ["_id", "status", "title", "views"])

    # 3) count：值参数化（对应 smoke test count 用例）
    out = reg.dialect_translate("sqlite", {
        "kind": "countDocuments", "collection": "posts", "filter": {"views": {"$gte": 2}}})
    expect_deep_equal(failures, "count.params", out["stmts"][0].get("params"), [2])

    # 4) restore_rows：平铺 JOIN 行 → 嵌套文档（对应 parity_dialect.rs 同名用例）
    agg = reg.dialect_translate("sqlite", {"kind": "aggregate", "collection": "orders", "pipeline": [
        {"$lookup": {"from": "order_items", "localField": "_id",
                     "foreignField": "orderId", "as": "items"}}]})
    shape = agg["stmts"][0]["rowShape"]
    rows = [
        {"_id": "A", "code": "A-1", "items_0_sku": "sku-a", "items_0_qty": 2},
        {"_id": "A", "code": "A-1", "items_0_sku": "sku-b", "items_0_qty": 3},
        {"_id": "B", "code": "B-1", "items_0_sku": None, "items_0_qty": None},
    ]
    restored = reg.restore_rows(shape, rows)
    expect_deep_equal(failures, "restore.len", len(restored), 2)
    a1 = next((d for d in restored if d.get("code") == "A-1"), None)
    expect_deep_equal(failures, "restore.A-1.items",
                      len(((a1 or {}).get("items") or [])), 2)
    expect_deep_equal(failures, "restore.A-1.skus",
                      sorted(i.get("sku") for i in ((a1 or {}).get("items") or [])), ["sku-a", "sku-b"])

    # 5) restore_rows：is_array 聚合路径（对应 parity_dialect.rs 同名用例）
    shape2 = {"columns": [
        {"alias": "_id", "path": ["_id"], "isArray": False, "subShape": None},
        {"alias": "code", "path": ["code"], "isArray": False, "subShape": None},
        {"alias": "it_0_sku", "path": ["items", "sku"], "isArray": True, "subShape": None},
    ]}
    rows2 = [{"_id": "1", "code": "A", "it_0_sku": "x"},
             {"_id": "1", "code": "A", "it_0_sku": "y"}]
    r2 = reg.restore_rows(shape2, rows2)
    expect_deep_equal(failures, "restore.is_array.len", len(r2), 1)
    expect_deep_equal(failures, "restore.is_array.skus",
                      [i.get("sku") for i in (r2[0].get("items") or [])], ["x", "y"])

    # 6) schema_from_rows：introspection 行 → schemaJSON（对应 parity_dialect.rs 同名用例）
    introspect_rows = {
        "tables": [{"name": "posts"}, {"name": "order_items"}],
        "columns": [
            {"table": "posts", "name": "_id", "type": "TEXT", "notnull": 1, "pk": 1},
            {"table": "posts", "name": "title", "type": "TEXT", "notnull": 0, "pk": 0},
            {"table": "posts", "name": "views", "type": "INTEGER", "notnull": 0, "pk": 0},
            {"table": "order_items", "name": "_id", "type": "TEXT", "notnull": 1, "pk": 1},
            {"table": "order_items", "name": "order_id", "type": "TEXT", "notnull": 1, "pk": 0},
            {"table": "order_items", "name": "sku", "type": "TEXT", "notnull": 0, "pk": 0},
        ],
        "fks": [{"table": "order_items", "column": "order_id",
                 "refTable": "posts", "refColumn": "_id"}],
        "indexes": [],
    }
    sch = reg.schema_from_rows(introspect_rows, "sqlite")
    posts = next((d for d in sch if d.get("name") == "posts"), None)
    oi = next((d for d in sch if d.get("name") == "order_items"), None)
    expect_deep_equal(failures, "introspect.views.type",
                      ((posts or {}).get("fields") or {}).get("views", {}).get("type"), "number")
    expect_deep_equal(failures, "introspect.order_items.rel.type",
                      ((oi or {}).get("relations") or {}).get("posts", {}).get("type"), "one")
    expect_deep_equal(failures, "introspect.posts.reverse.rel",
                      "order_items" in ((posts or {}).get("relations") or {}), True)

    # 7) merge_schema：字段并集 / 计算列注入 / 权限覆盖（对应 parity_dialect.rs 同名用例）
    base = [{"name": "posts", "collection": "posts",
             "fields": {"title": {"type": "string"}}, "relations": {}}]
    overlay = [{"name": "posts",
                "fields": {"views": {"type": "int", "default": 0}},
                "computes": {"slug": {"fn": True, "depends": ["title"]}},
                "read": {"roles": ["admin", "editor"]}}]
    merged = reg.merge_schema(base, overlay)
    d = merged[0]
    expect_deep_equal(failures, "merge.computes", "slug" in (d.get("computes") or {}), True)
    expect_deep_equal(failures, "merge.fields",
                      sorted((d.get("fields") or {}).keys()), ["title", "views"])
    expect_deep_equal(failures, "merge.read.roles", (d.get("read") or {}).get("roles"),
                      ["admin", "editor"])

    # 8) 错误路径：未知后端 → 抛出异常（不得返回错误值）
    with pytest.raises(RuntimeError):
        reg.dialect_translate("oracle", {"kind": "find", "collection": "posts"})

    assert not failures, f"dialect 对拍失败 {len(failures)} 项:\n" + "\n".join(failures)


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


# ─── 错误路径：非法 GQL / 未注册 model → 抛出异常（不得返回 {code} 字典） ─


def test_invalid_gql_and_unknown_model_raise():
    reg = Registry()
    with pytest.raises(RuntimeError, match="未注册"):
        reg.plan_query("Ghost{_id}", {}, None)
    with pytest.raises(RuntimeError, match="位置 0"):
        reg.build_pipeline("@@@", {}, None)
