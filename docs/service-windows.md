# Windows 服务化指引（sc.exe / NSSM）

票 B5-T10 / #82，ADR-0010 服务化示例。v1 仅文档示例，不做内置服务安装子命令
（服务参数集稳定后再评估）。Agent 在 Windows 上同样可参照本指引常驻。

## 前置

- 解压 `sisyphus-server-<ver>-windows-x86_64.zip`，把 `sisyphus-server.exe`
  放到固定目录（例 `C:\Program Files\sisyphus\`）。
- 数据目录默认 `./data`（相对 exe 工作目录）；建议显式 `--data-dir`
  指向独立目录（例 `C:\ProgramData\sisyphus`），内含 `sisyphus.db`、
  `artifacts/`、`master.key`，首启自动生成 `config.toml` + `master.key`。
- gRPC 默认 bind `127.0.0.1:50051`（loopback）——远端 Agent 需经环境变量
  `SISYPHUS_GRPC_ADDR=0.0.0.0:50051` 覆盖（见下）。

## 方案一：sc.exe（系统内置，原生服务）

`sc.exe` 创建的是原生 Windows 服务，要求进程自身实现 Service Control 接口。
**sisyphus-server v1 是普通控制台程序，未实现 SCM 接口**——直接 `sc create`
 起来后服务控制管理器无法正确启动它。故 Windows 上推荐方案二（NSSM）；
`sc.exe` 仅作「已实现 SCM 接口的服务」的参考形态保留，本期不适用。

## 方案二：NSSM（推荐——把任意 exe 包成服务）

下载 [NSSM](https://nssm.cc/)，管理员 PowerShell：

```powershell
nssm install sisyphus-server "C:\Program Files\sisyphus\sisyphus-server.exe"
nssm set    sisyphus-server AppParameters "--data-dir C:\ProgramData\sisyphus"
nssm set    sisyphus-server AppDirectory "C:\Program Files\sisyphus"
# 远端 Agent 经 gRPC 连接时放开 loopback（覆盖默认 127.0.0.1:50051）：
nssm set    sisyphus-server AppEnvironmentExtra "SISYPHUS_GRPC_ADDR=0.0.0.0:50051"
nssm set    sisyphus-server AppStdout  "C:\ProgramData\sisyphus\logs\stdout.log"
nssm set    sisyphus-server AppStderr  "C:\ProgramData\sisyphus\logs\stderr.log"
nssm set    sisyphus-server AppRotateFiles 1
nssm set    sisyphus-server AppRotateBytes 10485760
nssm set    sisyphus-server Start SERVICE_AUTO_START
nssm start  sisyphus-server
```

管理：

```powershell
nssm stop    sisyphus-server     # 停
nssm restart sisyphus-server     # 重启
nssm remove  sisyphus-server confirm   # 卸载
```

## 升级顺序（ADR-0010）

Server 先升、Agent 后升。升级 = 替换 `sisyphus-server.exe` 后重启服务
（`nssm restart sisyphus-server`）；启动自动跑前向迁移并迁移前自动备份
db（`<data-dir>\backups\`）。不支持降级。N-1 兼容窗口：1.x Server 支持上一
个 minor 的 Agent。
