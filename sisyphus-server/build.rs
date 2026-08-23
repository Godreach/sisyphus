//! rust-embed 内嵌目录的存在性保证（api/web.rs 的 `#[folder = "../sisyphus-web/dist/"]`）。
//!
//! 目录原先以 git 跟踪的 `dist/.gitkeep` 占位，但 `vite build` 的 emptyOutDir
//! 豁免名单只有 `.git`——每次前端构建都会把 `.gitkeep` 删掉，git 状态反复出现
//! 误删除。现改由本脚本编译前兜底创建目录，git 不再跟踪 dist 内任何内容。

use std::{env, fs, path::PathBuf};

fn main() {
    let dist = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("../sisyphus-web/dist");

    if !dist.is_dir() {
        fs::create_dir_all(&dist).expect("创建 sisyphus-web/dist 失败");
    }

    // dist 内容变化 → 本脚本重跑 → server 重编译：release 下 rust-embed 在编译期
    // 嵌入（ADR-0005），cargo 不感知 dist 内容，不声明则改完前端重编 server 仍嵌
    // 旧产物。声明后取代「包内任意文件变化即重跑」的默认规则。
    println!("cargo:rerun-if-changed={}", dist.display());

    // release 空嵌入守卫：debug 构建运行时读盘、前端可后补；release 是编译期嵌入，
    // 空 dist 意味着发布了一个没有前端的 server，静默失败不可接受。
    if env::var("PROFILE").as_deref() == Ok("release") && !dist.join("index.html").is_file() {
        println!(
            "cargo:warning=sisyphus-web/dist 无 index.html：release 构建将不内嵌前端，先在 sisyphus-web 执行 npm run build"
        );
    }
}
