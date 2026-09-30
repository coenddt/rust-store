//! rust-store —— rust-store-core 的纯 Rust 宿主（nodejs-store / py-store 的 Rust 孪生）。
//!
//! core 承载全部纯逻辑（GQL 解析 / 权限 / 命令规划 / SQL 方言翻译），本 crate 只做宿主三件事：
//! 驱动 IO（v0：SQLite via sqlx）、供给 now 与 new_id（core 无时钟无随机源）、
//! 归档与两阶段等命令序列编排。
//!
//! v0 范围（诚实声明）：单 SQLite 源的 query / query_one / insert / update / remove；
//! 两阶段查询、探针重入、归档编排（归档+删除事务化）已实现；
//! Mongo 源、MySQL/PG 连接、联邦与 mutation 步骤编排属后续阶段。

pub mod exec;
pub mod id;

use std::sync::RwLock;

use crate::id::now_ms;

use rust_store_core::command::{
    finalize_query, plan_archive_docs, plan_insert, plan_query, plan_query_one, plan_remove,
    plan_update, restore_sort_order, Probe, PHASE1_IDS,
};
use rust_store_core::dialect::Backend;
use rust_store_core::permission::Context;
use rust_store_core::schema::Registry;
use serde_json::{json, Map, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Connection as _, SqlitePool};

pub use exec::REL_PRED_IDS;

/// 纯 Rust 宿主。`Registry` 用 RwLock 包裹：注册期写、运行期读（plan/translate/finalize 全是 &Registry）。
pub struct Store {
    registry: RwLock<Registry>,
    pool: SqlitePool,
    backend: Backend,
    rng: RwLock<u64>,
}

impl Store {
    /// 连接一个 SQLite 数据库（`sqlite://path.db` 或 `sqlite::memory:`）
    pub async fn connect_sqlite(url: &str) -> Result<Self, String> {
        let opts: SqliteConnectOptions = url
            .parse()
            .map_err(|e| format!("SQLite 连接串非法: {e}"))?;
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await
            .map_err(|e| format!("SQLite 连接失败: {e}"))?;
        Ok(Store {
            registry: RwLock::new(Registry::new()),
            pool,
            backend: Backend::Sqlite,
            rng: RwLock::new(now_ms() as u64 ^ 0x9E3779B97F4A7C15),
        })
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

    /// 底层连接池（DDL / 维护脚本逃生口；日常读写走 Store API）
    pub fn sqlite_pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn list_schemas(&self) -> Vec<String> {
        exec::with_registry(&self.registry, |reg| Ok(reg.list())).unwrap_or_default()
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
        let outcome = exec::exec_translated(&mut conn, &translated).await?;

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
            let o1 = exec::exec_translated(&mut conn, &t1).await?;
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
        let outcome = exec::exec_translated(&mut conn, &translated).await?;
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
        let outcome = exec::exec_translated(&mut conn, &translated).await?;
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
        let command = self.plan_update_with_probe(schema_name, condition, data, ctx, now).await?;
        let mut conn = self.pool.acquire().await.map_err(|e| e.to_string())?;
        let translated = exec::translate_command(self.backend, &command, &self.registry)?;
        let outcome = exec::exec_translated(&mut conn, &translated).await?;
        drop(conn);
        Ok(outcome.docs.into_iter().next())
    }

    /// 删除（归档编排：find 源文档 → plan_archive_docs 写归档表 → deleteMany；
    /// 关系谓词条件先执行 preCommand 取命中 `_id`。返回 `{deletedCount, archivedCount}`）
    pub async fn remove(
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
            let outcome = exec::exec_translated(&mut conn, &translated).await?;
            drop(conn);
            let ids = exec::extract_ids(&outcome.docs);
            if let Some(obj) = delete_command.as_object_mut() {
                if let Some(filter) = obj.get_mut("filter") {
                    exec::substitute_ids(filter, REL_PRED_IDS, &ids);
                }
            }
        }

        // 归档 + 删除编排：同一事务内顺序执行（对齐 nodejs write.js:167-168 runAtomic）——
        // 归档落库与源删除原子生效，任一步失败整体回滚（禁「已删未归档」的静默失守）
        let mut conn = self.pool.acquire().await.map_err(|e| e.to_string())?;
        let mut tx = conn.begin().await.map_err(|e| format!("开启事务失败: {e}"))?;
        let mut archived_count: u64 = 0;

        let find_command = plan.get("findCommand").cloned().filter(|v| !v.is_null());
        if let Some(find_cmd) = find_command {
            let translated = exec::translate_command(self.backend, &find_cmd, &self.registry)?;
            let outcome = exec::exec_translated(&mut tx, &translated).await?;
            if !outcome.docs.is_empty() {
                let archive_plan = {
                    let reg = self.read_reg()?;
                    plan_archive_docs(schema_name, &reg, &outcome.docs, id::now())?
                };
                if let Some(arch_cmd) = archive_plan.get("command").cloned() {
                    let translated = exec::translate_command(self.backend, &arch_cmd, &self.registry)?;
                    let arch = exec::exec_translated(&mut tx, &translated).await?;
                    archived_count = arch.changes;
                }
            }
        }

        let translated = exec::translate_command(self.backend, &delete_command, &self.registry)?;
        let outcome = exec::exec_translated(&mut tx, &translated).await?;
        let deleted_count = outcome.changes;
        tx.commit().await.map_err(|e| format!("事务提交失败: {e}"))?;
        drop(conn);

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
        let mut rng = self
            .rng
            .write()
            .map_err(|_| "rng 锁中毒".to_string())?;
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
            plan_update(schema_name, &reg, ctx, condition, data, &json!({}), now, Probe::NotProbed)?
        };
        match first.get("needsProbe").cloned() {
            Some(probe_cmd) => {
                let found = self.run_probe(&probe_cmd).await?;
                let reg = self.read_reg()?;
                match &found {
                    Some(doc) => {
                        plan_update(schema_name, &reg, ctx, condition, data, &json!({}), now, Probe::Found(doc))
                    }
                    None => plan_update(schema_name, &reg, ctx, condition, data, &json!({}), now, Probe::NoResult),
                }
            }
            None => Ok(first.get("command").cloned().ok_or_else(|| "plan_update 缺少 command".to_string())?),
        }
    }

    /// 执行探针命令（cmd_find_one）取条件命中文档
    async fn run_probe(&self, probe_cmd: &Value) -> Result<Option<Value>, String> {
        let translated = exec::translate_command(self.backend, probe_cmd, &self.registry)?;
        let mut conn = self.pool.acquire().await.map_err(|e| e.to_string())?;
        let outcome = exec::exec_translated(&mut conn, &translated).await?;
        drop(conn);
        Ok(outcome.docs.into_iter().next())
    }
}
