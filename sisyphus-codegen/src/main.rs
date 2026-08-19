//! sisyphus-codegen：sisyphus-model → TS 类型/校验生成 + 前端对账（ADR-0009，票 B4-T7）。
//!
//! - `cargo run -p sisyphus-codegen -- gen`：生成 4 文件到 `sisyphus-web/src/model/`。
//! - `cargo run -p sisyphus-codegen -- check`：内存重生与盘上 diff，漂移/手改即 exit 1（CI 挂）。
//!
//! 自检 `#[test]`（`cargo test -p sisyphus-codegen`）跑 `validate()` 断言每样本的 Rust 标记码 ==
//! 声明码——抓 model 漂移与脏样本，无需 gen 即可由 `cargo test --workspace` 兜底。

mod codegen;
mod samples;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// 生成产物落点：`sisyphus-web/src/model/`（相对 sisyphus-codegen crate 目录）。
fn out_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("sisyphus-web")
        .join("src")
        .join("model")
}

fn main() -> ExitCode {
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        Some("gen") => generate(),
        Some("check") => check(),
        _ => {
            eprintln!("usage: cargo run -p sisyphus-codegen -- <gen|check>");
            ExitCode::from(2)
        }
    }
}

/// 生成 4 文件到 `sisyphus-web/src/model/`。
fn generate() -> ExitCode {
    let dir = out_dir();
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("无法创建输出目录 {dir_display}: {e}", dir_display = dir.display());
        return ExitCode::from(1);
    }
    for (name, content) in codegen::generated() {
        let path = dir.join(name);
        if let Err(e) = fs::write(&path, content) {
            eprintln!("无法写入 {path_display}: {e}", path_display = path.display());
            return ExitCode::from(1);
        }
        println!("  生成 {name}");
    }
    println!("完成：4 文件已写入 {}", dir.display());
    ExitCode::SUCCESS
}

/// 内存重生并与盘上逐文件 diff，任一不一致即 exit 1。
fn check() -> ExitCode {
    let dir = out_dir();
    let mut failed = false;
    for (name, expected) in codegen::generated() {
        let path = dir.join(name);
        let actual = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "漂移：{name} 读失败（{e}）——可能未生成（跑 `cargo run -p sisyphus-codegen -- gen`）"
                );
                failed = true;
                continue;
            }
        };
        if actual != expected {
            eprintln!(
                "漂移：{name} 与生成产物不一致（model 改后未 gen 或被手改）——跑 `cargo run -p sisyphus-codegen -- gen`"
            );
            failed = true;
        }
    }
    if failed {
        ExitCode::from(1)
    } else {
        println!("sisyphus-codegen check 通过：4 文件与生成产物一致");
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sisyphus_model::validate::{validate, ValidationCode};

    /// 把规则码切片按 snake_case 字符串排序为 `Vec<String>`（multiset 比较用）。
    fn sorted_codes(codes: &[ValidationCode]) -> Vec<String> {
        let mut s: Vec<String> = codes.iter().map(|c| serde_json::to_string(c).unwrap()).collect();
        s.sort();
        s
    }

    /// 自检：每样本跑 `validate()`，断言 Rust 标记的码 multiset == 声明码。
    /// 抓 model 漂移（某规则开始/停止在某样本触发）与脏样本（本应单规则却共触发）。
    #[test]
    fn rust_validate_matches_declared_codes() {
        for s in samples::samples() {
            let actual: Vec<ValidationCode> = match validate(&s.pipeline) {
                Ok(()) => Vec::new(),
                Err(errs) => errs.iter().map(|e| e.code).collect(),
            };
            let actual_sorted = sorted_codes(&actual);
            let expected_sorted = sorted_codes(s.expected_codes);
            assert_eq!(
                actual_sorted, expected_sorted,
                "样本 `{id}`：Rust 标记码 {actual_sorted:?} != 声明码 {expected_sorted:?}",
                id = s.id
            );
            // valid 与「无错」一致。
            assert_eq!(s.valid, actual.is_empty(), "样本 `{}`：valid 标记与校验结果矛盾", s.id);
        }
    }

    /// 防漏同步：每条规则码至少被一个样本覆盖。
    #[test]
    fn every_code_covered_by_a_sample() {
        for code in samples::ALL_CODES {
            let covered = samples::samples()
                .iter()
                .any(|s| s.expected_codes.contains(&code));
            assert!(covered, "规则码 {code:?} 无样本覆盖——补一条破坏样本");
        }
    }

    /// `ALL_CODES` 与 model `ValidationCode` 变体数一致（防 model 加规则后漏登记）。
    #[test]
    fn all_codes_count_matches_enum() {
        // 14 条规则——若 model 加规则，此处需同步更新 `ALL_CODES`。
        assert_eq!(samples::ALL_CODES.len(), 14);
    }
}
