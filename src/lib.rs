// 已知 clippy 误报:中文/日文 doc 注释里的连续行被 `doc_lazy_continuation` /
// `doc_markdown` 误判为未缩进的 markdown 列表项或未转义的内联代码。这些注释是给人读的
// 散文,不是 markdown 渲染输入,逐一改写会损害可读性。在 crate 级统一豁免。
#![allow(clippy::doc_lazy_continuation)]
#![allow(clippy::doc_markdown)]

pub mod agent;
pub mod cli;
pub mod common;
pub mod context;
pub mod mcp;
pub mod planning;
pub mod tools;
