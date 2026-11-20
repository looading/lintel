# 崩溃报告插件实现计划

**目标** 实现[插件设计](../specs/2026-11-20-crash-reports-plugin-design.md)中的应用接入、非致命快照、本地报告队列和独立上传进程。

**依赖关系** 捕获库生成 minidump，报告库负责导入和发送，Lintel facade 在现有启动点装配两者。已有捕获入口和包格式继续可用。首次实机范围是 macOS arm64。其他平台的手动抓取返回可识别的错误。

## 捕获库和独立快照

- [x] 在 `crates/lintel-crash` 增加一次性快照入口 `capture_snapshot(helper, output, timeout)` 和独立 helper。私有 IPC 只为本次请求交接宿主的 task 权限。验证完成文件，再交给报告库。
- [x] 给致命捕获写入向后兼容的捕获时间，保留 `report.json` 和 `completed` 原格式。
- [x] 执行 `cargo test --manifest-path crates/lintel-crash/Cargo.toml --locked --features test-capture`。覆盖重复非致命捕获、随后真实崩溃和失败后的资源回收。

## 报告库和上传进程

- [x] 新建 `crates/lintel-crash-reports`。用 SQLite 持久记录报告标识、来源、策略、发送意图和回执。原生 `completed` 和 macOS CEF 已完成目录是导入入口。手动快照走单独登记方法。
- [x] 同一个 uploader 持有队列发送锁，按应用配置读取私有目录。通过继承通道装配，宿主退出后按时限继续扫描，离线时保留报告。用稳定的幂等键发送并校验服务端回执。
- [x] 执行 `cargo test --manifest-path crates/lintel-crash-reports/Cargo.toml --locked --all-targets`。覆盖上传策略、崩溃恢复、重复提交和上传回执。

## Lintel facade 和交付

- [x] 在 `src/crash_reports.rs` 暴露 `CrashReportsConfig` 与 `CrashReports` 服务。配置通过 `Application`、`MultiWindow`、`WebviewApplication` 和 `AppBuilder` 接入。用现有启动顺序安装 capture backend，在 `AppContext` 使用应用级任务调用快照和队列 API。
- [x] 用现有 helper 校验打包 `lintel-crash-uploader` 与 `lintel-snapshot-helper`。更新框架目录、CLI 源码归档、README 和调用示例。
- [x] 对 `crash-reports` 运行聚焦框架检查和测试。运行相关 `lintel-build`、`lintel-cli` 测试，记录各自的验收结果。

## 验证结果

- macOS arm64 原生捕获通过 3 个单元测试和 6 个隔离进程测试。同一进程生成两份手动快照之后仍能捕获真实崩溃。临时签名的 Hardened Runtime 探针通过签名检查，致命报告可用对应构建的符号还原到预期源码行。
- 报告库通过 12 个单元测试和 1 个 uploader 进程测试。用本地 HTTPS 验证回执丢失后重试复用幂等键和内容，另外验证宿主连接关闭、迟到报告和上传授权撤销。
- 框架接入测试 3 项，CLI 测试 31 项，独立助手打包测试 6 项通过。带两个实际助手的临时签名示例包可以通过公开 API 抓取、登记和清理暂存的非致命快照。另一个临时签名的 CEF 应用从隔离 renderer 崩溃恢复，插件能导入该次真实报告并识别捕获时间。
- Mojo worker 报告导入使用测试夹具。Windows 与 Linux 没有执行原生验收。上述签名均为本地临时签名。
