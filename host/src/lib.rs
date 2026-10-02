//! rust-store —— rust-store-core 的纯 Rust 宿主（nodejs-store / py-store 的 Rust 孪生）。
//!
//! core 承载全部纯逻辑（GQL 解析 / 权限 / 命令规划 / SQL 方言翻译），本 crate 只做宿主三件事：
//! 驱动 IO（v0：SQLite via sqlx）、供给 now 与 new_id（core 无时钟无随机源）、
//! 归档与两阶段等命令序列编排。
//!
//! v1 范围（诚实声明）：单 SQL 源（SQLite / MySQL / PostgreSQL）的 query / query_one /
//! insert / update / remove；两阶段查询、探针重入、归档编排（归档+删除事务化）已实现；
//! Mongo 源、联邦与 mutation 步骤编排属后续阶段。

pub mod exec;
pub mod id;

use std::sync::RwLock;

use crate::id::now_ms;
use exec::Pool;

use rust_store_core::command::{
    finalize_query, plan_archive_docs, plan_insert, plan_query, plan_query_one, plan_remove,
    plan_update, restore_sort_order, Probe, PHASE1_IDS,
};
use rust_store_core::dialect::Backend;
use rust_store_core::permission::Context;
use rust_store_core::schema::Registry;
use serde_json::{json, Map, Value};
use sqlx::MySqlPool;
use sqlx::PgPool;
use sqlx::SqlitePool;

pub use exec::REL_PRED_IDS;

/// 归档 + 删除事务段（sqlx 原生 Transaction，按后端宏生成三份）。
/// 归档命令由同步闭包 `make_archive` 依 find 结果在事务内生成（core 纯逻辑，持 registry 读锁）。
/// 任意一步失败：显式 rollback 并原样上抛（禁「已删未归档」静默失守）。
macro_rules! run_remove_tx {
    ($fn_name:ident, $conn_ty:ty, $variant:ident) => {
        async fn $fn_name(
            conn: &mut $conn_ty,
            find: Option<&exec::Translated>,
            make_archive: &mut (dyn FnMut(&[Value]) -> Result<Option<exec::Translated>, String>
                      + Send),
            delete: &exec::Translated,
        ) -> Result<(u64, u64), String> {
            use sqlx::Connection as _;
            let mut tx = conn
                .begin()
                .await
                .map_err(|e| format!("开启事务失败: {e}"))?;
            let seq = async {
                let mut archived: u64 = 0;
                if let Some(t) = find {
                    let outcome = exec::exec_translated(exec::Conn::$variant(&mut *tx), t).await?;
                    if !outcome.docs.is_empty() {
                        if let Some(arch) = make_archive(&outcome.docs)? {
                            let a = exec::exec_translated(exec::Conn::$variant(&mut *tx), &arch)
                                .await?;
                            archived = a.changes;
                        }
                    }
                }
                let deleted = exec::exec_translated(exec::Conn::$variant(&mut *tx), delete)
                    .await?
                    .changes;
                Ok((deleted, archived))
            }
            .await;
            match seq {
                Ok(v) => {
                    tx.commit()
                        .await
                        .map_err(|e| format!("事务提交失败: {e}"))?;
                    Ok(v)
                }
                Err(e) => {
                    let _ = tx.rollback().await;
                    Err(e)
                }
            }
        }
    };
}

run_remove_tx!(run_remove_tx_sqlite, sqlx::SqliteConnection, Sqlite);
run_remove_tx!(run_remove_tx_mysql, sqlx::MySqlConnection, Mysql);
run_remove_tx!(run_remove_tx_pg, sqlx::PgConnection, Postgres);

/// 纯 Rust 宿主。`Registry` 用 RwLock 包裹：注册期写、运行期读（plan/translate/finalize 全是 &Registry）。
pub struct Store {
    registry: RwLock<Registry>,
    pool: Pool,
    backend: Backend,
    rng: RwLock<u64>,
}

impl Store {
    /// 连接一个数据源，按 URL scheme 分派后端：
    /// `sqlite://…`/`sqlite::memory:` → SQLite；`mysql://…` → MySQL；`postgres://…`/`postgresql://…` → PG。
    pub async fn connect(url: &str) -> Result<Self, String> {
        let pool = if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            Pool::Postgres(
                PgPool::connect(url)
                    .await
                    .map_err(|e| format!("PostgreSQL 连接失败: {e}"))?,
            )
        } else if url.starts_with("mysql://") {
            Pool::Mysql(
                MySqlPool::connect(url)
                    .await
                    .map_err(|e| format!("MySQL 连接失败: {e}"))?,
            )
        } else if url.starts_with("sqlite:") {
            Pool::Sqlite(
                SqlitePool::connect(url)
                    .await
                    .map_err(|e| format!("SQLite 连接失败: {e}"))?,
            )
        } else {
            return Err(format!(
                "无法识别的数据源 URL（{url}）：支持 sqlite:// / mysql:// / postgres://"
            ));
        };
        let backend = match pool {
            Pool::Sqlite(_) => Backend::Sqlite,
            Pool::Mysql(_) => Backend::Mysql,
            Pool::Postgres(_) => Backend::Postgres,
        };
        Ok(Store {
            registry: RwLock::new(Registry::new()),
            pool,
            backend,
            rng: RwLock::new(now_ms() as u64 ^ 0x9E3779B97F4A7C15),
        })
    }

    /// 连接 SQLite（`Store::connect` 的 SQLite 便捷入口，行为等价）
    pub async fn connect_sqlite(url: &str) -> Result<Self, String> {
        Self::connect(url).await
    }

    /// 注册一个 schema 定义（与 nodejs-store `store.register(defn)` 同一 JSON 契约；
    /// core 侧自动派生 `<Name>Deleted` 归档表）。
    pub fn register(&self, defn: &Value) -> Result<(), String> {
        let mut w = self
            .registry
            .write()
            .map_err(|_| "registry 写锁中毒".to_string())?;
        w.register(defn)
    }

    /// 开关「必须带上下文」（core `ensure_context`；默认关闭 = fail-open，与双宿主一致）
    pub fn set_require_context(&self, require: bool) {
        if let Ok(mut w) = self.registry.write() {
            w.set_require_context(require);
        }
    }

    /// 注入/清除 RBAC 动态策略（`None` = 关闭）；解析失败显式报错（fail-fast）。
    /// 判决唯一在 core：本方法仅配置注入，plan 链路拦截自动生效。
    pub fn set_rbac(&self, policy: Option<&Value>) -> Result<(), String> {
        let mut w = self
            .registry
            .write()
            .map_err(|_| "registry 写锁中毒".to_string())?;
        w.set_rbac(policy)
    }

    /// RBAC 策略是否已注入
    pub fn rbac_enabled(&self) -> bool {
        self.read_reg().map(|r| r.rbac().is_some()).unwrap_or(false)
    }

    /// 豁免角色清单（命中者在一切判决环节直接放行）。判决唯一在 core：本方法仅配置注入。
    pub fn set_exempt_roles(&self, roles: Vec<String>) -> Result<(), String> {
        let mut w = self
            .registry
            .write()
            .map_err(|_| "registry 写锁中毒".to_string())?;
        w.set_exempt_roles(roles);
        Ok(())
    }

    /// 拒写角色清单（命中者一切写路径拒绝，读不受影响）
    pub fn set_deny_write_roles(&self, roles: Vec<String>) -> Result<(), String> {
        let mut w = self
            .registry
            .write()
            .map_err(|_| "registry 写锁中毒".to_string())?;
        w.set_deny_write_roles(roles);
        Ok(())
    }

    /// 未配置姿态（"open" / "closed"）；未知值显式报错（fail-fast）
    pub fn set_unconfigured_policy(&self, policy: &str) -> Result<(), String> {
        let p = rust_store_core::permission::UnconfiguredPolicy::from_str_or_err(policy)?;
        let mut w = self
            .registry
            .write()
            .map_err(|_| "registry 写锁中毒".to_string())?;
        w.set_unconfigured_policy(p);
        Ok(())
    }

    /// 底层连接池（DDL / 维护脚本逃生口；日常读写走 Store API）
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// SQLite 连接池（仅 SQLite 源；其余后端显式报错，禁静默返回空池）
    pub fn sqlite_pool(&self) -> Result<&SqlitePool, String> {
        match &self.pool {
            Pool::Sqlite(p) => Ok(p),
            _ => Err("非 SQLite 源：请改用 pool() 并按后端类型访问".to_string()),
        }
    }

    /// 在本源上原样执行维护语句（DDL / 清理脚本逃生口；逐条执行，任何失败显式报错）
    pub async fn execute_ddl(&self, stmts: &[&str]) -> Result<(), String> {
        let mut conn = self.pool.acquire().await.map_err(|e| e.to_string())?;
        for s in stmts {
            let outcome = exec::exec_translated(
                conn.conn(),
                &exec::Translated {
                    stmts: vec![exec::SqlStmtJson {
                        text: (*s).to_string(),
                        params: Vec::new(),
                        is_write: !s.trim().to_uppercase().starts_with("SELECT"),
                        row_shape: Value::Null,
                        returning: Vec::new(),
                    }],
                },
            )
            .await?;
            let _ = outcome;
        }
        Ok(())
    }

    pub fn list_schemas(&self) -> Vec<String> {
        exec::with_registry(&self.registry, |reg| Ok(reg.list())).unwrap_or_default()
    }

    /// schema 的标量字段名列表（注册顺序；供 HTTP 适配层生成显式投影——
    /// GQL 无投影的契约语义是「只返回 _id」，见 core compute_keep）
    pub fn schema_fields(&self, schema_name: &str) -> Result<Vec<String>, String> {
        exec::with_registry(&self.registry, |reg| {
            Ok(reg.get(schema_name)?.fields.keys().cloned().collect())
        })
    }

    /// GQL 查询（列表）。
    /// `params` 即 `@name` 命名参数绑定（core 规划期消费）。
    pub async fn query(
        &self,
        gql: &str,
        params: &Map<String, Value>,
        ctx: Option<&Context>,
    ) -> Result<Vec<Value>, String> {
        // ① 持锁：规划 + 翻译第一条命令
        let (plan, translated) = {
            let plan = {
                let reg = self.read_reg()?;
                plan_query(gql, params, &reg, ctx)?
            };
            let t0 = exec::translate_command(self.backend, &plan.commands[0], &self.registry)?;
            (plan, t0)
        };

        // ② 执行第一条
        let mut conn = self.pool.acquire().await.map_err(|e| e.to_string())?;
        let outcome = exec::exec_translated(conn.conn(), &translated).await?;

        // ③ 两阶段：第一阶段取 ids → 替换占位符 → 执行第二阶段 → 还原排序
        if plan.mode == rust_store_core::command::Mode::TwoPhase {
            let ids = exec::extract_ids(&outcome.docs);
            let mut second = plan
                .commands
                .get(1)
                .cloned()
                .ok_or_else(|| "TwoPhase 计划缺少第二条命令".to_string())?;
            exec::substitute_ids(&mut second, PHASE1_IDS, &ids);
            let t1 = exec::translate_command(self.backend, &second, &self.registry)?;
            let o1 = exec::exec_translated(conn.conn(), &t1).await?;
            let mut items = o1.docs;
            exec::with_registry(&self.registry, |_reg| {
                restore_sort_order(&mut items, &ids, plan.sort.as_ref());
                Ok(())
            })?;
            drop(conn);
            return self.finalize(plan.postprocess.as_ref(), items, ctx).await;
        }
        let items = outcome.docs;
        drop(conn);
        self.finalize(plan.postprocess.as_ref(), items, ctx).await
    }

    /// GQL 查询（单条；core 自动注入 `$limit(1)`）
    pub async fn query_one(
        &self,
        gql: &str,
        params: &Map<String, Value>,
        ctx: Option<&Context>,
    ) -> Result<Option<Value>, String> {
        let plan = {
            let reg = self.read_reg()?;
            plan_query_one(gql, params, &reg, ctx)?
        };
        let translated = exec::translate_command(self.backend, &plan.commands[0], &self.registry)?;
        let mut conn = self.pool.acquire().await.map_err(|e| e.to_string())?;
        let outcome = exec::exec_translated(conn.conn(), &translated).await?;
        drop(conn);
        let items = outcome.docs;
        let items = self.finalize(plan.postprocess.as_ref(), items, ctx).await?;
        Ok(items.into_iter().next())
    }

    /// 插入一条（返回 core 产出的 `returns` 文档；autoincrement 主键回读 `_id` 后补入）
    pub async fn insert(
        &self,
        schema_name: &str,
        data: &Value,
        ctx: Option<&Context>,
    ) -> Result<Value, String> {
        let now = id::now();
        let new_id = self.next_id(schema_name)?;
        let plan = {
            let reg = self.read_reg()?;
            plan_insert(schema_name, &reg, ctx, data, now, &new_id, None)?
        };
        let command = plan
            .get("command")
            .cloned()
            .ok_or_else(|| "plan_insert 缺少 command".to_string())?;
        let mut returns = plan
            .get("returns")
            .cloned()
            .ok_or_else(|| "plan_insert 缺少 returns".to_string())?;

        let mut conn = self.pool.acquire().await.map_err(|e| e.to_string())?;
        let translated = exec::translate_command(self.backend, &command, &self.registry)?;
        let outcome = exec::exec_translated(conn.conn(), &translated).await?;
        drop(conn);

        // autoincrement：INSERT ... RETURNING _id → 回读补入 returns（对齐 nodejs write.js:22-35）
        if outcome.docs.len() == 1 {
            if let Some(new_pk) = outcome.docs[0].get("_id") {
                if let Some(obj) = returns.as_object_mut() {
                    obj.insert("_id".to_string(), new_pk.clone());
                }
            }
        }
        Ok(returns)
    }

    /// 更新（条件命中即返回更新后文档；条件未命中返回 None；探针重入按 core write 契约编排）
    pub async fn update(
        &self,
        schema_name: &str,
        condition: &Value,
        data: &Value,
        ctx: Option<&Context>,
    ) -> Result<Option<Value>, String> {
        let now = id::now();
        let command = self
            .plan_update_with_probe(schema_name, condition, data, ctx, now)
            .await?;
        let mut conn = self.pool.acquire().await.map_err(|e| e.to_string())?;
        let translated = exec::translate_command(self.backend, &command, &self.registry)?;
        let outcome = exec::exec_translated(conn.conn(), &translated).await?;
        drop(conn);
        Ok(outcome.docs.into_iter().next())
    }

    /// 删除（归档编排：find 源文档 → plan_archive_docs 写归档表 → deleteMany；
    /// 关系谓词条件先执行 preCommand 取命中 `_id`。返回 `{deletedCount, archivedCount}`）
    ///
    /// 返回类型显式 BoxFuture：事务借用链（Txn→Conn 枚举）在 async-trait 的
    /// `Box<dyn Future + Send>` 泛化检查下触发保守误报（"not general enough"），
    /// 显式具体化 future + Send 边界，调用方（如 store-api 适配层）即可直接 await。
    pub fn remove<'a>(
        &'a self,
        schema_name: &'a str,
        condition: &'a Value,
        ctx: Option<&'a Context>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send + 'a>>
    {
        Box::pin(self.remove_inner(schema_name, condition, ctx))
    }

    async fn remove_inner(
        &self,
        schema_name: &str,
        condition: &Value,
        ctx: Option<&Context>,
    ) -> Result<Value, String> {
        let mut plan = {
            let reg = self.read_reg()?;
            plan_remove(schema_name, &reg, ctx, condition, Probe::NotProbed)?
        };
        if let Some(probe_cmd) = plan.get("needsProbe").cloned() {
            let found = self.run_probe(&probe_cmd).await?;
            plan = {
                let reg = self.read_reg()?;
                match &found {
                    Some(doc) => plan_remove(schema_name, &reg, ctx, condition, Probe::Found(doc))?,
                    None => plan_remove(schema_name, &reg, ctx, condition, Probe::NoResult)?,
                }
            };
        }

        let mut delete_command = plan
            .get("deleteCommand")
            .cloned()
            .ok_or_else(|| "plan_remove 缺少 deleteCommand".to_string())?;

        // 关系谓词：deleteCommand.preCommand（aggregate 取 `_id`）→ 替换 __REL_PRED_IDS__
        let pre = delete_command.get("preCommand").cloned();
        if let Some(pre_cmd) = pre {
            let translated = exec::translate_command(self.backend, &pre_cmd, &self.registry)?;
            let mut conn = self.pool.acquire().await.map_err(|e| e.to_string())?;
            let outcome = exec::exec_translated(conn.conn(), &translated).await?;
            drop(conn);
            let ids = exec::extract_ids(&outcome.docs);
            if let Some(obj) = delete_command.as_object_mut() {
                if let Some(filter) = obj.get_mut("filter") {
                    exec::substitute_ids(filter, REL_PRED_IDS, &ids);
                }
            }
        }

        // 归档 + 删除编排：同一事务内顺序执行（对齐 nodejs write.js:167-168 runAtomic）——
        // 归档落库与源删除原子生效，任一步失败显式回滚（禁「已删未归档」的静默失守）。
        // 事务按后端宏分派（sqlx 原生 Transaction；归档命令由同步闭包在事务内依 find 结果生成）
        let find_translated = match plan.get("findCommand").cloned().filter(|v| !v.is_null()) {
            Some(cmd) => Some(exec::translate_command(self.backend, &cmd, &self.registry)?),
            None => None,
        };
        let delete_translated =
            exec::translate_command(self.backend, &delete_command, &self.registry)?;
        let (backend, registry) = (&self.backend, &self.registry);
        let mut make_archive = move |docs: &[Value]| -> Result<Option<exec::Translated>, String> {
            let archive_plan = exec::with_registry(registry, |reg| {
                plan_archive_docs(schema_name, reg, docs, id::now())
            })?;
            match archive_plan.get("command").cloned() {
                Some(cmd) if !cmd.is_null() => {
                    exec::translate_command(*backend, &cmd, registry).map(Some)
                }
                _ => Ok(None),
            }
        };

        let (deleted_count, archived_count) = match &self.pool {
            Pool::Sqlite(p) => {
                let mut conn = p.acquire().await.map_err(|e| e.to_string())?;
                run_remove_tx_sqlite(
                    &mut *conn,
                    find_translated.as_ref(),
                    &mut make_archive,
                    &delete_translated,
                )
                .await
            }
            Pool::Mysql(p) => {
                let mut conn = p.acquire().await.map_err(|e| e.to_string())?;
                run_remove_tx_mysql(
                    &mut *conn,
                    find_translated.as_ref(),
                    &mut make_archive,
                    &delete_translated,
                )
                .await
            }
            Pool::Postgres(p) => {
                let mut conn = p.acquire().await.map_err(|e| e.to_string())?;
                run_remove_tx_pg(
                    &mut *conn,
                    find_translated.as_ref(),
                    &mut make_archive,
                    &delete_translated,
                )
                .await
            }
        }?;

        Ok(json!({
            "deletedCount": deleted_count,
            "archivedCount": archived_count,
        }))
    }

    // ── 内部 ──

    fn read_reg(&self) -> Result<std::sync::RwLockReadGuard<'_, Registry>, String> {
        self.registry
            .read()
            .map_err(|_| "registry 读锁中毒（注册线程 panic）".to_string())
    }

    fn next_id(&self, schema_name: &str) -> Result<String, String> {
        let prefix = {
            let reg = self.read_reg()?;
            reg.get(schema_name)?.id_prefix.clone()
        };
        let mut rng = self.rng.write().map_err(|_| "rng 锁中毒".to_string())?;
        Ok(id::generate_id(&prefix, &mut rng))
    }

    async fn finalize(
        &self,
        postprocess: Option<&Value>,
        mut items: Vec<Value>,
        ctx: Option<&Context>,
    ) -> Result<Vec<Value>, String> {
        let post = postprocess.cloned().unwrap_or(Value::Null);
        exec::with_registry(&self.registry, |reg| {
            finalize_query(&post, &mut items, reg, None, ctx)
        })?;
        Ok(items)
    }

    /// update 的探针重入编排（对齐 nodejs write.js:12-19 `_planWithProbe`）：
    /// NotProbed → needsProbe → 执行探针 findOne → Found/NoResult 重入得最终 command
    async fn plan_update_with_probe(
        &self,
        schema_name: &str,
        condition: &Value,
        data: &Value,
        ctx: Option<&Context>,
        now: i64,
    ) -> Result<Value, String> {
        let first = {
            let reg = self.read_reg()?;
            plan_update(
                schema_name,
                &reg,
                ctx,
                condition,
                data,
                &json!({}),
                now,
                Probe::NotProbed,
            )?
        };
        match first.get("needsProbe").cloned() {
            Some(probe_cmd) => {
                let found = self.run_probe(&probe_cmd).await?;
                let reg = self.read_reg()?;
                match &found {
                    Some(doc) => plan_update(
                        schema_name,
                        &reg,
                        ctx,
                        condition,
                        data,
                        &json!({}),
                        now,
                        Probe::Found(doc),
                    ),
                    None => plan_update(
                        schema_name,
                        &reg,
                        ctx,
                        condition,
                        data,
                        &json!({}),
                        now,
                        Probe::NoResult,
                    ),
                }
            }
            None => Ok(first
                .get("command")
                .cloned()
                .ok_or_else(|| "plan_update 缺少 command".to_string())?),
        }
    }

    /// 执行探针命令（cmd_find_one）取条件命中文档
    async fn run_probe(&self, probe_cmd: &Value) -> Result<Option<Value>, String> {
        let translated = exec::translate_command(self.backend, probe_cmd, &self.registry)?;
        let mut conn = self.pool.acquire().await.map_err(|e| e.to_string())?;
        let outcome = exec::exec_translated(conn.conn(), &translated).await?;
        drop(conn);
        Ok(outcome.docs.into_iter().next())
    }
}
