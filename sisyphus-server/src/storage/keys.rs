//! 对象键策略（票 #122，ADR-0026）：产物/日志互不重叠，临时与最终再分开。

/// 对象类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectClass {
    /// 构建产物。
    Artifacts,
    /// 日志归档。
    Logs,
}

impl ObjectClass {
    fn dir(self) -> &'static str {
        match self {
            Self::Artifacts => "artifacts",
            Self::Logs => "logs",
        }
    }
}

/// 对象生命周期阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectPhase {
    /// 临时对象（可签发写权限）。
    Temporary,
    /// 最终对象（从不签发 PUT）。
    Final,
}

impl ObjectPhase {
    fn dir(self) -> &'static str {
        match self {
            Self::Temporary => "tmp",
            Self::Final => "final",
        }
    }
}

/// 拼接根前缀与后续段（前缀可空；段内不应再含首尾 `/`）。
pub fn join_key(prefix: &str, parts: &[&str]) -> String {
    let mut segs: Vec<&str> = Vec::new();
    let prefix = prefix.trim().trim_matches('/');
    if !prefix.is_empty() {
        segs.push(prefix);
    }
    segs.extend(parts.iter().copied().filter(|p| !p.is_empty()));
    segs.join("/")
}

/// 产物/日志对象键。
pub fn object_key(prefix: &str, class: ObjectClass, phase: ObjectPhase, name: &str) -> String {
    join_key(prefix, &[class.dir(), phase.dir(), name])
}

/// 连接自检探针对象键（不与产物/日志前缀重叠）。
pub fn probe_key(prefix: &str, probe_id: &str, name: &str) -> String {
    join_key(prefix, &[".sisyphus-probe", probe_id, name])
}

/// 单文件产物在临时/最终前缀下的对象名（构建/任务/attempt/产物名）。
pub fn artifact_blob_name(build_id: i64, job_id: i64, attempt: i32, name: &str) -> String {
    format!("{build_id}/{job_id}/{attempt}/{name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_keys_separate_class_and_phase() {
        assert_eq!(
            object_key("", ObjectClass::Artifacts, ObjectPhase::Temporary, "a/b"),
            "artifacts/tmp/a/b"
        );
        assert_eq!(
            object_key("prod", ObjectClass::Artifacts, ObjectPhase::Final, "a"),
            "prod/artifacts/final/a"
        );
        assert_eq!(
            object_key("/prod/", ObjectClass::Logs, ObjectPhase::Temporary, "x"),
            "prod/logs/tmp/x"
        );
        assert_eq!(
            probe_key("prod", "p1", "blob"),
            "prod/.sisyphus-probe/p1/blob"
        );
        assert!(
            !probe_key("prod", "p1", "blob").starts_with("prod/artifacts/")
                && !probe_key("prod", "p1", "blob").starts_with("prod/logs/")
        );
        assert_eq!(artifact_blob_name(7, 3, 1, "dist.bin"), "7/3/1/dist.bin");
    }
}
