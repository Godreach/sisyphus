// 按目标 OS 的 Agent 注册命令构建（ADR-0010/0007，票 B4-T5）。
//
// `sisyphus-agent --server-url <grpc> --api-url <rest> --reg-key <code>`：
// - `--reg-key` 是注册引导的实际 CLI flag（票 #57；ADR-0010 旧示例里的
//   `--registration-code` 是 #57 落地前的示意名，实际二进制以 clap 定义为准）。
// - `--api-url`（REST 注册面）与 `--server-url`（gRPC 通道）是两个不同端口：
//   默认 50051 / 8080（`config.rs`）。前端无法可靠获知部署者实际地址，占位
//   `<server>` 随部署替换（与 README 同约定）。
// - Windows 构建机二进制带 `.exe` 后缀（ADR-0010 发布矩阵命名）。
//
// 抽取自 SetupView（票 B4-T2）的 `agentCommand` 内联实现，行为不变；Agent
// 列表页建条目复用同一份命令形态，避免两处复制漂移。

/** 注册命令的目标 OS（ADR-0010 发布矩阵：Windows 带 .exe；linux/macos 同形）。 */
export type AgentTargetOs = 'linux' | 'macos' | 'windows'

/** 按目标 OS 生成复制即用注册命令。`registerCode` 为建条目响应的一次性注册码。 */
export function buildAgentRegisterCommand(
  os: AgentTargetOs,
  registerCode: string,
): string {
  const serverUrl = 'http://<server>:50051'
  const apiUrl = 'http://<server>:8080'
  const bin = os === 'windows' ? 'sisyphus-agent.exe' : 'sisyphus-agent'
  return `${bin} --server-url ${serverUrl} --api-url ${apiUrl} --reg-key ${registerCode}`
}
