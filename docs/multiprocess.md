# 多进程服务设计与接入

Lintel 通过可选 `multiprocess` feature 集成 [mojo-ipc](https://github.com/looading/mojo-ipc) 的匹配版本 `0.3.0`，复用其 `mojo-service` 服务目录、权限、调用和进程监管。实现和验收记录位于独立 Lintel 项目，不改动原业务仓库。

## 进程与职责

主进程运行既有 winit/iced/CEF 窗口路径，并作为 Mojo broker 管理服务进程。独立服务可执行文件作为 participant，保留 SDK 原生 dispatch 线程。窗口业务仍通过普通 Message/Task 消费结果，服务方法不操作 UI 对象。

框架将 ProcessSupervisor 放在独立 OS 线程，避免其阻塞式 bootstrap、健康检查和回收阻塞 UI 或 Tokio worker。宿主只向受限队列提交命令，保留有界状态/事件；结束时停止重启、关闭 admission、给予退出宽限并回收直接子进程。不会自动重放可能已执行的业务请求。

CEF renderer/GPU/helper 继续由 CEF 管理；新增业务服务进程不复用 CEF bootstrap，也不改变 CEF 的沙箱角色。

## 轻量业务接入

业务将共享 `.mojom` 放入一个协议 crate，由主进程与服务进程共同使用。协议 crate 固定 `mojo-ipc = "=0.3.0"` 和 `mojom-build = "=0.3.0"`，由上游适配生成器生成 Rust Remote/Receiver。应用通过 `lintel::process` 使用框架封装与 SDK 类型。协议生成需要 Python 3.10+；生成器与匹配 SDK 都固定为 `0.3.0`。需要独立、无 GUI 依赖的服务程序时，可直接消费公开的 `lintel-process` crate；内置 starter 为减少工程配置，把 UI 与 worker bin 放在同一 Cargo package，因此共享 Cargo features。

宿主声明受信任运行库、服务可执行文件、发布/连接权限、就绪要求和有限重启策略，再用 builder `.processes(config)` 装配。`application_with_context` 可取得已注册的 ProcessHost，在 Task 中连接服务并调用；服务进程使用 ProcessWorker 接入监管并发布 typed factory。

服务发现与调用权限默认拒绝。就绪需要指定服务键和协议版本实际注册；只有进程存活或 endpoint 已路由不代表业务完成。断连或重启后业务显式重新连接新的 generation。调用的本地取消/超时不撤销远端已经发生的副作用。

业务取得 `ProcessHost` 后，可以直接连接并绑定 typed client：

```rust,ignore
let connection = host.directory()
    .connect_client::<dyn Echo>(&calls, &key, 1, 16, Duration::from_secs(3)).await?;
let generation = connection.generation;
let (text, pid) = connection.client.call(Duration::from_secs(2), |remote, reply| {
    remote.Ping("hello".into(), move |text, pid| {
        reply.respond((text, pid));
    });
})?.await?;
```

`ServiceClient / ServiceCall` 保留底层代理的真实生命周期，让完整调用 Future 能跨 iced `Task` 使用，不要求业务增加 `unsafe Send`。共享或重入代理访问立即返回 `WouldBlock`。低层上游 `CallClient` 继续导出，普通 UI 接入使用框架适配器。完整业务代码见 [UI 入口](../templates/multiprocess/src/main.rs)、[服务入口](../templates/multiprocess/src/bin/lintel-worker.rs)和[共享协议](../templates/multiprocess/protocol/schema/echo.mojom)。

`connect_client` 返回 `ConnectedService { client, generation }`，使用原目录请求的提交时刻、期限和回复额度，不增加另一层 timer。容量为零或 CallContext 已关闭时，在提交前拒绝；绑定失败保留原 `CallError`，可从 `io::Error` 中检查原因。目录锁竞争立即返回 `WouldBlock`，不阻塞 UI，也不占用未提交额度。需要低层端点时，原 `connect` 和 `ServiceClient::bind` 仍可单独使用。

`DirectoryRequest` 在取得 Future 时就提交控制请求，期限从提交时计算；即使未 poll，也可能已经开始服务路由。丢弃 Future 只结束本地等待。已提交控制请求的额度直到回调或断连才释放；`ServiceCall` 的本地超时同样不会释放尚未回复的线上响应槽，达到额度后需等待回复或明确关闭/重建代理。不会自动重放请求。

每个 Host 生命周期最多累计登记 16 份进程 spec，自动重启不增加登记数。默认命令队列 32、事件队列 128、目录待回复额度 32；事件接收者落后会收到显式 Lagged，业务应重新读取快照。worker 最多保留 64 个 factory。

`wait_ready(identity, timeout)` 封装订阅、快照与期限；`wait_ready_after(identity, previous.attempts, timeout)` 等待新的启动 attempt，业务仍显式重新连接。WebView 可通过带页面上下文的命令取得服务，见 [WebView 服务接入](webview-services.md)。

`wait_ready` 确认具名进程的状态，目录连接按服务键和版本选择 provider。两者顺序调用不构成 provider 与该进程 identity 绑定的承诺。业务可以保留连接结果的 generation，在断连后显式建立新连接；框架不会自动重放请求。

## 构建与发行

服务在 Cargo 中声明为独立 `[[bin]]`，并在 `[[package.metadata.lintel.service_processes]]` 登记逻辑名称与 bin。多 bin 项目用 `default-run` 明确 UI 主程序。`lintel build/run/bundle` 使用相同 target、profile、features 构建登记的服务，复用已有 helper 打包、架构/依赖校验和签名流程。

Cargo 构建入口在编译前统一检查服务声明形状、全部服务 bin 是否存在，以及主程序是否被重复声明为服务，后面的服务写错也会在主程序的 build script 执行前报错。直接使用构建库时，`build_with_service_processes(&options)` 返回主程序与服务 helper；原 `build` 继续只构建所选目标。Cargo 的 feature 激活和目标编译仍由 Cargo 判断。预构建 `bundle --executable` 路径不执行这项 Cargo 检查。

生成和运行：

```bash
lintel new --template multiprocess ../my-services
cd ../my-services
lintel build
lintel run --mojo-runtime /trusted/mojo-native/libmojo_rs_native.dylib
lintel bundle --mojo-runtime /trusted/mojo-native/libmojo_rs_native.dylib
```

模板设置 `default-run`，并生成以下服务登记：

```toml
[[package.metadata.lintel.service_processes]]
name = "echo-worker"
bin = "lintel-worker"
```

两个多进程模板预留应用名称 `lintel-worker` 和 `lintel-app-protocol`，分别避免与固定 worker 目标及共享协议 package 冲突。生成器会在发布目标目录前拒绝这两个名字，既有空目录保持为空；其他模板的名称规则不受影响。

可用 `package.metadata.lintel.mojo_runtime` 配置相对 package 的运行库路径，CLI 显式参数优先。已有二进制也可用 `bundle --executable ... --service-process echo-worker=/absolute/worker --mojo-runtime ...`；这条路径不替业务编译外部 package。

Mojo 原生库通过显式 `--mojo-runtime PATH` 或项目 metadata 提供。macOS 运行库进入 Contents/Frameworks、服务进入 Contents/Helpers；Windows 运行库与服务程序都在 bundle 根。运行时解析包内路径，不依赖开发机绝对路径。

运行库旁必须提供非空真实文件 `LICENSE`、`CHROMIUM_LICENSE` 和 `THIRD_PARTY_NOTICES.txt`，每份最多 16 MiB。macOS 复制到 `Contents/Resources/licenses/mojo-ipc`；Windows 保持原 portable 平面布局，使用固定的三个 `mojo-*` 声明文件，并纳入 manifest、复制/安装和签名分发校验。

上游 macOS 库的精确 self-ID `./libmojo_rs_native.dylib` 仅在显式匹配的 Mojo 运行库中按固定目标安装，保留原始字节；实际相对 `LC_LOAD_*` 依赖仍被既有依赖图拒绝，不放宽通用 native-library 校验。

CEF 的 `LICENSE.txt` 与 `CREDITS.html` 同样是发布必需内容，直接输入必须为非空真实文件且每份不超过 64 MiB；规范化 SDK 缓存可以使用校验和匹配的同一已打开归档回退。缺失或无效内容在发布前失败。无论是否签名，应用都先验证完整资源链接结构，拒绝包外或绝对资源链接；合法 Framework version 相对链接继续支持。

运行库必须来自可信且与 SDK/Chromium pin、ABI、OS、架构匹配的构建。提供路径、哈希或版本检查不能将任意本机代码变成可信代码；公开加载 API 保留 unsafe 信任前提。不会默认下载或执行来自工作目录/环境变量的运行库。Mojo 自身保留到进程退出，业务服务生命周期结束不代表能卸载 Core 或切换 broker/participant 角色。

## 服务崩溃报告

两个多进程模板提供默认关闭的 `service-crash-reports` Cargo feature。编译该 feature 后，显式传入绝对 `--service-crash-directory` 才启用 worker reporter；没有目录时继续普通 worker 路径。主程序只解析、校验和传递路径数据，实际捕获在服务进程内安装。

```bash
lintel run --features service-crash-reports \
  --worker-crash-helper /trusted/lintel-crash-helper \
  --mojo-runtime /trusted/libmojo_rs_native.dylib --sign - -- \
  --service-crash-directory /absolute/app-reports/services
```

CLI 将指定 helper 以独立角色 `lintel-worker-crash-helper` 打包，沿用架构、依赖、签名和原子发布检查。该参数只负责打包，不会为主进程安装 reporter。它可以与 CEF 的 `--cef-crash-reports` 组合；原浏览器 reporter 的 `--crash-helper/--crash-reporter` 与 CEF Crashpad 互斥规则继续保留。

自行接入时，使用 `ProcessWorker::connect_with_crash_reporting(library, identity, CrashConfig)`。根 crate 同时启用 `multiprocess,crash-reporting`，独立 `lintel-process` 使用其 `crash-reporting` feature。返回的 RAII wrapper 可直接调用原有 `publish/wait` 接口，原 `ProcessWorker` 不增加 reporter 字段。捕获安装在 Mojo 原生库加载前，失败会清理已经创建的 reporter/helper。

`report.json` 保留应用 ID、版本、PID、系统及架构，服务报告额外包含 `process_role=service` 和 `service.identity/attempt`。identity 是应用声明的关联标签，attempt 从 SDK 的监管启动上下文读取；没有启动时未知的 generation，也不记录原始环境、参数、端点或请求。诊断导出会保留这份 metadata，并继续受显式目录授权、记录数和字节预算约束。

正常退出清理与父进程异常退出分别处理。SDK 的父存活 watcher 在 EOF 后直接退出 worker，跳过 Rust Drop；crash helper 通过自己的控制管道 EOF 退出。该路径不应被算作生成了崩溃 dump，可能留下没有 completed/dump 的 metadata 目录。上传、符号化、强制终止/OOM 捕获不由本接口承诺。

## 验收结果与边界

macOS arm64 使用已核验 `v0.3.0` 发布校验和的上游运行库，完成：

- 真实 `lintel build/run/bundle`、UI 与不同 PID 服务的 typed Echo 调用、正常退出清理。
- 应用整体移动并从 `/tmp` 工作目录启动，服务和运行库仍从包内解析；严格签名检查通过，三个声明文件保存正确。
- 初始化主动失败时，已经启动的服务被回收，原错误保留。
- 服务崩溃后有限重启，发现新 generation 并显式重连；调用没有自动重放。
- 无响应调用超时仍保留线上响应额度，饱和返回 AtCapacity；动态 Add/Stop 回复时快照已更新；应用返回前所有直接子进程已回收。
- 仅强制终止 broker，worker 自动退出，没有通过进程组终止来替代该验收。

首批源码回归包括运行时 18 项测试、受影响根库 109 项测试、构建包 92 项通过/1 项原有 ignored、CLI 27 项，以及真实 Mach-O self-ID/绝对 dlopen 回归。后续就绪等待增加到 26 项运行时测试，WebView 页面/启动/JS 合同共 20 项、原生错误编码 2 项、生成器 10 项。定向测试之间可能重叠，不能直接相加。最终命令、日志和机器结果见 `build/mojo-integration/`。

Windows/Linux 完成相关目标代码检查，未在真实桌面机器运行。Linux 的进程 API 可独立使用，当前 CLI 的应用 bundle 仍按既有 macOS/Windows 平台范围提供。macOS 已完成 CEF 与 Mojo 联合原生验收，包括多窗口、页面取消、服务重启重连、合成回读与子进程回收；测试用钥匙串隔离及未覆盖范围见 [联合接入说明](webview-services.md)。

多进程隔离不等于操作系统权限沙箱。首版不提供远程网络服务、任意外部进程接管、热替换协议、自动业务重放或孙进程监管。原生 Core 保留到进程退出，服务关闭不能卸载它或切换角色。普通开发不启用 AI Dev Loop。
