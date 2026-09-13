//! 数据源（DataSource）—— 多后端路由的纯逻辑
//!
//! 一个 schema 绑定到一个「数据源」（source 名）；数据源配置把 source 名解析为
//! [`DataSource`]（Mongo 原生 / 某个 SQL 后端）。本模块**只做解析**：不持有连接、
//! 不做任何 IO（连接句柄留在 Host，参见铁律 1）。
//!
//! 配置 JSON 形状（Host 只把 kind 传进来，连接句柄不出 Host）：
//!
//! ```json
//! { "sources": { "default": "mongo", "mysql1": "mysql", "pg1": "postgres" } }
//! ```
//!
//! schema 定义里的 `datasource` 字段是 **source 名**（可选）：
//! - 缺省 → 名为 [`DEFAULT_SOURCE`] 的数据源；
//! - 若配置未声明 `default` → 回落到单源 Mongo（保证既有调用方零变更）。

use serde_json::Value;

use crate::dialect::Backend;

/// schema 未显式声明 `datasource` 时使用的 source 名
pub const DEFAULT_SOURCE: &str = "default";

/// 一个 schema 的底层数据源
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataSource {
    /// MongoDB —— 走 Mongo 原生驱动路径
    Mongo,
    /// 关系型 SQL 后端 —— 命令经 dialect 翻译后执行
    Sql(Backend),
}

impl DataSource {
    /// 从数据源 kind 字符串解析（未知 → Err）
    pub fn from_kind(kind: &str) -> Result<DataSource, String> {
        match kind.to_lowercase().as_str() {
            "mongo" | "mongodb" => Ok(DataSource::Mongo),
            other => Backend::parse(other).map(DataSource::Sql),
        }
    }

    /// 数据源 kind（与配置里的字符串一致，供绑定层序列化）
    pub fn as_str(&self) -> &'static str {
        match self {
            DataSource::Mongo => "mongo",
            DataSource::Sql(b) => b.as_str(),
        }
    }

    pub fn is_mongo(&self) -> bool {
        matches!(self, DataSource::Mongo)
    }

    /// SQL 后端（Mongo 时为 `None`）
    pub fn backend(&self) -> Option<Backend> {
        match self {
            DataSource::Sql(b) => Some(*b),
            DataSource::Mongo => None,
        }
    }
}

/// 数据源配置：source 名 → [`DataSource`]
#[derive(Debug, Clone, Default)]
pub struct DataSourceConfig {
    /// 保持声明顺序（便于确定性输出与对拍）
    sources: Vec<(String, DataSource)>,
}

impl DataSourceConfig {
    pub fn new() -> Self {
        Self::default()
    }

    /// 从 JSON 解析：`{ "sources": { "<name>": "<kind>" } }`
    ///
    /// `null` / 无 `sources` → 空配置（等价单源 Mongo）。
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let mut cfg = Self::new();
        let Some(obj) = v.as_object() else {
            return Ok(cfg);
        };
        if let Some(Value::Object(sources)) = obj.get("sources") {
            for (name, kind) in sources {
                let kind = kind
                    .as_str()
                    .ok_or_else(|| format!("数据源 {} 的 kind 必须是字符串", name))?;
                cfg.register(name, DataSource::from_kind(kind)?);
            }
        }
        Ok(cfg)
    }

    /// 注册 / 覆盖一个数据源
    pub fn register(&mut self, name: &str, source: DataSource) {
        if let Some(slot) = self.sources.iter_mut().find(|(n, _)| n == name) {
            slot.1 = source;
        } else {
            self.sources.push((name.to_string(), source));
        }
    }

    pub fn get(&self, name: &str) -> Option<DataSource> {
        self.sources
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, s)| *s)
    }

    /// 解析 schema 声明的 source 名 → [`DataSource`]
    ///
    /// - `None`（schema 未声明）→ 名为 [`DEFAULT_SOURCE`] 的数据源；配置未声明则回落 Mongo
    /// - `Some(name)` → 必须已在配置中声明，否则 Err（避免静默走错库）
    pub fn resolve(&self, source: Option<&str>) -> Result<DataSource, String> {
        match source {
            None => Ok(self.get(DEFAULT_SOURCE).unwrap_or(DataSource::Mongo)),
            Some(name) => self
                .get(name)
                .ok_or_else(|| format!("schema 绑定的数据源未在配置中声明: {}", name)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_and_resolve() {
        let cfg = DataSourceConfig::from_json(&json!({
            "sources": { "default": "mongo", "mysql1": "mysql", "pg1": "postgres", "lite": "sqlite" }
        }))
        .unwrap();

        assert_eq!(cfg.resolve(None).unwrap(), DataSource::Mongo);
        assert_eq!(
            cfg.resolve(Some("mysql1")).unwrap(),
            DataSource::Sql(Backend::Mysql)
        );
        assert_eq!(
            cfg.resolve(Some("pg1")).unwrap(),
            DataSource::Sql(Backend::Postgres)
        );
        assert_eq!(
            cfg.resolve(Some("lite")).unwrap(),
            DataSource::Sql(Backend::Sqlite)
        );
    }

    #[test]
    fn empty_config_defaults_to_mongo() {
        let cfg = DataSourceConfig::from_json(&Value::Null).unwrap();
        assert_eq!(cfg.resolve(None).unwrap(), DataSource::Mongo);
        // 未声明 default 时，显式 source 名仍报错
        assert!(cfg.resolve(Some("missing")).is_err());
    }

    #[test]
    fn unknown_kind_and_source_error() {
        assert!(DataSourceConfig::from_json(&json!({ "sources": { "x": "oracle" } })).is_err());
        let cfg =
            DataSourceConfig::from_json(&json!({ "sources": { "default": "mongo" } })).unwrap();
        assert!(cfg.resolve(Some("nope")).is_err());
    }

    #[test]
    fn kind_roundtrip() {
        assert_eq!(DataSource::from_kind("mongodb").unwrap().as_str(), "mongo");
        assert_eq!(DataSource::from_kind("MySQL").unwrap().as_str(), "mysql");
        assert_eq!(
            DataSource::from_kind("pg").unwrap().backend(),
            Some(Backend::Postgres)
        );
        assert!(DataSource::Mongo.is_mongo());
    }
}
