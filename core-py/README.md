# rust-store-py

`rust-store` 的 **Python 绑定（PyO3）**：把 Rust core 的 Command 契约暴露给 Python Host。

- 语言无关核心见 `../core`（GQL / 权限 / 计算列 / 命令规划 / SQL 方言翻译，纯逻辑无 IO）。
- 绑定层只做「dict in → dict out」的类型转换与转发，不持有数据库驱动，不执行任何 IO。
- Node 侧同构绑定见 `../core-node`（`rust-store-node`）。

## 安装

```bash
pip install rust-store-py
```

## 使用

```python
from rust_store_py import Registry

reg = Registry()
reg.register({"name": "User", "collection": "users", "fields": {"name": {"type": "string"}}, "relations": {}})

plan = reg.plan_query("User{name}", {}, None)  # → Mongo 命令 JSON（dict）
```

同步计算列回调经 `set_fn` 注册；异步计算列（`asyncFn`）由 Host 走 `prepare_query` / `strip_query` 两段式执行。
错误一律以异常抛出（`PyErr`），不会以返回值形式混进结果 dict。

## 构建（开发）

```powershell
cargo build --manifest-path core-py/Cargo.toml
Copy-Item core-py/target/debug/rust_store_py.dll core-py/dist/rust_store_py.pyd -Force
```

发布期用 maturin 构建 wheel（产物名为 `rust_store_py-*.whl`）：

```bash
maturin build --manifest-path core-py/Cargo.toml --release
```

> PyO3 未开启 `abi3`，wheel 与 Python 版本一一对应（逐版本构建）。

## License

[MIT](LICENSE)
