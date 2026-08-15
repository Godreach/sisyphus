//! sisyphus-model：Pipeline 定义数据模型（ADR-0006）。
//!
//! 纯类型与纯逻辑叶子 crate：Pipeline 三级结构、when 表达式 AST 与求值、
//! `${}` 变量解析、保存校验规则。作为编辑器保存校验、构建快照存储与
//! 未来 TS 类型生成的单一事实源（ADR-0009）。

pub mod pipeline;
pub mod validate;
pub mod variables;
pub mod when;
