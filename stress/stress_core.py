"""rust-store core 压力测试 —— 直压 core-py 绑定（无宿主层）

测试对象：rust-store 的产品 = core 本体（Rust 纯逻辑 + PyO3 绑定）。
本脚本实现一个**最小宿主**：只做「命令路由 + 驱动 IO + ID 随机源」，
全部纯逻辑（GQL 解析、schema 校验、命令规划、SQL 翻译、联邦计划、
结果还原/后处理）都经 core-py 逐次调用，压力集中在 core 的 FFI 与计算路径。

四库并载（MongoDB / MySQL / PostgreSQL / SQLite），每 worker 每轮 5 操作，
与 py-store/stress/stress.py、nodejs-store/scripts/stress.js 的操作模型对称：
  1. 联邦查询（plan_federated → 逐源执行 → merge_federated）
  2. SQLite 写（plan_insert → 执行）
  3. PostgreSQL 读（plan_count → 执行）
  4. MongoDB 读（plan_query → 执行）
  5. MySQL 写（plan_update → 执行）

用法: python stress_core.py [--workers N] [--rounds N]
表名后缀固定 'rs'，与 py-store('py') / nodejs-store('js') 的压测表隔离。
"""

import asyncio
import json
import os
import random
import string
import sys
import time
from pathlib import Path

import aiosqlite
import asyncmy
import asyncpg
from pymongo import AsyncMongoClient, ReturnDocument

# core-py 本地产物（maturin build 产物目录，含 rust_store_py.pyd）
_CORE_PY_DIST = Path(__file__).resolve().parent.parent / 'core-py' / 'dist'
sys.path.insert(0, str(_CORE_PY_DIST))
from rust_store_py import Registry  # noqa: E402


def _arg(name, default):
    argv = sys.argv[1:]
    if f'--{name}' in argv:
        return int(argv[argv.index(f'--{name}') + 1])
    return default


WORKERS = _arg('workers', 8)
ROUNDS = _arg('rounds', 200)
SUFFIX = 'rs'  # 表隔离后缀（本仓库专用）


def T(base):
    return base + SUFFIX


def S(base):
    return base + SUFFIX[:1].upper() + SUFFIX[1:]


MYSQL_URI = os.environ.get('MYSQL_URI', 'mysql://e2e:e2e123@127.0.0.1:3306/mongo_store_e2e?charset=utf8mb4')
PG_URI = os.environ.get('PG_URI', 'postgres://e2e:e2e123@127.0.0.1:5432/mongo_store_e2e')
MONGO_URI = os.environ.get('MONGO_URI', 'mongodb://127.0.0.1:27017/mongo_store_e2e')

SEED_USERS = 30
SEED_PER_USER = 2

USER_SCHEMA = S('StressUser')
FED_GQL = f'{USER_SCHEMA}($condition:@c0){{_id, name, orders{{code, amount}}, invoices{{title, value}}}}'
FED_PARAMS = {'c0': {}}

# ─── 最小宿主：ID 随机源（与 py-store/id.py 同语义） ─────────
_ID_CHARS = 'abcdefghijklmnopqrstuvwxyz0123456789'
_BASE36 = string.digits + string.ascii_lowercase


def _to_base36(n):
    if n == 0:
        return '0'
    out = []
    while n:
        n, r = divmod(n, 36)
        out.append(_BASE36[r])
    return ''.join(reversed(out))


def _gen_id(prefix):
    return prefix + _to_base36(int(time.time() * 1000)).upper() + ''.join(
        random.choice(_ID_CHARS).upper() for _ in range(4))


# ─── 最小宿主：命令执行（唯一 IO 边界） ─────────────────────
async def _exec_mongo(db, cmd):
    """Command JSON → PyMongo async 驱动调用（对齐 py-store/executors/mongo.py）"""
    coll = db[cmd['collection']]
    kind = cmd['kind']
    if kind == 'find':
        cursor = coll.find(cmd['filter'], cmd.get('projection'))
        return await cursor.to_list(length=None)
    if kind == 'aggregate':
        cursor = await coll.aggregate(cmd['pipeline'])
        return await cursor.to_list(length=None)
    if kind == 'countDocuments':
        return await coll.count_documents(cmd['filter'])
    if kind == 'findOne':
        return await coll.find_one(cmd['filter'], cmd.get('projection'))
    if kind == 'insertOne':
        await coll.insert_one(cmd['doc'])
        return cmd['doc']
    if kind == 'insertMany':
        await coll.insert_many(cmd['docs'])
        return {'insertedCount': len(cmd['docs'])}
    if kind == 'findOneAndUpdate':
        options = dict(cmd.get('options') or {})
        rd = options.pop('returnDocument', 'after')
        options['return_document'] = (
            ReturnDocument.AFTER if rd == 'after' else ReturnDocument.BEFORE)
        return await coll.find_one_and_update(cmd['filter'], cmd['update'], **options)
    if kind == 'updateMany':
        return await coll.update_many(cmd['filter'], cmd['update'])
    if kind == 'deleteMany':
        return await coll.delete_many(cmd['filter'])
    raise RuntimeError(f'未支持的命令: {kind}')


def _to_pyformat(text):
    """core 产 ``?`` 占位（对齐 mysql2），asyncmy 沿用 ``%s``：跳过引号内占位符仅替换"""
    out = []
    quote = None
    for ch in text:
        if quote is not None:
            out.append(ch)
            if ch == quote:
                quote = None
            continue
        if ch in ("'", '"', '`'):
            quote = ch
            out.append(ch)
            continue
        out.append('%s' if ch == '?' else ch)
    return ''.join(out)


def _affected(tag):
    if not tag:
        return 0
    last = str(tag).rsplit(' ', 1)[-1]
    return int(last) if last.isdigit() else 0


def _sqlite_bind(params):
    return [1 if v is True else 0 if v is False else v for v in (params or [])]


class MiniHost:
    """最小宿主：连接路由 + SQL translate→执行 + Mongo 驱动直调"""

    def __init__(self, core, mongo_db, m_pool, pg_pool, sqlite_conn):
        self.core = core
        self.conns = {
            'mongo_e2e': ('mongo', mongo_db),
            'mysql_e2e': ('mysql', m_pool),
            'pg_e2e': ('postgres', pg_pool),
            'default': ('sqlite', sqlite_conn),
        }

    async def _exec_sql(self, kind, driver, cmd):
        plan = self.core.dialect_translate(kind, cmd)
        if plan.get('unsupported'):
            raise RuntimeError(f"SQL 下推不支持: {plan['unsupported']}")
        docs = rows = None
        affected = 0
        for stmt in plan.get('stmts') or []:
            text = stmt['text']
            params = list(stmt.get('params') or [])
            shape = stmt.get('rowShape')
            if kind == 'mysql':
                from asyncmy.cursors import DictCursor
                async with driver.acquire() as conn, conn.cursor(DictCursor) as cur:
                    await cur.execute(_to_pyformat(text), params)
                    if cur.description is not None:
                        rows = [dict(r) for r in await cur.fetchall()]
                        if shape:
                            docs = self.core.restore_rows(shape, rows)
                    else:
                        affected = int(cur.rowcount or 0)
            elif kind == 'postgres':
                if shape:
                    records = await driver.fetch(text, *params)
                    rows = [dict(r) for r in records]
                    docs = self.core.restore_rows(shape, rows)
                else:
                    affected = _affected(await driver.execute(text, *params))
            else:  # sqlite
                cur = await driver.execute(text, _sqlite_bind(params))
                try:
                    if cur.description is not None:
                        cols = [d[0] for d in cur.description]
                        rows = [dict(zip(cols, r)) for r in await cur.fetchall()]
                        if shape:
                            docs = self.core.restore_rows(shape, rows)
                    else:
                        affected = int(cur.rowcount or 0)
                finally:
                    await cur.close()
        return {'docs': docs, 'rows': rows, 'affectedRows': affected}

    async def exec_on(self, source, cmd):
        kind, driver = self.conns[source or 'default']
        if kind == 'mongo':
            return await _exec_mongo(driver, cmd)
        out = await self._exec_sql(kind, driver, cmd)
        return self._shape(cmd, out)

    async def exec(self, cmd):
        return await self.exec_on(cmd.get('source') or 'default', cmd)

    @staticmethod
    def _shape(cmd, out):
        kind = cmd.get('kind')
        if kind in ('find', 'aggregate'):
            return out.get('docs') or []
        if kind in ('findOne', 'findOneAndUpdate'):
            docs = out.get('docs') or []
            return docs[0] if docs else None
        if kind == 'countDocuments':
            rows = out.get('rows')
            if not rows:
                return 0
            return next(iter(rows[0].values()), 0)
        if kind == 'insertOne':
            return cmd.get('doc')
        if kind == 'insertMany':
            return {'insertedCount': len(cmd.get('docs') or [])}
        if kind == 'updateMany':
            return {'modifiedCount': out.get('affectedRows')}
        if kind == 'deleteMany':
            return {'deletedCount': out.get('affectedRows')}
        return out

    # ── 读路径 ──────────────────────────────────────────────
    async def _run_plan(self, plan):
        if plan.get('mode') == 'two_phase':
            id_docs = await self.exec(plan['commands'][0])
            ids = [d['_id'] for d in id_docs]
            if not ids:
                return []
            cmd2 = self._resolve_ids(plan['commands'][1], ids)
            items = await self.exec(cmd2)
            return self.core.restore_sort_order(items, ids, plan.get('sort'))['items']
        return await self.exec(plan['commands'][0])

    def _finalize(self, plan, items):
        post = plan.get('postprocess')
        if not post:
            return items
        prepared = self.core.prepare_query(post, items, None)
        return self.core.strip_query(post, prepared['items'])['items']

    async def query(self, gql, params=None):
        plan = self.core.plan_query(gql, params if params is not None else {}, None, None)
        return self._finalize(plan, await self._run_plan(plan))

    async def query_federated(self, gql, params=None):
        plan = self.core.plan_federated(gql, params if params is not None else {}, None)
        results = []
        for unit in plan.get('sources') or []:
            commands = unit.get('commands') or []
            if unit.get('mode') == 'two_phase':
                id_docs = await self.exec_on(unit['source'], commands[0])
                ids = [d['_id'] for d in id_docs]
                if not ids:
                    results.append([])
                    continue
                cmd2 = self._resolve_ids(commands[1], ids)
                items = await self.exec_on(unit['source'], cmd2)
                results.append(
                    self.core.restore_sort_order(items, ids, unit.get('sort'))['items'])
            else:
                results.append(await self.exec_on(unit['source'], commands[0]))
        merged = self.core.merge_federated(plan, results)
        return self._finalize(plan, merged)

    @staticmethod
    def _resolve_ids(command, ids):
        """替换 {{phase1.ids}} 占位符（本压测只需这一种）"""
        def sub(v):
            if isinstance(v, str):
                return ids if v == '{{phase1.ids}}' else v
            if isinstance(v, list):
                return [sub(x) for x in v]
            if isinstance(v, dict):
                return {k: sub(x) for k, x in v.items()}
            return v
        return sub(command)

    # ── 写路径 / 计数 ────────────────────────────────────────
    async def insert(self, schema_name, data, id_prefix=''):
        plan = self.core.plan_insert(
            schema_name, data, int(time.time() * 1000),
            _gen_id(id_prefix) if id_prefix else '', None, None)
        await self.exec(plan['command'])
        return plan['returns']

    async def update(self, schema_name, condition, data):
        out = self.core.plan_update(
            schema_name, condition, data, None, int(time.time() * 1000), None, None, None, None)
        if out.get('needsProbe'):
            probe_doc = await self.exec(out['needsProbe'])
            out = self.core.plan_update(
                schema_name, condition, data, None, int(time.time() * 1000), None,
                probe_doc is not None, probe_doc, None)
        result = await self.exec(out['command'])
        return self.core.apply_write_defaults(schema_name, result) if result else None

    async def count(self, schema_name, flt=None):
        cmd = self.core.plan_count(schema_name, flt, None)
        return await self.exec(cmd)


def _summarize(latencies):
    if not latencies:
        return {'ops': 0, 'total_ms': 0, 'p50_ms': 0, 'p95_ms': 0, 'max_ms': 0, 'avg_ms': 0}
    s = sorted(latencies)

    def percentile(p):
        return s[min(len(s) - 1, max(0, int(p * len(s)) - 1))]

    return {
        'ops': len(s),
        'total_ms': round(sum(s)),
        'p50_ms': percentile(0.5),
        'p95_ms': percentile(0.95),
        'max_ms': s[-1],
        'avg_ms': round(sum(s) / len(s), 2),
    }


def _parse_mysql_uri(uri):
    from urllib.parse import unquote, urlparse
    u = urlparse(uri)
    return {
        'host': u.hostname or '127.0.0.1',
        'port': u.port or 3306,
        'user': unquote(u.username or ''),
        'password': unquote(u.password or ''),
        'db': (u.path or '/').lstrip('/'),
    }


async def main():
    core = Registry()

    # 1. 连接四库
    client = AsyncMongoClient(MONGO_URI, serverSelectionTimeoutMS=3000)
    mongo_db = client.get_default_database()
    m_pool = await asyncmy.create_pool(autocommit=True, **_parse_mysql_uri(MYSQL_URI))
    pg_pool = await asyncpg.create_pool(dsn=PG_URI)
    sqlite_conn = await aiosqlite.connect(':memory:')
    sqlite_lock = asyncio.Lock()  # SQLite 单连接串行化
    host = MiniHost(core, mongo_db, m_pool, pg_pool, sqlite_conn)

    # 2. 建表 / 清数据（幂等）
    await mongo_db[T('stress_users')].delete_many({})
    async with m_pool.acquire() as conn, conn.cursor() as cur:
        await cur.execute(f'DROP TABLE IF EXISTS {T("stress_orders")}')
        await cur.execute(
            f'CREATE TABLE {T("stress_orders")} (_id VARCHAR(64) NOT NULL, `userId` VARCHAR(64), '
            '`code` VARCHAR(255), `amount` DOUBLE, PRIMARY KEY (_id)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4')
    await pg_pool.execute(f'DROP TABLE IF EXISTS {T("stress_invoices")}')
    await pg_pool.execute(
        f'CREATE TABLE {T("stress_invoices")} (_id TEXT PRIMARY KEY, "userId" TEXT, "title" TEXT, "value" DOUBLE PRECISION)')
    await sqlite_conn.execute(f'DROP TABLE IF EXISTS {T("stress_logs")}')
    await sqlite_conn.execute(f'CREATE TABLE {T("stress_logs")} (_id TEXT PRIMARY KEY, msg TEXT, level TEXT)')
    await sqlite_conn.commit()

    # 3. schema 注册（直调 core.register，跨源 relation = 联邦路径）
    core.register({
        'name': S('StressUser'), 'collection': T('stress_users'), 'idPrefix': 'u_', 'datasource': 'mongo_e2e',
        'timestamps': False,
        'fields': {'name': {'type': 'string'}},
        'relations': {
            'orders': {'model': S('StressOrder'), 'type': 'many', 'localField': '_id', 'foreignField': 'userId'},
            'invoices': {'model': S('StressInvoice'), 'type': 'many', 'localField': '_id', 'foreignField': 'userId'},
        },
    })
    core.register({
        'name': S('StressOrder'), 'collection': T('stress_orders'), 'idPrefix': 'o_', 'datasource': 'mysql_e2e',
        'timestamps': False,
        'fields': {'userId': {'type': 'string'}, 'code': {'type': 'string'}, 'amount': {'type': 'number'}},
        'relations': {},
    })
    core.register({
        'name': S('StressInvoice'), 'collection': T('stress_invoices'), 'idPrefix': 'v_', 'datasource': 'pg_e2e',
        'timestamps': False,
        'fields': {'userId': {'type': 'string'}, 'title': {'type': 'string'}, 'value': {'type': 'number'}},
        'relations': {},
    })
    core.register({
        'name': S('StressLog'), 'collection': T('stress_logs'), 'idPrefix': 'l_', 'datasource': 'default',
        'timestamps': False,
        'fields': {'msg': {'type': 'string'}, 'level': {'type': 'string'}},
        'relations': {},
    })

    # 4. 预热（种子数据，直调 core 规划 + 最小宿主执行）
    order_ids = []
    for u in range(SEED_USERS):
        user = await host.insert(S('StressUser'), {'name': f'u_{u}'}, 'u_')
        for i in range(SEED_PER_USER):
            o = await host.insert(S('StressOrder'),
                                  {'userId': user['_id'], 'code': f'c_{u}_{i}', 'amount': 1}, 'o_')
            order_ids.append(o['_id'])
            await host.insert(S('StressInvoice'),
                              {'userId': user['_id'], 'title': f'v_{u}_{i}', 'value': u + i}, 'v_')
    print(f'[prewarm] users={SEED_USERS} orders={len(order_ids)} invoices={SEED_USERS * SEED_PER_USER} ready', flush=True)

    # 5. 冒烟：跨 3 源联邦一次，验证链路
    smoke = await host.query_federated(FED_GQL, FED_PARAMS)
    if not smoke or not smoke[0].get('orders') or not smoke[0].get('invoices'):
        raise RuntimeError(f'联邦冒烟失败: 结果形状异常 {json.dumps((smoke[0] if smoke else {}), ensure_ascii=False)[:200]}')
    print(f"[smoke] federation ok: {len(smoke)} users, sample orders={len(smoke[0]['orders'])} invoices={len(smoke[0]['invoices'])}", flush=True)

    # 6. 并发压测
    all_times = []
    fed_times = []
    errors_by_type = {}

    def record_error(op, e):
        key = f'{op}: {e}'
        errors_by_type[key] = errors_by_type.get(key, 0) + 1

    async def worker(w):
        for r in range(ROUNDS):
            # op1: 联邦查询（跨 Mongo+MySQL+PG）
            t0 = time.perf_counter()
            try:
                await host.query_federated(FED_GQL, FED_PARAMS)
                all_times.append((time.perf_counter() - t0) * 1000)
                fed_times.append((time.perf_counter() - t0) * 1000)
            except Exception as e:
                record_error('federated', e)
            # op2: SQLite 写
            t0 = time.perf_counter()
            try:
                async with sqlite_lock:
                    await host.insert(S('StressLog'), {'msg': f'w{w}_r{r}', 'level': 'info'}, 'l_')
                all_times.append((time.perf_counter() - t0) * 1000)
            except Exception as e:
                record_error('sqlite-insert', e)
            # op3: PG 读
            t0 = time.perf_counter()
            try:
                await host.count(S('StressInvoice'), {})
                all_times.append((time.perf_counter() - t0) * 1000)
            except Exception as e:
                record_error('pg-count', e)
            # op4: Mongo 读
            t0 = time.perf_counter()
            try:
                await host.query(f'{S("StressUser")}($condition:@c0){{_id, name}}',
                                 {'c0': {'name': f'u_{(w + r) % SEED_USERS}'}})
                all_times.append((time.perf_counter() - t0) * 1000)
            except Exception as e:
                record_error('mongo-query', e)
            # op5: MySQL 写
            t0 = time.perf_counter()
            try:
                oid = order_ids[(w * ROUNDS + r) % len(order_ids)]
                await host.update(S('StressOrder'), {'_id': oid}, {'$inc': {'amount': 1}})
                all_times.append((time.perf_counter() - t0) * 1000)
            except Exception as e:
                record_error('mysql-update', e)

    t_start = time.perf_counter()
    await asyncio.gather(*(worker(w) for w in range(WORKERS)))
    total_ms = (time.perf_counter() - t_start) * 1000

    # 7. 汇总输出
    summary = _summarize(all_times)
    fed = _summarize(fed_times)
    out = {
        'product': 'rust-store (core-py direct)',
        'suffix': SUFFIX,
        'workers': WORKERS,
        'rounds': ROUNDS,
        'ops_per_round': 5,
        'total_ops': summary['ops'],
        'errors': sum(errors_by_type.values()),
        'errors_by_type': errors_by_type,
        'total_ms': round(total_ms),
        'qps': round(summary['ops'] / (total_ms / 1000), 2),
        'all_ops': summary,
        'federation_ops': fed,
    }
    print('[RESULT]' + json.dumps(out))

    # 清理
    m_pool.close()  # asyncmy 为同步 close
    await pg_pool.close()
    await sqlite_conn.close()
    await client.close()


if __name__ == '__main__':
    try:
        asyncio.run(main())
    except Exception as e:  # noqa: BLE001
        import traceback
        traceback.print_exc()
        print(f'[stress rust-store] FAIL: {e}', file=sys.stderr)
        sys.exit(1)
