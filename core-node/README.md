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

## 本地构建（开发）

产物在发布期由 `napi build --platform` 生成平台包；开发期可用 cargo 手工构建：

```powershell
cargo build --manifest-path core-node/Cargo.toml
Copy-Item core-node/target/debug/rust_store_node.dll core-node/dist/rust-store-node.node -Force
```

`dist/rust-store-node.node` 仅供本仓库测试与相邻 Host 调试使用，**不会**随 npm 包发布。

## License

[MIT](LICENSE)
