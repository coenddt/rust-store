# rust-store —— Rust 单核心 + 双绑定（多后端数据引擎）

rust-store 是 `nodejs-store` / `py-store` 共享的 **Rust 核心引擎**：schema/GQL/权限/计算列/命令规划全部在 Rust core 实现，通过 napi-rs 与 PyO3 绑定分别服务 Node.js 与 Python，保证双端语义天然一致。

## workspace 成员

| 目录 | 说明 |
| --- | --- |
| `core/` | `rust-store-core` —— 语言无关核心（GQL/权限/计算列/命令规划，纯逻辑无 IO） |
| `core-node/` | `rust-store-node` —— Node 绑定（napi-rs，产出 `dist/rust-store-node.node`） |
| `core-py/` | `rust-store-py` —— Python 绑定（PyO3，产出 `dist/rust_store_py.pyd`） |

workspace 统一使用根目录 `target/` 与根 `Cargo.lock`。

## 支持后端

core 按约定产出 **Mongo 命令 JSON**（`find`/`aggregate`/`countDocuments`/…），再由 `core/src/dialect` 纯函数翻译通道落成各后端原生查询：

| 后端 | 说明 |
| --- | --- |
| MongoDB | 原生聚合管道（`$lookup` / aggregation） |
| MySQL | 参数化 SQL，`information_schema` 建/查表感知 |
| SQLite | 参数化 SQL（`?`），`sqlite_master` + `PRAGMA` introspection |
| PostgreSQL | 参数化 SQL（`$n`），`RETURNING` 写后回读 |

翻译层把驱动返回的**平铺 JOIN 行**还原为嵌套 Mongo 文档（见 `dialect::row`），对外保持「用 MongoDB 方言，其他数据库适配」。

## 绑定产物对接

Node/Python 的薄 Host 适配层（`src/nodejs-store`、`py-store`）通过各绑定产物引用本引擎：

- Node（Windows）：`cargo build --manifest-path core-node/Cargo.toml`，再把
  `core-node/target/debug/rust_store_node.dll` 复制为 `core-node/dist/rust-store-node.node`；
- Python：`cargo build --manifest-path core-py/Cargo.toml`，产出 `core-py/dist/rust_store_py.pyd`。

## 对拍测试

`core/tests/parity*.rs` 与 `core-node/test/parity.test.js` 覆盖 schema/pipeline/permission/computes 在 Rust core 与绑定层的一致性。

## License

[MIT](LICENSE)