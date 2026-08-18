//! `${name}` 变量引用解析（ADR-0006/0011）。
//!
//! 语法：`${name}` 展开、`$${name}` 转义为字面 `${name}`；可在任意字符串字段
//! 中使用。8 个内置变量与 Pipeline 参数同一套语法。
//!
//! 解析分工：7 个内置变量与用户参数由 Server 端解析完毕；`SISY_WORKSPACE` 以
//! 占位符随任务规格下发、Agent 执行前替换（ADR-0011）。when 与缓存 key 禁用
//! `SISY_WORKSPACE`（由保存校验拒绝，见 validate 模块）。

/// 8 个内置变量（ADR-0006）。除 `SISY_WORKSPACE` 外均 Server 端解析。
pub const BUILTIN_VARIABLES: [&str; 8] = [
    "SISY_BUILD_NUMBER",
    "SISY_PIPELINE_NAME",
    "SISY_PROJECT_NAME",
    "SISY_JOB_NAME",
    "SISY_STAGE_NAME",
    "SISY_COMMIT_ID",
    "SISY_BRANCH",
    "SISY_WORKSPACE",
];

/// 是否为内置变量名。
pub fn is_builtin(name: &str) -> bool {
    BUILTIN_VARIABLES.contains(&name)
}

/// 变量名是否合法（用于未定义变量检测与校验信息）。
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// 变量值查询函数：给定变量名返回其值（`None` = 未定义）。
pub type Lookup<'a> = Box<dyn Fn(&str) -> Option<String> + 'a>;

/// 变量解析配置：Server 端把能解析的变量解析完，`SISY_WORKSPACE` 等保留。
pub struct Resolver<'a> {
    /// 变量值查询（Pipeline 参数 + Server 端解析的 7 个内置变量）。
    lookup: Lookup<'a>,
    /// 当变量未定义时：`None` = 保留原样（占位符语义），`Some` = 报错名。
    undefined: UndefinedPolicy,
}

/// 未定义变量处理策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndefinedPolicy {
    /// 保留 `\${name}` 原样（占位符语义，`SISY_WORKSPACE` 场景）。
    Keep,
    /// 记录未定义变量（校验场景：返回错误）。
    Report,
}

impl<'a> Resolver<'a> {
    /// 新建解析器。
    pub fn new(lookup: impl Fn(&str) -> Option<String> + 'a, undefined: UndefinedPolicy) -> Self {
        Self {
            lookup: Box::new(lookup),
            undefined,
        }
    }

    /// 展开输入字符串中的 `${name}`（`$${name}` 转义）。
    ///
    /// 返回 `(展开结果, 未定义变量列表)`。`Keep` 策略下未定义变量保留原样且
    /// 不进错误列表；`Report` 策略下未定义变量被记入错误列表。
    pub fn resolve(&self, input: &str) -> (String, Vec<String>) {
        let mut out = String::new();
        let mut undefined = Vec::new();
        let chars: Vec<char> = input.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            let c = chars[i];
            // 处理转义：$${ -> 字面 ${
            if c == '$' && i + 1 < chars.len() && chars[i + 1] == '$' {
                out.push('$');
                i += 2;
                continue;
            }
            // 处理引用：${name}
            if c == '$' && i + 1 < chars.len() && chars[i + 1] == '{' {
                if let Some(end) = input[i + 2..].find('}') {
                    let name = &input[i + 2..i + 2 + end];
                    if is_valid_name(name) {
                        match (self.lookup)(name) {
                            Some(v) => out.push_str(&v),
                            None => {
                                if self.undefined == UndefinedPolicy::Keep {
                                    // 保留占位符原样（如 SISY_WORKSPACE）
                                    out.push_str(&format!("${{{name}}}"));
                                } else {
                                    undefined.push(name.to_string());
                                    // Report 策略下也保留原样，便于调用方展示
                                    out.push_str(&format!("${{{name}}}"));
                                }
                            }
                        }
                        i += 2 + end + 1; // 跳过 ${name}
                        continue;
                    }
                }
                // 未闭合或非法名：按字面输出
                out.push(c);
                i += 1;
                continue;
            }
            out.push(c);
            i += 1;
        }

        (out, undefined)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_lookup(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn resolves_variables_from_lookup() {
        let r = Resolver::new(
            |name: &str| match name {
                "SISY_BRANCH" => Some("main".into()),
                "target" => Some("x86_64".into()),
                _ => None,
            },
            UndefinedPolicy::Keep,
        );
        let (out, undefined) = r.resolve("branch=${SISY_BRANCH} arch=${target}");
        assert_eq!(out, "branch=main arch=x86_64");
        assert!(undefined.is_empty());
    }

    #[test]
    fn escapes_double_dollar() {
        let r = Resolver::new(no_lookup, UndefinedPolicy::Keep);
        let (out, _) = r.resolve("$${SISY_BRANCH}");
        assert_eq!(out, "${SISY_BRANCH}");
    }

    #[test]
    fn keeps_undefined_as_placeholder() {
        let r = Resolver::new(no_lookup, UndefinedPolicy::Keep);
        let (out, undefined) = r.resolve("${SISY_WORKSPACE}/sub");
        assert_eq!(out, "${SISY_WORKSPACE}/sub");
        assert!(undefined.is_empty());
    }

    #[test]
    fn reports_undefined_when_policy_is_report() {
        let r = Resolver::new(no_lookup, UndefinedPolicy::Report);
        let (out, undefined) = r.resolve("a=${MISSING}");
        assert_eq!(out, "a=${MISSING}");
        assert_eq!(undefined, vec!["MISSING"]);
    }

    #[test]
    fn builtin_enumeration_is_complete() {
        assert_eq!(BUILTIN_VARIABLES.len(), 8);
        assert!(is_builtin("SISY_WORKSPACE"));
        assert!(is_builtin("SISY_BUILD_NUMBER"));
        assert!(!is_builtin("SISY_NOPE"));
    }

    #[test]
    fn mixed_escape_and_resolve() {
        let r = Resolver::new(
            |name: &str| if name == "A" { Some("1".into()) } else { None },
            UndefinedPolicy::Keep,
        );
        let (out, _) = r.resolve("$${A} ${A} ${B}");
        assert_eq!(out, "${A} 1 ${B}");
    }
}
