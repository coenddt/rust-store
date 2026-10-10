//! local：本地磁盘数据源纯求值转发（宿主提供集合快照 → 返回结果与变更后快照）。

use napi::Result;
use napi_derive::napi;
use serde_json::Value;

use crate::convert::err;
use crate::Registry;

#[napi]
impl Registry {
    /// 本地磁盘数据源纯求值（宿主提供集合快照 → 返回 `{result, changed, collections}`）
    #[napi]
    pub fn local_eval(&self, collections: Value, command: Value) -> Result<Value> {
        rust_store_core::local::eval_command(&collections, &command).map_err(err)
    }
}
