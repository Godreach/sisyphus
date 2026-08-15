//! sisyphus-proto：Agent/Server 唯一共享契约（ADR-0007）。
//!
//! `.proto` 源文件位于 `proto/`，本 crate 的 `build.rs` 用 vendored protoc
//! 现场生成 tonic/prost 代码（生成物不进 git）。

pub mod agent {
    //! 契约消息与 `AgentChannel` service（ADR-0007）。

    include!(concat!(env!("OUT_DIR"), "/sisyphus.v1.rs"));
}

pub mod version;
