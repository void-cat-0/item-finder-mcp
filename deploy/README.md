# 服务部署模板

本目录只有三个**待替换模板**，不是安装器，也不会自动创建用户、目录、模型或数据备份。安装前先读 [常驻运行与运维](../docs/operations.md)，并把模板中的占位路径替换为真实的绝对路径。

| 平台 | 实际模板 | 服务管理器 | 默认触发 | 优雅停止来源 |
|---|---|---|---|---|
| Linux | `item-ingest.service` | systemd | `multi-user.target` | `SIGTERM`（也支持 SIGINT/SIGHUP） |
| macOS | `com.whereiput.ingest.plist` | launchd | `RunAtLoad` | `SIGTERM`（也支持 SIGINT/SIGHUP） |
| Windows | `install-windows-task.ps1` | Task Scheduler | `AtLogOn`，或脚本的 `-AtStartup` | 控制台事件；任务停止命令是硬停 |

三个模板均只启动 `item-ingest --daemon --config <绝对路径>`。features、模型参数、RTSP/USB camera 和 webhook 端口由已构建 exe、TOML 与环境决定；模板不替换它们。

## 通用准备

1. 构建与分发包含实际所需 cargo features 的 `item-ingest`，并把 YOLO 模型、RTSP 动态库（如需要）放在可读位置。模型由 `--model` 或其默认值提供，不是 TOML 的 `model` 字段。
2. 复制配置并替换 `[[camera]]`、输出目录、权限和 token。daemon 的相对 `db`、快照、health、事件和模型路径相对 **config.toml 所在目录**，不是服务管理器的当前目录；配置和数据可同树，也可在 TOML 中写绝对路径。
3. 创建模板引用的 WorkingDirectory、数据目录和日志目录；launchd 不会创建日志父目录。账号必须能读 exe/config/model/USB 设备，并能写 DB 的 WAL/SHM、快照、health、events、raw/标注目录。
4. 只让一个 daemon 写一个数据库。启动后检查服务状态、日志、health 的 `updated_at`、逐 camera state/last_error/last_frame_age_s。health 超过 90 秒说明信号过期，不单独证明进程已死。
5. 模板没有 secret 代理。`ITEM_DAEMON_TOKEN`、`ITEM_DAEMON_ANNOTATION_TOKEN`、VLM 等环境应通过对应平台的受保护环境机制注入，不能把 token 或带凭据 RTSP URL 提交进仓库。

停止语义：Unix 的 SIGTERM/SIGINT/SIGHUP 会让 daemon 停止接收、请求 camera drain（10 秒预算）、写最终 health、checkpoint 并释放锁。操作系统强杀可能留下 stale lock。没有通用 `--stop` 参数；维护和锁恢复的真实语义见 [运维指南](../docs/operations.md)。

## Linux：systemd

只改 `deploy/item-ingest.service` 中的 `User`、`Group`、`WorkingDirectory`、`ExecStart`、`ReadWritePaths`；`ExecStart` 的 config 路径、exe 路径必须指向实际文件，`ReadWritePaths` 覆盖 TOML 的所有写入目录。`ProtectSystem=strict` 会使未列出的目录不可写。USB 账号通常需要 `video` 组（以系统实际设备权限为准）。

```sh
sudo install -m 0644 deploy/item-ingest.service /etc/systemd/system/item-ingest.service
sudo systemctl daemon-reload
sudo systemctl enable --now item-ingest.service
systemctl status item-ingest.service
journalctl -u item-ingest.service -f
```

模板是 `Restart=on-failure`；正常处理停止信号退出 0，不应因正常停止重启。异常重启时 daemon 自己仅在确认 lock heartbeat 已停止且 health 不新鲜后接管 stale lock。修改 unit 后执行 `daemon-reload`，再按维护窗口重启。

```sh
sudo systemctl stop item-ingest.service
sudo systemctl disable item-ingest.service   # 如需取消开机启动
sudo systemctl reset-failed item-ingest.service
```

`stop` 发送模板中的 SIGTERM；若进程卡死，systemd 最终可能强制结束，先查日志和 health，不要直接把 `--maintenance --force` 当日常修复。

## macOS：launchd

模板支持两种实际安装位置，但其 lifetime/权限不同：

- `~/Library/LaunchAgents/`：当前用户登录时启动，作为该用户运行。
- `/Library/LaunchDaemons/`：系统启动时加载，通常作为 root；这不会自动赋予 USB/网络摄像头正确的业务权限，生产上应评估专用服务账号和设备权限。

先把 plist 中的每个 `/opt/where-i-put` 替换掉，并预先创建 `StandardOutPath`/`StandardErrorPath` 的父目录。

```sh
mkdir -p /实际部署目录/data
cp deploy/com.whereiput.ingest.plist ~/Library/LaunchAgents/
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.whereiput.ingest.plist
launchctl print gui/$(id -u)/com.whereiput.ingest
launchctl kickstart -k gui/$(id -u)/com.whereiput.ingest
```

该 plist 的 `KeepAlive=true` 会在任何退出（包括干净退出）后再次拉起；停止后若不先卸载，launchd 可能立即重启它。优雅卸载：

```sh
launchctl bootout gui/$(id -u)/com.whereiput.ingest
```

系统 LaunchDaemon 使用 `sudo launchctl bootstrap system /Library/LaunchDaemons/com.whereiput.ingest.plist`、`launchctl print system/com.whereiput.ingest` 和 `sudo launchctl bootout system/com.whereiput.ingest`；不要把用户级 `gui/<uid>` 命令混用。`kickstart -k` 触发重启/终止，是否送达优雅信号取决于 launchd/进程状态，仍要核对最终 health 与锁。

## Windows：任务计划程序

实际脚本是 `deploy\install-windows-task.ps1`。它检查 exe/config 存在，以 config 目录为 WorkingDirectory，注册一个通过 Task Scheduler 启动的 daemon；默认当前用户 `AtLogOn`，`-AtStartup` 改为系统启动触发。

```powershell
powershell -ExecutionPolicy Bypass -File .\deploy\install-windows-task.ps1 `
  -ExePath C:\where-i-put\item-ingest.exe `
  -ConfigPath C:\where-i-put\config.toml

Start-ScheduledTask -TaskName 'where-i-put ingest'
Get-ScheduledTaskInfo -TaskName 'where-i-put ingest'
```

脚本实际使用 `Interactive` principal、`Limited` run level。即使指定 `-AtStartup`，也**不保证无人登录时可运行**：Interactive 任务需要相应用户会话/权限，摄像头访问还受 Windows 隐私与设备权限影响。需要无人登录的场景不能仅凭此模板宣称已实现；应使用经验证的服务身份/包装器方案，并单独验收设备访问和优雅停止。

脚本配置异常退出最多重启 3 次、间隔 1 分钟；daemon 内部另行重连每台摄像头。脚本没有日志重定向，按 Task Scheduler 记录和 daemon 启动环境收集 stderr/stdout。

停止/移除：

```powershell
Stop-ScheduledTask -TaskName 'where-i-put ingest'
Get-ScheduledTaskInfo -TaskName 'where-i-put ingest'
powershell -ExecutionPolicy Bypass -File .\deploy\install-windows-task.ps1 -Unregister
```

`Stop-ScheduledTask` 会直接结束进程，不保证发送 Ctrl-C/Ctrl-Break；它不是优雅停止。硬停后可能留下 lock，下一次 daemon 仅在 lock heartbeat 至少 45 秒无更新且 health 不新鲜时自动接管。不要反复硬停，也不要日常使用 maintenance `--force`；确认不存在旧进程、暂停任务触发并检查日志/health 后再处理残留。

## 安装后的最小验收

- 先确认任务/服务管理器报告的实际 exe、config、账号和 WorkingDirectory。
- 确认只有一个 daemon 写入该 DB；health 的 camera source 已脱敏、状态和帧计数符合实际，日志没有持续 connect/model 错误。
- 分别测试一次管理器 stop/start 和机器/用户会话重启；Windows 尤其测试登录与未登录差异。不要把“已注册”“进程存在”当作摄像头或检测验收。
- 升级时不要覆盖运行中的 Windows exe/DLL；在新版本目录显式构建并切换服务路径，或先停止并确认文件已释放。`cargo test` 不会刷新普通 exe。
- 离线维护会删除过期数据、清理 orphan、checkpoint 并 VACUUM，不是只读检查；先备份并停止写入。详见 [常驻运行与运维](../docs/operations.md)。
