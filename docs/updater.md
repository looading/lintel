# Lintel 自更新

`lintel-updater` 是独立的可选能力，不依赖 iced、CEF 或业务模块。它提供签名更新清单、版本检查、下载进度和整包校验。macOS 使用退出后的 `.app` 原子替换；Windows 已实现当前用户安装目录、稳定启动器、独立更新助手和版本选择切换，但 Windows 原生安装、Registry、ACL、进程等待和启动行为仍需实机验收。检查和下载由应用调用，退出继续走应用自身的正常流程。

原仓库的 `x-lib-updater` 依赖内部原生 SDK，不能直接作为公开发行依赖。本实现保留它的检查、下载、安装阶段划分，复用现有 lintel-build 的签名校验和同卷原子发布，不接入内部更新服务、埋点、账号或配置。

更新清单由固定公钥验证 Ed25519 签名，签名覆盖应用 ID、版本、渠道、目标平台、下载 URL、大小、SHA-256 和有效期。客户端只接受更高版本且匹配当前应用、渠道和平台的清单。生产 URL 使用 HTTPS；本地验收可显式允许数字 loopback HTTP。

macOS 首版使用发布工具生成的 GNU `.tar.gz` 整包，包内只能包含一个 `.app`。解压拒绝路径穿越、外部符号链接、硬链接、PAX、稀疏和特殊条目，并在 tar 解析器之前限制元数据及解压大小；保留 CEF Framework 内部的相对符号链接。解压后检查应用 ID、版本、架构和完整代码签名。

应用调用安装交接接口后仍需正常退出。私有目录在创建时设为 0700，助手验收并返回 READY 后才完成交接。随包的独立 `lintel-updater` 助手等待应用和包内子进程退出，再校验暂存包，并在发布锁内紧邻原子替换前重新检查进程。复制或验签期间应用重新打开时，放弃更新并保留旧包；新程序启动调用失败时恢复并重启旧版。macOS 不承诺新程序启动后发生崩溃的健康检查回滚；Windows 可显式启用下文的业务启动健康确认。

安装交接异步执行，验证助手就绪和等待应用退出分别最多 120 秒。业务应先等待 `install_on_exit` 返回 receipt，再立即发起正常退出，不强制结束应用或 CEF Helper。不请求管理员权限，也不接管只读安装位置。检查和下载与 UI 无关；Windows 实现和模型测试不代表 Windows 平台运行验收，Linux 安装仍不支持。差分更新、自动后台调度和发行服务未实现。Windows 的可选启动确认只覆盖本次受管更新启动，不承担持续运行期间的健康监控。

## 业务接入

生成工程保持最小依赖，只有需要自更新时才增加这一项：

```bash
lintel new my-app
cd my-app
cargo add lintel-updater --path .lintel/framework/crates/lintel-updater
```

业务固定更新源和公钥，应用 ID 与版本从当前 `.app` 或 Windows 受管版本目录读取。普通 Windows portable 目录尚未进入受管安装布局时，`for_current_app`/`prepare` 会明确拒绝安装操作。此代码应在 UI 的异步任务中执行：

```rust
let updater = lintel_updater::Updater::for_current_app(
  "https://updates.example.com/stable/aarch64-apple-darwin/latest.json",
  include_str!("../updater.pub"),
)?;

if let Some(update) = updater.check().await? {
  // UI 可以先展示 update.release().notes，等待用户确认。
  let downloaded = update.download(|progress| {
    println!("{} / {}", progress.downloaded, progress.total);
  }).await?;
  let prepared = downloaded.prepare().await?;
  let receipt = prepared.install_on_exit(true).await?;
  // 此处调用应用原有正常退出接口；例如 ctx.quit()?。
  // receipt.result_path 是结果文件，receipt.installer_pid 可用于外部验收。
}
```

`check()` 对同版、旧版和仅 build metadata 不同的版本返回 `None`。网络、签名、身份、过期和内容校验错误返回 `Err`。丢弃尚未调用交接接口的下载/准备对象会清理临时文件。`install_on_exit` 的握手运行在 blocking worker 上，启动后丢弃外层 Future 不等同于取消握手；只应在用户已确认安装后调用，并等待结果后再退出。交接成功后由助手负责缓存和结果文件；`receipt.read_result()` 可读取此次尝试的结果，`success=false` 时 `version` 是未安装成功的目标版本。

完整原生 UI 示例在 `templates/updater/native.rs`。复制到新工程的 `src/main.rs`，修改开头两个常量即可。按钮通过 `Task::perform` 执行异步更新；交接成功后关闭窗口，由现有运行时自然退出。

```bash
cp .lintel/framework/templates/updater/native.rs src/main.rs
lintel bundle --updater --sign -
```

macOS 的 `--updater` 会从已安装 `lintel` 的同一目录找到独立助手，并加入 `Contents/Helpers/lintel-updater`。这条命令仍是 macOS bundle 操作。Windows 使用下面的 `windows install`，把独立工具安装到版本目录之外；不能据此声称普通 portable 包自动具备更新能力。安装 CLI 时应构建/安装所有 binaries，包括 `lintel-updater`、`lintel-launcher` 和可选崩溃助手。

## Windows 受管安装与更新

初装是用户明确选择本地 package 的动作，并显式固定后续更新公钥。它验证 portable manifest、文件哈希、PE 机器类型和依赖，但不宣称初装包经过 Authenticode 验签。私钥只保留在发行端，应用与安装器使用同一个公钥。

```powershell
# 在具有 MSVC SDK/linker 的 Windows 环境构建，工具需静态 CRT 或只有系统 DLL 依赖。
$env:RUSTFLAGS = '-Ctarget-feature=+crt-static'
cargo build --manifest-path crates/lintel-cli/Cargo.toml --release --bins
lintel windows install 'build/windows/Example-1.2.0-x86_64' `
  --public-key updater.pub --scheme example
```

默认目录是 `%LOCALAPPDATA%/LintelApplications/<app-id>`；`--root` 只接受当前用户 LocalAppData 内的新本地目录。普通/extended 路径和大小写变体先正规化最近已存在父目录，再做 Windows ordinal 不区分大小写的边界检查。LocalAppData 以下每一级已有父目录都必须私有且不能是重解析点；共享父级会被拒绝，不修改原 ACL。新建目录和复制文件使用当前用户的 protected DACL。

稳定的 `lintel-launcher.exe` 位于安装根目录，`tools/lintel-updater.exe` 留在版本目录之外。应用进入 `versions/<name>-<version>-<arch>/`，根目录 `active.json` 只记录单个受控相对目录和 manifest SHA-256。启动器锁内验证选择记录及整包后用 `Command` 启动，不通过命令 shell。协议关联指向稳定启动器，因此版本升级无需改写 Registry。`--lintel-protocol` 模式严格接受一个已批准 URL，拒绝额外 argv、引号、控制字符和其他 scheme。

更新客户端保留已签名 envelope。独立助手从自身受管路径确定安装根，重新验证固定公钥、app ID、target、channel、版本与有效期，并核对归档大小和 SHA-256。归档路径在所有宿主上拒绝 drive、UNC、backslash、ADS 冒号、设备名、大小写冲突、符号链接及其他特殊条目。解压对实际读入的压缩字节再次计算哈希；提取后的 manifest 摘要保存在内存中，等待期间即使改写文件和自洽 manifest，也不能成为被选择的候选版本。

助手在 READY 前保留父进程 handle，等待它及旧版本目录内的应用/助手退出，不按可复用 PID 盲等，也不终止应用。重复交接由非阻塞独占 lease 拒绝。切换前在安装锁内重读 active、检查进程是否重开、检查签名内容及有效期，并核对发布后的 manifest 仍等于最初认证快照。新版本发布为不可变目录，`active.json` 再以文件替换切换；旧 exe 和旧目录始终保留。

立即启动失败时只在 active 仍等于本次候选的条件下恢复旧记录，防止撤回另一个已成功的选择。journal 写入后或 active 切换后中断，若 active 是完整有效的旧/新记录，启动器可保留该记录并清理 journal。active 缺失、损坏或与 journal 不一致时会保守报错，不会自动猜测覆盖；这不等于覆盖所有掉电恢复。默认仍以成功创建新进程作为交接结束，不增加业务握手。需要业务启动确认时按下节显式启用；确认完成后的运行崩溃不触发自动回滚。旧版本按需调用维护命令回收，没有后台自动清理。安装位置不可写、空间不足或同名进程无法安全核查时会拒绝更新。

`--channel beta` 等非 stable 安装应与业务客户端的 `.channel("beta")` 匹配。更新不会自动轮换公钥或扩大安装时批准的协议范围。协议撤销使用受管根目录的 `protocol-ownership.json`，详见 [windows-bundles.md](windows-bundles.md)。这些状态文件和工具依赖当前用户目录及 ACL 信任；它们不隔离已经能以同一用户修改应用文件的恶意进程。

## Windows 可选启动健康确认

既有应用默认不启用握手，不改变普通 stdin/stdout。候选版本必须已接入业务就绪回调，发行方才应为这次更新选择以下契约：

```rust,ignore
use std::time::Duration;
let prepared = downloaded.prepare().await?
  .with_windows_startup_health(lintel_updater::WindowsStartupHealth::new(
    Duration::from_secs(30),
    Duration::from_secs(2),
  )?)?;
let receipt = prepared.install_on_exit(true).await?;
// 等交接返回后，走应用正常退出流程。
```

`with_windows_startup_health` 只接受 Windows 受管更新，超时范围是 100 毫秒至 5 分钟，最短存活时间必须小于超时。启用后必须 `relaunch=true`。它授权助手在失败或超时时终止**本次由助手直接创建并保留 Child handle 的候选进程**，随后尝试安全回滚；不会按保存的 PID 打开并终止其他进程。

在候选应用已经完成自身初始化、可以提供业务服务的位置调用：

```rust,ignore
// 放在业务 boot/setup/已就绪回调中；CEF 应先完成其子进程分发。
let confirmed = lintel_updater::confirm_windows_startup()?;
```

这个接口不会替业务定义“健康”。普通启动没有助手握手标记时立即返回 `false`，不会读取 stdin；带 CEF `--type` 的子进程同样直接返回，不能替 browser 读取挑战。不要在 `webview_app.run()` 前无条件等待启动输入。`LINTEL_UPDATE_HEALTH` 是助手内部标记，业务不应自行设置。仅显式启用的候选启动使用专用 stdin/stdout 管道，不适合同时自行消费这些标准输入输出的应用。

助手生成随机挑战，经新子进程独有的输入管道发送，将响应与本次挑战、实际 PID、进程创建时间和候选 manifest 摘要逐项比较。响应前退出（即使 exit 0）、旧版本响应、PID 复用、其他尝试的响应和重复响应都不能确认健康。助手只扫描前 64 KiB 输出，每行最多 4 KiB；确认后仍要求进程达到最短存活时间。SDK 读取挑战最多等待 5 秒。就绪前大量或不换行的 stdout 日志会使本次尝试失败，业务可改用自己的日志文件。

超时后，助手只结束自己的候选 Child；若该版本目录仍有 renderer/helper 运行，就保留未确认 journal，并拒绝新的启动或更新。待它们正常退出后，稳定启动器可在独占 lease 和安装锁内恢复旧选择。恢复只在 active 仍是本次旧/新记录时执行，不覆盖另一个版本。助手崩溃后的恢复也不会用持久化 PID 杀进程。已持久化确认的候选保留为活动版本，旧版记入 `rollback.json`。缺失或损坏的 active/journal 继续保守报错，不承诺任意掉电恢复。

确认成功后，助手先提交结果、释放安装锁和生命周期 lease，再继续排空候选 stdout 并等待该进程退出，避免管道阻塞；这段等待不拦截后续更新。这个可选契约需要匹配版本的 SDK、launcher 和 updater 工具。旧助手对新增请求字段会明确拒绝，不能静默降级为“已健康”。Windows 原生管道、终止精确 Child、最短存活及重启恢复仍待实机验收。

## Windows 回收与卸载

安装器把用户明确选择的初装版本记入 `managed-versions.json`；独立更新助手只在验签、解压和固定 manifest 快照发布完成后登记后续版本，并保存签名 envelope。过期的历史 envelope 可用于证明既有版本的所有权，不能授权新的安装。没有 ledger 的旧安装只认当前已验证的 active，其他目录不会被自动收编。

```powershell
lintel windows inspect 'C:\Users\me\AppData\Local\LintelApplications\dev.example.app' --app-id dev.example.app
lintel windows prune 'C:\Users\me\AppData\Local\LintelApplications\dev.example.app' --app-id dev.example.app --keep-recent 2 --dry-run
lintel windows uninstall 'C:\Users\me\AppData\Local\LintelApplications\dev.example.app' --app-id dev.example.app --dry-run
# 核对 JSON 后去掉 --dry-run 执行。
```

`prune`/`uninstall` 必须从安装根之外的维护程序运行，并先关闭该安装的 app、launcher 和助手。启动器、更新器和维护命令共用非阻塞生命周期 lease，进入文件修改时再取得安装锁，防止协议启动、多次更新和回收相互交错。`inspect` 是只读快照，不代表文件未被进程使用。

回收始终保留 active、rollback、pending journal 的旧/新版本和指定数量的最近登记版本；未登记、签名/哈希不匹配、修改过或正在使用的目录也会保留。Windows 路径重新检查私有 ACL 和重解析点。原生删除先通过 Restart Manager 查询使用者（不调用关闭进程接口），再独占打开待删文件、重验哈希，并通过同一 DELETE handle 要求映像区检查后标记删除。不能可靠预留、检查或删除时报告保留，不采用 POSIX 强删或重启后删除。

卸载只删除通过所有权验证的版本和哈希仍匹配的稳定工具。协议 receipt 必须同时匹配此安装的 app ID、启动器路径和 scheme 集合，Registry 内仍需完整树及 owner token 匹配；已被别的应用更改的关联保留。业务数据、未知目录、未知工具和改动过的文件都保留。卸载写 `uninstall.json` 使启动器拒绝再启动；安装根、元数据、receipt、锁和少量审计记录刻意保留，不递归删除根目录。发生占用或部分删除失败时，JSON 会列出保留项，应用仍处于卸载状态，释放占用后可重新执行。这是显式的用户级维护 API，不是 MSI/NSIS 安装器。

## 发布更新

密钥生成和更新产物生成均为本地操作，不上传文件：

```bash
lintel updater keygen --private-key release.key --public-key updater.pub

lintel bundle --updater --sign 'Developer ID Application: Company (TEAMID)'
# 正式发行按已有流程完成公证和票据验收，再生成更新产物。
lintel updater release 'build/macos/My App.app' \
  --url https://updates.example.com/stable/aarch64-apple-darwin/update.tar.gz \
  --target aarch64-apple-darwin \
  --private-key release.key --output-dir dist/2.0.0
```

输出 `update.tar.gz` 和已签名的 `latest.json`，可以放在普通静态文件服务或 CDN。每个平台/渠道使用独立清单；`--target` 必须出现在应用实际架构中，macOS 默认使用发布工具的编译目标，Windows 默认见下文。默认清单有效期 7 天，可通过 `--expires-in` 指定秒数，过期后需重新签发。密钥生成不覆盖已有文件；客户端嵌入公钥，私钥只留在发行环境。

Windows release 可以从已经验证的 portable 目录生成；不要求在发布宿主执行 Windows exe。默认 target 取 portable manifest，显式 `--target` 必须与之完全匹配。发布时先固定并复制已验证 manifest 快照，再归档和签名。

```sh
lintel updater release build/windows/Example-2.0.0-x86_64 \
  --url https://updates.example.com/stable/x86_64-pc-windows-msvc/update.tar.gz \
  --target x86_64-pc-windows-msvc --private-key release.key --output-dir dist/windows-2.0.0
```

Windows HTTPS 使用系统 Schannel，非 Windows 保留 rustls。证书验证、重定向、Ed25519、大小/哈希和过期策略不因后端变化而放宽。

本地验收可使用 ad-hoc 签名包和数字 loopback HTTP；需要显式调用 `allow_local_http_for_testing()`。生产默认只允许 HTTPS，HTTP 重定向也受同一策略限制。Ed25519 更新签名与 Apple 应用签名分别检查，真实 Developer ID、公证及票据在更新归档中的发行验收仍留待证书配置完成后进行。

## 实施与验收

先实现独立 crate、签名清单与受限下载/解压；再复用打包器安装原语，实现退出后助手；随后接入 CLI 的密钥生成、更新包签名和助手打包；最后用本地更新源验证两个版本的实际升级、错误签名/哈希拒绝、版本拒绝及旧包保护。真实发行证书和公证继续按已有约定独立验收。

Windows 的当前证据分开记录：macOS 上的 PE/COFF fixture 验证签名/哈希、解压边界、active/journal、并发选择保护、模拟 CreateProcess 失败回滚和协议 argv；MSVC 目标交叉检查验证 Windows 分支类型和 API。真实 Registry 事务提交/撤销、ACL、普通/extended 路径、进程 handle/wait、launcher、Windows 文件替换仍需 Windows 实机验收。不能把模型或 `cargo check` 当作原生运行通过；也不宣称已有 MSI/NSIS 或 Windows CEF sandbox/bootstrap 发行支持。此前发行证据保存在 `build/checklist-followup/`，本轮回收、卸载、健康确认与公开 CEF ABI 的模型/交叉检查保存在 `build/checklist-platform/`。
