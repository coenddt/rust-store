//! 向量能力矩阵（纯函数，零 IO）：宿主探测环境后喂入，判定唯一在本模块。

pub const VECTOR_METRICS: [&str; 3] = ["cosine", "l2", "ip"];

/// 应用层（JSON/TEXT 存储 + 精确打分）的统一维度上限。
/// 数值源唯一：引用 01 在 `schema::definition` 定义的 `MAX_VECTOR_DIM`（16000，与 pgvector `vector` 类型上限同值）。
/// 禁止在 core 内出现第二个 16000 字面量。
pub const APPLICATION_MAX_DIM: u32 = crate::schema::MAX_VECTOR_DIM;
/// pgvector 近似索引（HNSW / IVFFlat）的维度上限。
/// 与 02 宿主侧 `ddl.js` / `ddl.py` 的 `PGVECTOR_INDEX_MAX_DIM` 同值（parity 断言两端相等）。
/// 本文件只把它用作 `capabilities().vector.maxDim` 的输出值；DDL 期拦截由 02 的 `vectorIndexStmt` 负责。
pub const PGVECTOR_INDEX_MAX_DIM: u32 = 2000;
/// MongoDB Atlas `vectorSearch` 索引的维度上限（V-09 待外部环境复核）。
pub const ATLAS_INDEX_MAX_DIM: u32 = 4096;

/// 环境探测结果（宿主填写；core 不探测）
pub struct VectorEnv<'a> {
    pub backend: &'a str,
    /// postgres: `pgvector` 扩展已安装；sqlite: `vec0` 已装载（本轮恒 false）
    pub native_ready: bool,
    /// mongodb: 该 source 下至少一个 schema 的索引项声明了 `options.indexName`
    pub atlas_index: bool,
}

/// 返回 `{ supported, native, backend, metrics, maxDim }`（字段名固定，宿主原样透出）
pub fn vector_capability(env: VectorEnv<'_>) -> serde_json::Value {
    let (supported, native, backend, metrics, max_dim) = match env.backend {
        "postgres" => {
            if env.native_ready {
                (true, true, "pgvector", &VECTOR_METRICS[..], PGVECTOR_INDEX_MAX_DIM)
            } else {
                (false, false, "pgvector", &[][..], 0)
            }
        }
        "mongodb" => {
            if env.atlas_index {
                (true, true, "atlas", &VECTOR_METRICS[..], ATLAS_INDEX_MAX_DIM)
            } else {
                (true, false, "mongo", &VECTOR_METRICS[..], APPLICATION_MAX_DIM)
            }
        }
        "mysql" => (true, false, "mysql", &VECTOR_METRICS[..], APPLICATION_MAX_DIM),
        "sqlite" => (true, false, "sqlite", &VECTOR_METRICS[..], APPLICATION_MAX_DIM),
        "local" => (true, false, "local", &VECTOR_METRICS[..], APPLICATION_MAX_DIM),
        "memory" => (true, false, "memory", &VECTOR_METRICS[..], APPLICATION_MAX_DIM),
        _ => (false, false, "unknown", &[][..], 0),
    };
    serde_json::json!({
        "supported": supported,
        "native": native,
        "backend": backend,
        "metrics": metrics,
        "maxDim": max_dim,
    })
}
