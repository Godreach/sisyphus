//! 版本与兼容窗口（ADR-0010/0017）。
//!
//! Server 与 Agent 同版本成对发布（semver），兼容窗口 N-1。版本比较逻辑放
//! 在唯一共享 crate，两端复用，避免 Server/Agent 各自实现漂移。

use crate::agent::Version;

/// 本发行版本（Server 与 Agent 同版本成对发布，ADR-0010）。
pub const VERSION: Version = Version {
    major: 1,
    minor: 0,
    patch: 0,
};

/// 兼容窗口：对端不高于本地（全序比较 major/minor/patch）即视为窗口内。
/// 过新（对端任意段大于本地）判定为不兼容，直接拒连（ADR-0017）。
pub fn compatible(peer: &Version, local: &Version) -> bool {
    !peer_too_new(peer, local)
}

/// 对端是否过新（semver 全序：major → minor → patch，任一大于即过新）。
pub fn peer_too_new(peer: &Version, local: &Version) -> bool {
    (peer.major, peer.minor, peer.patch) > (local.major, local.minor, local.patch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_version_compatible() {
        assert!(compatible(&VERSION, &VERSION));
    }

    #[test]
    fn older_peer_compatible() {
        // N-1 兼容窗口：旧 Agent 可连（任务面细化归后续批次）。
        let older = Version {
            major: 0,
            minor: 9,
            patch: 0,
        };
        assert!(compatible(&older, &VERSION));
    }

    #[test]
    fn newer_peer_incompatible() {
        let newer = Version {
            major: 2,
            minor: 0,
            patch: 0,
        };
        assert!(!compatible(&newer, &VERSION));
        assert!(peer_too_new(&newer, &VERSION));
        // minor/patch 任一更大也判过新（semver 全序）。
        assert!(peer_too_new(
            &Version {
                major: 1,
                minor: 1,
                patch: 0
            },
            &VERSION
        ));
        assert!(peer_too_new(
            &Version {
                major: 1,
                minor: 0,
                patch: 1
            },
            &VERSION
        ));
    }
}
