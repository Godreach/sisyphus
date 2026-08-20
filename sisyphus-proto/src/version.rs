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

/// N-1 兼容窗口的下界（ADR-0010/0017）：对端低于此版本即「过旧」——任务面
/// 拒连、升级面保留。
///
/// 「上一个 minor」的取值约定：`local.minor >= 1` 时下界为
/// `{local.major, local.minor - 1, 0}`；`local.minor == 0`（某 major 的首
/// minor）时回退到 `{local.major - 1, 9, 0}`——即把上一 major 的末 minor
/// 视作 N-1（v1 发布约定：0.9 是 1.0 的 N-1）。major 用 `saturating_sub`
/// 以免 0.0.0（实践中不存在的取值）下溢 panic。minor > 9 的情形不受影响：
/// 仅 `minor == 0` 触发跨 major 回退，1.10 的下界仍是 {1, 9, 0}。
pub fn n_minus_one_floor(local: &Version) -> Version {
    if local.minor == 0 {
        Version {
            major: local.major.saturating_sub(1),
            minor: 9,
            patch: 0,
        }
    } else {
        Version {
            major: local.major,
            minor: local.minor - 1,
            patch: 0,
        }
    }
}

/// 对端是否过旧（< N-1 下界，semver 全序）。过旧 Agent：任务面拒连、升级
/// 面保留（ADR-0017）。握手不拒（`compatible` 只判过新），过旧仅在调度派发
/// 与 UI 四态派生时生效——由 server 侧在 B5-T4 落地。
pub fn peer_too_old(peer: &Version, local: &Version) -> bool {
    let floor = n_minus_one_floor(local);
    (peer.major, peer.minor, peer.patch) < (floor.major, floor.minor, floor.patch)
}

/// 对端是否落在 N-1 兼容窗口内（不过新且不过旧）。升级包上传校验与调度
/// 派发门共用：窗外拒收 / 拒派。
pub fn peer_in_window(peer: &Version, local: &Version) -> bool {
    !peer_too_new(peer, local) && !peer_too_old(peer, local)
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
        // N-1 兼容窗口：旧 Agent 可连（握手只判过新，`compatible` 放行）。
        // 任务面「过旧拒派」是更窄的门，见 `peer_too_old` 测试——握手与派发
        // 是两道独立门，0.9 可连且仍在 1.0 的 N-1 窗口内（可派发）。
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

    /// N-1 下界：1.0 的下界是 0.9（minor==0 跨 major 回退约定），
    /// 1.2 的下界是 1.1（同 major 上一 minor）。
    #[test]
    fn n_minus_one_floor_wraps_at_first_minor() {
        assert_eq!(
            n_minus_one_floor(&VERSION),
            Version {
                major: 0,
                minor: 9,
                patch: 0
            },
            "1.0.0 的 N-1 下界是 0.9.0"
        );
        assert_eq!(
            n_minus_one_floor(&Version {
                major: 1,
                minor: 2,
                patch: 5
            }),
            Version {
                major: 1,
                minor: 1,
                patch: 0
            },
            "1.2.x 的下界取 1.1.0（patch 抹零）"
        );
        assert_eq!(
            n_minus_one_floor(&Version {
                major: 2,
                minor: 0,
                patch: 0
            }),
            Version {
                major: 1,
                minor: 9,
                patch: 0
            },
            "2.0.0 的下界跨 major 回退到 1.9.0"
        );
    }

    /// 过旧判定 + 窗口：0.9 在 1.0 窗口内（可派发），0.8 过旧（任务面拒派、
    /// 升级面保留），1.0 自身与 1.0.1 都在窗口内。
    #[test]
    fn peer_too_old_and_in_window_for_server_1_0_0() {
        // 0.9.0 = 1.0 的 N-1，在窗口内：不过旧、不过新。
        let n_minus_1 = Version {
            major: 0,
            minor: 9,
            patch: 0,
        };
        assert!(!peer_too_old(&n_minus_1, &VERSION));
        assert!(peer_in_window(&n_minus_1, &VERSION));

        // 0.9.5 仍在窗口内（patch 不影响下界）。
        assert!(peer_in_window(
            &Version {
                major: 0,
                minor: 9,
                patch: 5
            },
            &VERSION
        ));

        // 0.8.0 过旧（< 0.9 下界）：窗口外、任务面拒派。
        let too_old = Version {
            major: 0,
            minor: 8,
            patch: 0,
        };
        assert!(peer_too_old(&too_old, &VERSION));
        assert!(!peer_in_window(&too_old, &VERSION));
        // 过旧仍可连（握手只判过新）——升级面保留语义的契约点。
        assert!(compatible(&too_old, &VERSION));

        // 1.0.0 自身（== Server，窗口上界）在窗口内。
        assert!(peer_in_window(&VERSION, &VERSION));

        // 1.0.1 比 Server 高一个 patch → 过新（ADR-0010：Agent 新于 Server 即
        // 拒连），窗口外。窗口是 [下界, Server] 闭区间，上界即 Server 自身。
        assert!(!peer_in_window(
            &Version {
                major: 1,
                minor: 0,
                patch: 1
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

        // 1.1.0 过新（> Server）：窗口外。
        assert!(!peer_in_window(
            &Version {
                major: 1,
                minor: 1,
                patch: 0
            },
            &VERSION
        ));
    }

    /// 1.2 Server 的窗口 = [1.1, 1.2]：1.1 在窗、1.0 过旧、0.9 过旧、1.3 过新。
    #[test]
    fn peer_too_old_and_in_window_for_server_1_2_0() {
        let server = Version {
            major: 1,
            minor: 2,
            patch: 0,
        };
        assert!(peer_in_window(
            &Version {
                major: 1,
                minor: 1,
                patch: 0
            },
            &server
        ));
        assert!(peer_too_old(
            &Version {
                major: 1,
                minor: 0,
                patch: 0
            },
            &server
        ));
        // 0.9 远低于 1.1 下界 → 过旧。
        assert!(peer_too_old(
            &Version {
                major: 0,
                minor: 9,
                patch: 0
            },
            &server
        ));
        assert!(!peer_in_window(
            &Version {
                major: 1,
                minor: 3,
                patch: 0
            },
            &server
        ));
    }
}
