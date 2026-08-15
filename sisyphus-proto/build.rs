//! sisyphus-proto 构建脚本：用 vendored protoc 现场生成 tonic/prost 代码。

use std::path::PathBuf;

fn main() {
    let proto_dir = PathBuf::from("proto");
    println!("cargo:rerun-if-changed={}", proto_dir.display());

    // prost-build 0.14 经 PROTOC 环境变量定位 protoc（见其 lib.rs 文档）。
    // 用 vendored protoc 保证构建链无系统 protoc 前置（ADR-0009）。
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
    // build.rs 在单线程环境执行，此处 set_var 无并发风险（1.97 起标记为 unsafe，
    // 经 allow 豁免 workspace 的 unsafe_code=warn）。
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("PROTOC", protoc);
    }

    let proto_files = std::fs::read_dir(&proto_dir)
        .expect("proto dir")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "proto"))
        .collect::<Vec<_>>();

    let mut prost_config = prost_build::Config::new();
    // 大 oneof 变体（JobSpec 超 300 字节）boxed，避免 clippy large_enum_variant
    // 对生成代码报警。oneof 字段路径：消息.oneof名.字段名。
    prost_config.boxed(".sisyphus.v1.ChannelMessage.kind.job_spec");
    // 生成的消息类型豁免 missing_docs（生成物文档来自 .proto 注释，不手改）。
    prost_config.type_attribute(".", "#[allow(missing_docs)]");

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .build_transport(false) // 关闭 connect(dst) 便利方法，避免与 RPC 方法名碰撞
        // 多路复用通道的 oneof 变体尺寸差异是设计使然（生成物不手改）。
        .enum_attribute(
            "sisyphus.v1.ChannelMessage.Kind",
            "#[allow(clippy::large_enum_variant)]",
        )
        .compile_with_config(prost_config, &proto_files, std::slice::from_ref(&proto_dir))
        .expect("compile protos");
}
