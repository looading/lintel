# Lintel 崩溃报告插件设计

应用通过一个可选插件启用崩溃报告。插件提供公开 Rust API，业务可以手动抓取当前进程的快照、查询本地报告和提交上传。自动捕获与自动上传分别配置。现有捕获后端负责生成报告，一个独立上传进程负责发送已完成的报告。它可以在主应用退出后继续尝试发送，未收到回执的报告留待后续重试。

本文定义首版的对外契约和验收条件。下文的插件 API、手动快照和上传进程均为拟实现能力。

## 应用如何接入

新增可选 feature `crash-reports` 和公开模块 `lintel::crash_reports`。`application`、`multi_window`、`webview_app` 和 `AppBuilder` 都提供 `.crash_reports(config)`。builder 用同一份配置装配启动早期的捕获和普通的 `CrashReports` 服务。业务从 `AppContext` 获取服务，界面可以用它实现「生成诊断快照」「发送报告」和「查看发送状态」。

```rust,ignore
let app = app.crash_reports(config);

// 应用启动后，在异步任务中调用
let reports = context
    .get_service::<CrashReports>()
    .ok_or(CrashReportsError::Unavailable)?;
let report = reports.capture_now().await?;
reports.request_upload(report.id).await?;
let status = reports.status(report.id).await?;
```

| API | 返回结果 |
| --- | --- |
| `capture_now()` | 抓取当前进程的非致命 minidump。验证、落盘并登记后才返回 `ReportSummary`。忙、超时、写入失败或平台不支持时返回明确错误。 |
| `list_reports(query)` | 分页读取报告摘要，可按来源、类型和状态筛选。 |
| `status(id)` | 查询 `Local`、`Queued`、`Uploading`、`Uploaded`、`Failed` 或 `Expired`。`Uploaded` 带服务端回执，`Failed` 带类型化原因。 |
| `request_upload(id)` | 为当前接收端持久记录手动发送意图。成功表示报告已受理。处于排队、发送中或已上传时，重复提交返回原状态和已有回执。允许重试的失败可重新排队，复用原报告 ID 和幂等键。 |
| `set_upload_policy(policy)` | 更新并持久化应用的上传策略。返回成功时，在运行的 uploader 已应用新策略，或者当前没有 uploader 持有发送权。 |
| `upload_policy()` | 读取已保存的策略、uploader 已应用的版本及进程状态。策略更新超时时，可用它判断变更是否已落地。 |

`ReportSummary` 包含报告 ID、来源、报告类型、捕获时间、可确认的应用版本、文件大小和状态。`ReportKind` 区分 `Fatal` 与 `ManualSnapshot`，`ReportSource` 区分原生主进程、CEF 和指定 Mojo 服务。CEF 报告没有可验证的进程角色时，角色留空。公开接口使用报告 ID 访问插件管理的文件，调用方不能借它指定任意 PID、文件路径或上传目标。首版在应用进程内开放 Rust API，应用可按自己的授权规则对设置页和用户反馈入口暴露这些操作。

## 捕获与上传策略

插件配置声明宿主后端、自动捕获开关、手动快照开关、可读的 worker 报告目录、上传目标和配额。宿主后端选原生、CEF 或不安装宿主致命捕获。Mojo worker 继续在各自进程中选择是否启用致命捕获，宿主只配置允许读取的来源。启用插件时的默认策略是所选后端开启致命捕获，上传为 `ManualOnly`。应用需要配置目标，手动或自动上传才能提交。

| 配置 | 生效时机与行为 |
| --- | --- |
| 自动捕获 `Off` / `OnFatal` | 应用或 worker 启动时生效。`Off` 关闭对应进程的致命事件捕获。平台支持且应用允许手动快照时，`capture_now()` 仍可调用。 |
| 上传 `Disabled` | 保留本地报告，拒绝 `request_upload`，挂起已排队的上传。策略切换后停止发起新请求并取消仍在执行的请求。切换前已经到达服务端的请求仍可能有回执。 |
| 上传 `ManualOnly` | 发送明确通过 `request_upload` 提交的报告，按配置重试。只因自动策略入队的报告保持挂起，业务可以逐条改为手动提交。 |
| 上传 `Automatic` | 发送符合配置的报告类型和来源，默认选择致命崩溃。业务可将手动快照加入自动发送范围。切换为自动策略时，已有报告默认仍需手动提交。显式选择包含历史报告后，才自动处理这些报告。 |

自动模式启用时记录策略版本与生效时间。原生和 worker helper 在接到捕获请求时记下捕获时间，在本地完成标记发布前写入可选元数据。CEF 使用 adapter 核实的生成时间。缺失或存在歧义的时间留空，允许应用显式纳入历史报告或手动提交。现有原生报告的启动元数据不含捕获时间，已有文件仍可读取。 [原生启动元数据](../../../crates/lintel-crash/src/lib.rs#L82-L92)

手动提交的意图和重试进度跨重启保留，每次实际发送前仍检查当前策略和目标。上传地址只由应用的启动配置提供。报告 ID 在接收端变更后仍可用于查找本地报告。发送状态和回执按报告 ID 与接收端标识记录，API 展示当前接收端的状态。原目标的队列项停留在原队列，应用可以对新目标重新提交。

原生宿主可以用相同成品切换启动时捕获配置。CEF 的 `OnFatal` 依赖随包的 `crash_reporter.cfg`，`Off` 要使用未包含该配置的 bundle。关闭捕获的应用仍可授权读取旧的报告目录。 [CEF 配置校验](../../../src/cef/crash_reports.rs#L109-L137)

## 进程和捕获生命周期

| 运行部件 | 所有权与职责 |
| --- | --- |
| 宿主启动装配 | 识别 crash helper、CEF 子进程和第二实例之后进行装配。复用原生 reporter 的早期安装与退出顺序，CEF 继续从签名前写入 bundle 的配置启动，主进程的原生 reporter 和 CEF Crashpad 继续互斥。 |
| 原生与 CEF 捕获 | 原生主进程和 Mojo worker 各用现有本地 helper。CEF 保持现有本地 Crashpad，从其报告目录导入完成的报告。捕获路径写盘后结束本地确认，网络请求在上传进程中执行。 |
| `CrashReports` 插件服务 | 普通 `PluginLifecycle` 注册服务。手动快照和本地报告索引从受管的应用任务执行，上传禁用时仍能列出本地报告。单次调用有取消和超时边界。 |
| 独立 uploader | 一个应用报告队列由一个 uploader 持有发送权。在启动阶段恢复已授权的请求，自动策略生效或业务提交第一条手动请求时也可启动。它读取已完成报告、处理持久队列和回执。它只需要报告库和上传目标访问权，捕获所需的进程句柄由捕获 helper 持有。 |

上传进程在宿主运行期间按有界周期扫描授权目录，也接收手动提交的唤醒。控制通道意外关闭时，在宽限期内持续扫描和处理已获准的报告。排空一次也继续扫描，因为最后一份 dump 可能晚于宿主退出发布。正常退出时宿主可通知 uploader 完成当前尝试后退出。首版宽限期默认 15 秒，可配置，单次请求超时受剩余宽限期约束。到期、断网或 uploader 被系统回收时，未确认的报告留待下次运行。宿主启动和正常退出均不等待网络请求。uploader 无法启动时保留本地报告与队列状态，插件公开错误状态供业务显示和后续恢复。

uploader 用队列目录上的操作系统排他锁取得发送权。快速重启时，新旧 uploader 可以先后尝试取锁，只有持锁者发送。新宿主持久更新策略，如果旧 uploader 仍在发送，就通过持久策略版本与确认状态等候它应用变更。接收端标识或凭据代次变化时，旧 uploader 停止发送并释放发送权。新进程取得锁并载入对应凭据后才能发送。策略确认超时作为错误返回，调用方可查询当前状态。私有控制通道用于传递启动配置和唤醒，未收到唤醒时周期扫描仍可恢复。限时发送无需安装系统常驻服务，宿主未再启动且发送窗口已结束时，后续补传等下一次运行。

### 手动快照

首版验证 macOS arm64 原生应用。每次手动请求使用独立的一次性捕获通道和报告目录。宿主通过受认证的私有 Mach 通道传递自身的 task 权限，一次性 helper 使用已锁定的 `minidump-writer` 以无异常上下文模式生成 minidump。手动通道自行管理 Mach send right 与 receive right，覆盖成功、超时与取消。抓取运行中的线程属于尽力快照，读取竞态作为失败返回。这条路径与致命捕获有各自的连接和输出文件，开启手动快照无需安装致命 hook。默认同一宿主一次只处理一个手动请求，重入返回 `Busy`。手动快照生成超时后终止并回收该请求的 helper，清理未发布文件。

一次性 helper 只写临时文件，写完同步数据并返回结果。宿主回收 helper、验证临时文件后原子发布报告，持久登记成功才返回 ID。发布前取消会回收 helper 和临时文件，发布后取消保留已生成的报告。手动报告应具有有效模块和线程流，允许省略异常流。致命捕获继续校验异常流。现有 native helper 写完一次就结束捕获循环，因此两种请求必须保持独立。 [现有 helper 完成路径](../../../crates/lintel-crash/src/lib.rs#L485-L511) · [minidump-writer macOS 实现](https://github.com/rust-minidump/minidump-writer/blob/6d49fc6454f5039bac946c31722fd1f147643aa7/src/mac/minidump_writer.rs)

CEF 和 Mojo worker 首版提供已有自动报告的查询与上传。在 CEF browser、renderer、GPU、Mojo worker 和其他平台开放手动快照需要分别验收。未验收的组合由 API 返回 `Unsupported`。CEF 上游的 `CefDumpWithoutCrashing` 位于 C++ wrapper，已有入口无法给业务一个可验证的报告 ID。后续支持需要证明快照请求与落盘报告准确对应。 [CEF 接口](https://github.com/chromiumembedded/cef/blob/dd46a3761b5389255e1824c81ae1b131459f7cd0/include/base/cef_dump_without_crashing.h) · [CEF 当前配置检查](../../../src/cef/crash_reports.rs#L104-L146)

## 报告导入与持久化

- **原生与 worker。** 从应用显式授权的私有目录读取 `report.json`、`report.dmp` 和内容匹配协议版本的 `completed` 标记。部分写入、仅元数据和写入失败留档无法进入队列。保留报告生成时的版本，worker 报告还保留应用声明的服务标识和监管启动次数。 [原生报告元数据](../../../crates/lintel-crash/src/lib.rs#L82-L92) · [worker 现有报告规则](../../multiprocess.md#服务崩溃报告)
- **CEF。** macOS Crashpad 写完报告后发布到 `pending`，两处已完成目录 `pending` 和 `completed` 共用一个来源键。只读取合法 UUID 命名的普通文件，adapter 在锁定的 CEF 版本上验证完成与迁移规则后才开启。目录迁移、扫描后文件消失和 Crashpad 自身回收均可恢复。CEF 的 `completed` 记录的是 Crashpad 本地处理结果。本插件的服务端回执由自己的队列记录。没有可验证的捕获时版本就留空，符号化按 dump 内模块标识匹配。 [macOS Crashpad 完成路径](https://chromium.googlesource.com/crashpad/crashpad/+/refs/heads/main/client/crash_report_database_mac.mm) · [Lintel 导出用例](../../../src/services/diagnostics.rs#L529-L575) · [Crashpad 报告状态](https://chromium.googlesource.com/crashpad/crashpad/+/refs/heads/main/client/crash_report_database.h)
- **导入。** 从同一个经过路径与权限验证的文件句柄读取并做有界结构检查，包括文件头、流目录与偏移边界。流式写入应用私有目录中的临时文件，同步后在同一文件系统上原子发布完整文件，再登记来源键、报告 ID 和摘要。来源键由显式来源 ID 和后端报告标识组成，数据库约束其唯一。重启时清理未发布的临时文件，按已发布的元数据恢复尚未登记的完整报告。已上传或过期的报告保留来源去重记录，直到原始文件消失或超过导入时限。

应用私有 SQLite 数据库记录策略版本、报告状态、尝试次数、下次发送时间和服务端回执。宿主与 uploader 使用同一套导入逻辑。共用的导入锁保护暂存、配额和登记，发送过程无需持有导入锁。报告及上传意图都提交后，API 才返回已受理。获取发送权后，将上一个进程遗留的 `Uploading` 复原为可重试状态并沿用原报告 ID。

队列配额覆盖暂存文件及本地副本，首版默认单份 64 MiB、总计 256 MiB、保留 7 天，应用可调。扫描只导入捕获时间仍在保留期内的报告。旧格式缺少捕获时间时，使用已完成文件的时间仅作导入保留判断，并保持报告摘要的捕获时间未知。优先回收过期和已上传报告，避开进行中的发送。无法腾出空间时拒绝新入队并暴露原因。捕获目录按各后端规则管理，队列只管理自己导入的副本。

## 上传协议和权限

首版接应用指定的 HTTPS 地址。每个请求用 `multipart/form-data` POST 发送一份报告，字段为 `metadata`（JSON）和 `dump`（minidump）。元数据固定 `schema_version` 为 `1`，允许 `report_id`、应用 ID 与已知版本、来源、报告类型、已知捕获时间、平台与架构，以及可确认的进程角色或服务标识。导入时固定元数据和 dump 的内容，重试沿用同一份数据。

请求带稳定的 `Idempotency-Key`，接收端按应用和报告 ID 幂等。相同 ID 与相同内容返回同一个回执，同 ID 内容冲突返回永久错误。服务端应在持久保存后返回携带 `schema_version`、`report_id` 与 `receipt_id` 的成功 JSON。uploader 验证并持久记录回执后将报告标为 `Uploaded`。HTTPS 验证失败或目标返回重定向时停止这次请求。

暂时网络错误、超时、HTTP `429` 和可重试的 `5xx` 按带抖动的退避策略重试，有效的 `Retry-After` 在配置上限内遵从。认证失败、协议不匹配和达到尝试上限记为 `Failed`。修正凭据或重试耗尽后，手动提交可重置尝试次数，复用原 ID 和幂等键。服务端报告相同 ID 内容冲突时保留永久失败。服务端已经保存但客户端回执丢失时，即使 uploader 随后退出，重试仍应得到同一个回执。

应用决定可读取的目录、接收端和上传策略。接收端可使用应用在 uploader 启动时经继承私有通道传入的专用凭据。凭据只保存在该进程内存，队列、命令行和诊断日志只保留非秘密的配置标识。上传内容限报告所需元数据与 dump，保持诊断包的人工导出另行授权。Minidump 含进程内存的可能性仍然存在，应用和接收端按故障诊断资料管理其访问和留存。 [现有诊断导出边界](../../capabilities.md#cef-子进程崩溃报告)

## 实现边界与交付顺序

1. **先接已有的本地报告。** 增加独立插件、类型化服务与原生报告适配器。完成队列、手动上传、自动上传、策略更新和独立 uploader 的打包。现有 `lintel::crash`、`crash-reporting` 和原生打包入口仍可单独用于本地捕获。
2. **接入非致命快照。** 增加 macOS arm64 一次性捕获路径。在同一签名测试包验证手动抓取、真实崩溃和独立上传。两次连续手动抓取必须产生两个 ID，随后仍可抓取真实崩溃。
3. **纳入已有 CEF 与 Mojo 报告。** 增加显式目录和来源配置，验证 renderer 与 worker 崩溃不会影响宿主上传。按后端与平台分别公布能力。原生 Windows 和 Linux 的手动快照需各自的实机验收。

宿主集成沿用现有 builder 和 `PluginLifecycle`。独立 `lintel-crash-reports` crate 提供队列、策略与 uploader 共用逻辑，应用侧接口通过 `lintel` 公开。现有 `lintel-crash` 保持为本地捕获库，worker 可继续独立使用。 [应用启动流程](../../../src/runtime/mod.rs#L274-L305) · [WebView 启动流程](../../../src/webview_app.rs#L295-L353)

新增 `--crash-uploader PATH` 与 `--snapshot-helper PATH`，将指定的可执行文件按独立角色 `lintel-crash-uploader` 和 `lintel-snapshot-helper` 打包，复用现有架构、依赖和签名校验。捕获后端沿用 `--crash-reporter`、`--cef-crash-reports` 与 `--worker-crash-helper`。手动快照角色只响应已授权的单次请求，可与 CEF 的致命捕获配置并存。公开 feature、builder、打包入口和平台状态落地时，同步更新示例、README、框架目录与 CLI 内嵌副本。新增交付文件进入 CLI 源码归档清单。 [框架目录](../../../lintel.project.json) · [CLI 源码归档清单](../../../crates/lintel-cli/resources/framework-sources.json)

## 验收

| 场景 | 通过条件 |
| --- | --- |
| 启用 `crash-reporting` 的现有应用 | 现有纯本地捕获用例通过。缺少插件配置的应用不会启动 uploader。 |
| 签名的 macOS arm64 测试包 | 同一个隔离测试应用进程连续手动快照两次，继续运行并产生两个有效 ID，Mach port 与文件描述符无持续增长。随后该测试应用进程触发真实崩溃，仍产生独立的致命报告。测试包使用同一次构建的符号还原到预期函数和源码行。 |
| 自动捕获关闭 | 手动快照继续可用。独立测试触发真实异常时，该插件不生成自动报告。 |
| 按模式发送 | `Disabled` 下手动调用返回禁用。`ManualOnly` 只发送手动提交的条目。`Automatic` 按来源和类型过滤，历史报告默认留在本地。切回禁用后没有新的网络请求开始。 |
| 主应用退出、离线与 uploader 被杀 | 宿主意外退出后才落盘的 dump 能在宽限期内被发现并尝试发送。离线或发送中被杀后报告仍可查，下次运行可以补传。新旧进程交接时只有一个发送者。 |
| 回执丢失、并发与接收端变更 | 服务端已收件后断开，自动与手动重试得到同一回执。更换目标后原队列不会向新目标重传。 |
| 部分报告、队列满和多来源 | 原生半成品不入队。CEF 同 UUID 移动只登记一次。尺寸超限和队列满可查询失败原因。CEF 与 worker 的报告保留正确来源，宿主仍可运行。 |

现有原生隔离用例可作为捕获回归。新增用例需覆盖策略与队列的状态转换、可控接收端的回执丢失、进程退出竞态和签名成品的本地捕获。各平台只有在完成对应原生验收后才公布支持。 [现有原生隔离用例](../../../crates/lintel-crash/tests/isolation.rs)
