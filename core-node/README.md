# rust-store-node

`rust-store` 的 **Node 绑定（napi-rs）**：把 Rust core 的 Command 契约暴露给 JS Host。

- 语言无关核心见 `../core`（GQL / 权限 / 计算列 / 命令规划 / SQL 方言翻译，纯逻辑无 IO）。
- 绑定层只做「JSON in → JSON out」的类型转换与转发，不持有数据库驱动，不执行任何 IO。
- Python 侧同构绑定见 `../core-py`（`rust-store-py`）。

## 安装

```bash
npm i rust-store-node
```

平台原生二进制按 `optionalDependencies` 分包发布，安装后自动匹配当前平台。

## 使用

```js
const { Registry } = require('rust-store-node');

const reg = new Registry();
reg.register({ name: 'User', collection: 'users', fields: { name: { type: 'string' } }, relations: {} });

const plan = reg.planQuery('User{name}', {}, null); // → Mongo 命令 JSON
```

同步计算列回调经 `setFn` 注册；异步计算列（`asyncFn`）由 Host 走 `prepareQuery` / `stripQuery` 两段式执行。

## 命名规范与翻译摘要

归一实现唯一在 `core::naming`，经 `core-node` / `core-py` 透出，宿主不得重复实现。本语言（Node）归一匹配示例：实现 `orderTotal` ↔ 逻辑 `Order.total`。

<!-- SPEC:NAMING-STYLE:BEGIN -->
### Naming: freeform definitions, system-directed translation

Definitions (`collection`, fields, referenced relation fields, computed-column keys, `fnRef` values, index names) may use any style; the engine translates them to the target style. Contract keys (`fnRef`, `localField`, `foreignField`, `asyncFn`, `type`, ...) and the schema `name` are never translated.

| Target | Style | Example (`orderTotal`) |
|---|---|---|
| MySQL / PostgreSQL / SQLite (physical) | snake_case | `order_total` |
| MongoDB (physical) | camelCase | `orderTotal` |
| Node.js / Java / C# / Rust (code; computed columns follow) | camelCase | `orderTotal` |
| Go (code; computed columns follow) | PascalCase (must be exported) | `OrderTotal` |
| Python (code; computed columns follow) | snake_case | `order_total` |

Canonicalization (single implementation `core::naming`, re-exported by the bindings; hosts must not re-implement it): split on `_`, `-`, `.`, space and at lower/digit-to-upper boundaries; a trailing uppercase in a run followed by a lowercase starts the next token (`HTTPServer` -> `[http, server]`, `userID` -> `[user, id]`); digits stay inside a token (`order2Items` -> `[order2, items]`). Reassembly: snake = `t1_t2`, camel = `t1T2`, pascal = `T1T2`.

Two logical names in one schema that canonicalize equal (`orderTotal` vs `order_total`), or a name that canonicalizes onto a reserved contract key (e.g. `fnref`), is an error `ERR_NAME_CONFLICT:` and the service does not start (never silently overwritten).
<!-- SPEC:NAMING-STYLE:END -->

<!-- SPEC:FNREF:BEGIN -->
### Computed columns: `fnRef` binding by composite name + canonical match

Computed columns live at the schema top level, `computes: { <key>: { type, fn | asyncFn | agg, fnRef?, depends?, read? } }` (`fn` / `asyncFn` / `agg` are mutually exclusive).

- The logical `fnRef` defaults to `<schema.name>.<computed-column key>` (generated, never hand-written); since `name` is globally unique, the `fnRef` is globally unique too.
- Host implementations bind by canonicalization: both the implementation's name in the host language style and the schema's logical `fnRef` are canonicalized to token sequences and compared. So Node's `orderAmountLabel` and Python's `order_amount_label` bind to the same logical computed column.
- Reusing one implementation across schemas: write an explicit shared name (e.g. `"fnRef": "common.moneyLabel"`); naming goes from required to optional.
- Every declared `fnRef` must have an implementation, otherwise the service fails to start with `ERR_FN_MISSING`.
<!-- SPEC:FNREF:END -->

## 本地构建（开发）

产物在发布期由 `napi build --platform` 生成平台包；开发期可用 cargo 手工构建：

```powershell
cargo build --manifest-path core-node/Cargo.toml
Copy-Item core-node/target/debug/rust_store_node.dll core-node/dist/rust-store-node.node -Force
```

`dist/rust-store-node.node` 仅供本仓库测试与相邻 Host 调试使用，**不会**随 npm 包发布。

## License

[MIT](LICENSE)
