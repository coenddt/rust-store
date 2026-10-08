//! 本地磁盘文档数据源 —— 纯逻辑求值器（无 IO / 无时钟 / 无随机）。
//!
//! 语义基准 = MongoDB 驱动语义（local 与 mongo 走同一条命令路径，必须同结果）。
//! 子模块随步骤 2~5 逐步接入：value / filter / pipeline / update / eval；
//! 最终模块形态见执行文档 §4.1。

pub mod value;
