//! OpenAPI snapshot 守护（ADR-0005）：生成 OpenAPI JSON 与入库快照比对，
//! 防 utoipa 注解悄悄漂移——端点/形态变更必须显式重写快照并随代码评审。
//! 重写方式：`UPDATE_SNAPSHOTS=1 cargo test -p sisyphus-server`（ADR-0009：
//! OpenAPI snapshot 就近落 server 集成测试，不经 sisyphus-codegen，环境变量控制）。

use sisyphus_server::api::ApiDoc;
use utoipa::OpenApi;

/// 快照落位（随 git 提交）。
const SNAPSHOT_PATH: &str = "tests/snapshots/openapi.json";

#[test]
fn openapi_json_matches_committed_snapshot() {
    let json = ApiDoc::openapi()
        .to_pretty_json()
        .expect("生成 OpenAPI JSON");
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(SNAPSHOT_PATH);

    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("建快照目录");
        }
        std::fs::write(&path, &json).expect("重写快照文件");
        eprintln!(
            "已重写 OpenAPI 快照：{}（请随代码一并提交评审）",
            path.display()
        );
        return;
    }

    let snapshot = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "快照缺失：UPDATE_SNAPSHOTS=1 cargo test -p sisyphus-server 生成并提交 {}",
            path.display()
        )
    });
    assert_eq!(
        json, snapshot,
        "OpenAPI 与入库快照不一致——若变更是有意的，先 UPDATE_SNAPSHOTS=1 重写、再随代码评审 diff"
    );
}
