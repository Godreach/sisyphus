//! 任务机密输出字面量脱敏（ADR-0015；票 B3-T5 / #59）。
//!
//! Agent 侧精确字面量脱敏：本任务注入的机密值（机密 env 值 + checkout 凭据
//! password）在输出块离机前替换为 `***`。断线缓冲、补传、Server 落库
//! （ADR-0013 链路）全部是脱敏后版本——脱敏在喂给 [`crate::logbuf`] 之前。
//!
//! - **跨输出块边界**（ADR-0013「输出块」= 一个 `OutputChunk` 帧）：机密值
//!   可能被拆在两次读取的两个块之间。[`Redactor`] 是有状态的流式脱敏器：
//!   每次 [`Redactor::process`] 喂入一段字节，返回可安全外发的脱敏后字节，
//!   末尾最多 `max_secret_len - 1` 字节暂留（可能是某机密的前缀，等下一段
//!   补齐再判）；[`Redactor::flush`] 在流结束时把暂留字节作为明文外发
//!   （无后续字节，部分前缀即普通文本）。
//! - **最长匹配优先**（ADR-0015「重叠以最长匹配优先」）：同一位置命中多个
//!   机密时取最长；左优先于右（左匹配先生效）。对标 GitHub Actions 内置
//!   脱敏的同等承诺，不做变形匹配（base64/截断绕过防不住，文档写明）。
//! - **无机密直通**：机密清单为空时 [`Redactor`] 不暂留、不拷贝，原样外发
//!   （零开销快路径）。
//!
//! 纯逻辑模块：无 IO、无 proto 依赖，内联单测覆盖跨块边界 / 最长匹配 /
//! 多机密 / 直通 / flush。

/// 流式字面量脱敏器（跨输出块边界、最长匹配优先）。
///
/// 一个 [`Redactor`] 绑定一条输出流（stdout 或 stderr）的机密集合——同流
/// 内跨块边界由本状态机吸收；不同流各自独立（stdout/stderr 的 stream 标记
/// 在 [`crate::runner`] 编码层保留，不在此混淆）。
pub struct Redactor {
    /// 机密字面量（按长度降序，便于同位置取最长；空串已滤除）。
    secrets: Vec<Vec<u8>>,
    /// 最长机密字节数（暂留窗口 = max_len - 1；0 = 无机密）。
    max_len: usize,
    /// 暂留窗口：尚未外发的尾段（可能是某机密的前缀，等下一段补齐）。
    pending: Vec<u8>,
}

/// 脱敏后的占位标记（ADR-0015：字面量替换为 `***`）。
const MASK: &[u8] = b"***";

impl Redactor {
    /// 以机密字面量集合构造。空串自动滤除（空机密无意义且会到处命中）。
    /// 集合为空 = 直通模式（[`Self::process`] 原样返回输入、不暂留）。
    pub fn new(mut secrets: Vec<Vec<u8>>) -> Self {
        secrets.retain(|s| !s.is_empty());
        let max_len = secrets.iter().map(|s| s.len()).max().unwrap_or(0);
        // 降序：同位置扫描时先试长的，便于取最长命中（长度并列时取先列入的，
        // 不影响脱敏正确性——等长命中替换结果同为 `***`）。
        secrets.sort_by_key(|b| std::cmp::Reverse(b.len()));
        Self {
            secrets,
            max_len,
            pending: Vec::new(),
        }
    }

    /// 是否为直通模式（无机密）。直通时 [`Self::process`] 零拷贝语义（返回
    /// 输入的拷贝但不暂留；调用方在直通时可跳过本脱敏器）。
    pub fn is_passthrough(&self) -> bool {
        self.max_len == 0
    }

    /// 喂入一段字节，返回可安全外发的脱敏后字节。末尾最多 `max_len - 1`
    /// 字节暂留（可能是机密前缀，等下一段补齐再判）。直通模式下原样返回
    /// （不暂留）。
    pub fn process(&mut self, input: &[u8]) -> Vec<u8> {
        if self.max_len == 0 {
            // 直通：无机密可匹配，不暂留、不拷贝 pending。
            return input.to_vec();
        }
        self.pending.extend_from_slice(input);
        let mut out = Vec::with_capacity(self.pending.len());
        // 反复扫左最长命中：命中即外发命中前明文 + `***`，drain 命中段后重扫。
        while let Some((pos, len)) = find_leftmost_longest(&self.pending, &self.secrets) {
            out.extend_from_slice(&self.pending[..pos]);
            out.extend_from_slice(MASK);
            self.pending.drain(..pos + len);
        }
        // 无完整命中：除末尾 `max_len - 1` 暂留窗外，前段可安全外发（其内无
        // 命中——左扫描已穷尽）。暂留窗口 = 可能的机密前缀上限。
        let keep = self.max_len - 1;
        let safe = self.pending.len().saturating_sub(keep);
        out.extend_from_slice(&self.pending[..safe]);
        self.pending = self.pending[safe..].to_vec();
        out
    }

    /// 流结束：把暂留窗口作为明文外发（无后续字节，部分前缀即普通文本）。
    /// 直通模式下暂留恒空，返回空。
    pub fn flush(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

/// 在 `buf` 中找最左、同位置最长的机密命中：返回 `(起始, 长度)`。
///
/// 左优先：从位置 0 起逐位试，首个有任何命中的位置即取该位置最长的命中。
/// `secrets` 已按长度降序，故同位置首个命中的即最长。
fn find_leftmost_longest(buf: &[u8], secrets: &[Vec<u8>]) -> Option<(usize, usize)> {
    if secrets.is_empty() {
        return None;
    }
    for pos in 0..buf.len() {
        for s in secrets {
            if buf[pos..].starts_with(s) {
                return Some((pos, s.len()));
            }
        }
    }
    None
}

// ============================================================
// 单元测试（纯逻辑：跨块边界 / 最长匹配 / 多机密 / 直通 / flush）
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn redact_one(secrets: &[&[u8]], input: &[u8]) -> Vec<u8> {
        let mut r = Redactor::new(secrets.iter().map(|s| s.to_vec()).collect());
        let mut out = r.process(input);
        out.extend(r.flush());
        out
    }

    #[test]
    fn empty_secrets_is_passthrough() {
        let mut r = Redactor::new(vec![]);
        assert!(r.is_passthrough());
        assert_eq!(r.process(b"anything"), b"anything");
        assert_eq!(r.flush(), b"");
    }

    #[test]
    fn redacts_full_literal_in_one_chunk() {
        assert_eq!(
            redact_one(&[b"secret"], b"prefix secret suffix"),
            b"prefix *** suffix"
        );
    }

    #[test]
    fn redacts_across_chunk_boundary() {
        // 机密 "secret" 拆在两段：第一段到 "sec"（暂留），第二段补 "ret"。
        let mut r = Redactor::new(vec![b"secret".to_vec()]);
        let a = r.process(b"xx sec");
        let b = r.process(b"ret yy");
        let mut out = a;
        out.extend(b);
        out.extend(r.flush());
        assert_eq!(out, b"xx *** yy", "跨块边界机密应被脱敏");
    }

    #[test]
    fn retains_only_max_len_minus_one_tail() {
        // 机密长 6 → 暂留 5 字节。第一段末尾 5 字节正好是机密前缀，必须暂留。
        let mut r = Redactor::new(vec![b"secret".to_vec()]);
        let a = r.process(b"ab sec"); // " sec" 是 4 字节前缀（< 5）→ 暂留 " sec"? 实为 "sec" 3 字节
        // "ab sec" 无完整命中；暂留 max_len-1=5 末字节 = "b sec"（5 字节），外发 "a"。
        assert_eq!(a, b"a");
        let b = r.process(b"ret"); // 暂留 "b sec" + "ret" = "b secret" → 命中 "secret" 在 pos2
        // 外发 "b " + "***"，暂留空。
        assert_eq!(b, b"b ***");
        assert_eq!(r.flush(), b"");
    }

    #[test]
    fn longest_match_wins_at_same_position() {
        // "abcd" 与 "abc" 同位置（0）命中，取最长 "abcd"。
        assert_eq!(redact_one(&[b"abc", b"abcd"], b"abcd end"), b"*** end");
        // 顺序无关：颠倒列举仍取最长。
        assert_eq!(redact_one(&[b"abcd", b"abc"], b"abcd end"), b"*** end");
    }

    #[test]
    fn leftmost_wins_when_overlapping() {
        // "abc"（pos0）与 "bcd"（pos1）重叠于 "abcd"：左优先 → 命中 "abc"，余 "d"。
        assert_eq!(redact_one(&[b"bcd", b"abc"], b"abcd"), b"***d");
    }

    #[test]
    fn multiple_secrets_all_redacted() {
        let mut r = Redactor::new(vec![b"alpha".to_vec(), b"beta".to_vec()]);
        let mut out = r.process(b"alpha-x beta-y");
        out.extend(r.flush());
        assert_eq!(out, b"***-x ***-y");
    }

    #[test]
    fn secret_split_three_ways_across_chunks() {
        // "secret" 拆三段，每段都不构成完整命中，拼齐后脱敏。
        let mut r = Redactor::new(vec![b"secret".to_vec()]);
        let mut out = r.process(b"a se");
        out.extend(r.process(b"cr"));
        out.extend(r.process(b"et b"));
        out.extend(r.flush());
        assert_eq!(out, b"a *** b");
    }

    #[test]
    fn no_match_passes_through_unchanged() {
        // 含非 ASCII 的普通输出：无机密命中，原样往返（字节级，不依赖字符串语义）。
        let input = "完全无关的输出 no match";
        assert_eq!(redact_one(&[b"secret"], input.as_bytes()), input.as_bytes());
    }

    #[test]
    fn empty_input_processes_and_flushes_cleanly() {
        let mut r = Redactor::new(vec![b"secret".to_vec()]);
        assert_eq!(r.process(b""), b"");
        assert_eq!(r.flush(), b"");
    }

    #[test]
    fn single_byte_secret_needs_no_retain() {
        // 单字节机密：暂留窗口 = 0，永不暂留，跨块也直接命中。
        let mut r = Redactor::new(vec![b"X".to_vec()]);
        let a = r.process(b"ab");
        let b = r.process(b"Xc");
        let mut out = a;
        out.extend(b);
        out.extend(r.flush());
        assert_eq!(out, b"ab***c");
    }

    #[test]
    fn flush_emits_retained_prefix_as_plaintext() {
        // 末段以机密前缀结尾（未拼齐）→ flush 作明文外发（无后续字节补齐）。
        let mut r = Redactor::new(vec![b"secret".to_vec()]);
        let a = r.process(b"hello sec");
        let mut out = a.clone();
        out.extend(r.flush()); // " sec"? 实为暂留 "o sec"(5) 外发 "hell"
        // "hello sec": 无命中；暂留 max_len-1=5 末字节 = "o sec"，外发 "hell"。
        assert_eq!(a, b"hell");
        // flush 外发暂留 "o sec"（明文——已无后续字节，前缀即普通文本）。
        out.extend(r.flush());
        assert_eq!(out, b"hello sec");
    }

    #[test]
    fn redacts_repeated_adjacent_secrets() {
        let mut r = Redactor::new(vec![b"s".to_vec()]);
        let mut out = r.process(b"sss");
        out.extend(r.flush());
        assert_eq!(out, b"*********", "三个单字节机密各替换为 ***（3×3=9 星）");
    }

    #[test]
    fn binary_secret_bytes_roundtrip_redacted() {
        // 二进制机密值（含 0xFF/换行）——字面量字节匹配，不依赖字符串语义。
        let secret = vec![0u8, 255, b'\n', b'X'];
        let mut r = Redactor::new(vec![secret.clone()]);
        let mut out = r.process(b"\x00\xff\nX trailing");
        out.extend(r.flush());
        assert_eq!(out, b"*** trailing");
    }
}
