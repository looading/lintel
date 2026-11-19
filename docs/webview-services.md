# WebView 与独立服务进程

`webview-multiprocess` starter 复用现有 CEF 窗口、JS Bridge 和 Mojo 服务，不要求业务通过全局变量传递服务句柄。模板包含本地页面、Rust 命令、共享 Mojom 协议 crate 和独立 worker。

## 创建与运行

从 Lintel 根目录构建或更新 CLI 后，在 macOS 创建项目：

```sh
lintel new --template webview-multiprocess ../my-web-services
cd ../my-web-services
lintel run --cef-path /trusted/cef143 --mojo-runtime /trusted/mojo/libmojo_rs_native.dylib --sign -
```

CEF 使用框架现有锁定版本的完整分发目录；Mojo 使用匹配 SDK 0.3.0 的可信运行库和随附三个许可声明。CLI 同时构建 UI 和 worker，沿用现有 helper、架构、依赖、签名和原子发布检查。模板不会内置或自动信任任意原生镜像。

## 业务调用路径

页面通过 `lintel.invoke('echo', text)` 调用显式登记的 Rust 命令。新 `async_command_with_context` 接收 `WebviewCommandContext` 和类型化参数，能够查找 `ProcessHost`、`CallContext` 等服务；SDK 结果经原来的命令回复返回页面。

```rust,ignore
.async_command_with_context("echo", |page, text: String| async move {
    let host = page.require_service::<ProcessHost>()?;
    let calls = page.require_service::<CallContext>()?;
    host.wait_ready("echo-worker", Duration::from_secs(5)).await?;
    let connection = host.directory()
        .connect::<dyn Echo>(&key(), 1, Duration::from_secs(3)).await?;
    let client = ServiceClient::bind(host.runtime(), &calls, connection.remote, 16)?;
    Ok(client.call(Duration::from_secs(3), |remote, reply| {
        remote.Ping(text, move |text, pid| reply.respond((text, pid)));
    })?.await?)
})
```

完整源码见 [主程序](../templates/webview-multiprocess/src/main.rs)、[本地页面](../templates/webview-multiprocess/ui/index.html)及[共享 worker](../templates/multiprocess/src/bin/lintel-worker.rs)。协议由两端共享，业务继续实现普通 Rust trait，不维护另一个窗口循环。

## 页面上下文与权限

`command_with_context` 提供同步版本；`async_command_with_context` 仍绑定原来的页面任务域、期限和取消机制。旧 `command`、`async_command` 继续可用。

上下文的 `label()` 和 `window_scope()` 由宿主窗口实例指定，JS 参数不能覆盖。`service::<S>()` 返回可选的 Arc，`require_service::<S>()` 在缺失或失效时返回固定错误。上下文只持有弱服务表和弱页面会话；导航、关闭或旧 generation 后，新的服务查找失效，不延长页面/应用的生命周期。此前交付给可信 Rust 代码的 Arc 不是可撤销权限。

原生主框架加载开始时同步撤销旧文档，不再由延迟的 UI 加载元数据撤销新文档。native ingress 捕获可信 generation，排队后仍携带同一个代号；缺失、过期或在准入间隙被撤销的请求不能 poll 业务 Future。启动 gate 在预约核对及取消 handle 登记后才打开，JS 自报字段不参与判断。

低层 Rust `JsInvocation` 增加 `host_generation` 字段；手工结构体字面量需补字段，或使用默认 legacy None 的 `JsInvocation::new`。普通自定义 handler 继续支持 legacy 调用，strict facade 只接受宿主捕获的代号。JS 协议没有新增字段。

窗口命令授权与进程服务发布/连接授权分别检查。服务查找不会替网页登记命令，也不会使服务拥有 OS 沙箱。业务操作的本地取消、超时和页面关闭不能回滚远端已经发生的副作用。

## 浏览器初始化事务

新 `try_setup` 在插件与服务启动后、默认窗口预约前运行，适合建立浏览器角色专用服务。创建 CallContext 等会启动线程的操作应放在这里，避免 CEF helper 在角色分发前执行。

```rust,ignore
.try_setup(|context, _windows| {
    let calls = Arc::new(CallContext::new()?);
    context.register_service(move || calls.clone());
    Ok(())
})
```

原 `setup` 保持默认窗口预约后的执行顺序；两种 setter 最后一次调用生效。try_setup 显式预约的窗口不会被默认流程重复预约。初始化或部分预约失败时，只撤销已经验证未创建原生窗口/浏览器的预约，保留原错误；真正启动过的资源不能被标记为已关闭。Runtime 在初始化失败后禁止创建新程序的约束继续保留。

## 就绪与重连

`ProcessHost::wait_ready(identity, timeout)` 把订阅、快照检查和 Lagged 后重读封装成一个 Send Future。`wait_ready_after(identity, previous.attempts, timeout)` 等待更大的 attempt，可用于服务崩溃后的显式重连。期限从调用时开始计算，停止、未知/歧义身份和超时返回明确错误。

Ready 只表示所配置的启动条件曾成立；进程随后仍可能退出。已发送 RPC 不自动重放，客户端应丢弃失效代理，等待新的 attempt/generation，再建立连接。

普通页面命令使用 `host.directory().connect_client::<dyn Echo>(&calls, &key(), 1, 16, timeout).await?` 即可取得绑定后的 client 和目录 generation，完整示例已采用此入口。它保留原请求期限和额度，目录锁竞争时立即返回 WouldBlock。`wait_ready` 仍显式调用；具名进程 Ready 与按服务键路由不构成 provider identity 亲和性承诺。

## 长任务、期限和取消

standalone 命令由 `CommandHandler / PendingGuard` 统一管理期限和一次性回复，底层 relay 不再用另一个固定 15 秒计时器提前终止它。宿主继续通过 `command_timeout` 配置正数且不超过 3600 秒的期限；普通低层 widget 保留原有 15 秒保护，只有明确承诺管理所有请求终态的可信 Rust handler 才能在浏览器创建前选择 managed 回复。

页面的 `timeoutMs` 是独立的本地等待上限，不会延长宿主权限。长任务应同时配置 JS 等待、宿主命令期限和所调用服务的 RPC 期限，例如：

```rust,ignore
.command_timeout(Duration::from_secs(60))
```

```javascript
await lintel.invoke('long_task', input, {timeoutMs: 65000, signal});
```

服务代理的 `call` 期限由 Rust 业务明确设置。JS Abort 或本地超时会沿原通道请求取消该 invocation；宿主期限会结束受管 Future 并返回 TIMEOUT。取消是协作式的，不能撤销远端已发生的副作用，也不能中断不让出执行的 Rust/native 代码。同步 handler 仍在执行时继续持有真实并发 slot，不因本地等待结束而假装可再次接入。

## 错误与失败响应

原生失败响应与 renderer 使用相同的 TransferData 编码。页面收到稳定错误码，例如权限拒绝的 `error.code === "DENIED"`；未知错误回退为 COMMAND_FAILED，不包含原始 Debug 错误链。即使底层 failure callback 传入空值，Promise 也明确 reject，不能被当作成功的 undefined。合法 JSON 成功值 null、false、0 和空字符串继续正常返回。

## 发布与重定位

即使不请求 bundle 签名，发布前也执行完整结构与链接验证。CEF 许可声明必须为非空、有界的真实文件，或通过已核验归档取得；失败不会替换已有应用。规范化缓存的 metadata 有大小上限，校验和读取与提取使用同一已打开文件，归档内容不会写回 SDK。

已实际验证预构建程序路径 `bundle --executable --service-process --cef-path --mojo-runtime`：不读取业务 Cargo 配置，生成后移动整个应用包、从 `/tmp` 启动，仍完成 CEF/JS 到 Mojo 的调用。这里“未请求 bundle 签名”不表示输入 Mach-O 或 SDK 自身没有签名；生产签名与公证仍按既有发行入口处理。

## 浏览器与服务分别捕获崩溃

WebView 多进程模板可以同时使用浏览器 CEF Crashpad 和 worker 原生 reporter：构建时启用 `--features service-crash-reports`，打包时显式提供 `--worker-crash-helper PATH --cef-crash-reports`，应用参数分别提供 `--service-crash-directory ABS --cef-crash-directory ABS`。目录按各自进程的规则选择，parent/template 的参数解析只处理数据。

worker helper 的固定角色为 `lintel-worker-crash-helper`，与浏览器的 `lintel-crash-helper` 分开。CEF 浏览器进程仍不能同时安装 Embark reporter；本组合只在独立服务进程内安装它。编译 feature 或复制 helper 都不单独启用捕获。默认模板不产生额外报告目录或 helper。完整配置见[服务崩溃报告](multiprocess.md#服务崩溃报告)。

## 原生验收与边界

macOS arm64 已通过以下联合运行：

- 生成模板的真实 CLI 构建/打包/运行，本地 JS 经 Rust 命令完成独立 PID 的 typed Mojo 调用。
- try_setup 显式预约窗口后返回错误，原错误保留，未创建预约回滚，已启动的服务被回收。
- 两个真实页面的宿主标签、scope 和命令拒绝正确；禁止的 handler 未执行，页面收到 DENIED。
- 关闭并重开 B 时，A 的实例保持不变；B 的旧页面请求 Future 释放且没有完成，重开的页面获得新 scope 和文档。
- 服务崩溃后有限重启，等待新的 attempt，再由页面显式重新连接到更大的 generation；未自动重放 RPC。
- 五次真实合成画面读回，检查页面颜色及内容指纹；Renderer/GPU 保留 CEF seatbelt profile，Renderer 对包外 sentinel 的文件读取被拒绝。
- 应用退出后，本次观测到的浏览器、CEF helper 和 Mojo 服务进程均结束。

联合压力示例使用 Chromium 的测试开关 `--use-mock-keychain`，隔离临时签名应用在系统钥匙串查询中的环境阻塞。该做法用于浏览器测试，见 [Chromium macOS 构建说明](https://chromium.googlesource.com/chromium/src/+/e4ea0f5667f0e83c8d55b583f82c34311a724ad1/docs/mac_build_instructions.md)。公开模板和框架默认值没有加入此开关；这批不提供真实钥匙串或持久 Cookie 加解密的验收结论。

详细日志与机器结果在 `build/mojo-cef/`。压力示例的故障 worker 只保存在验收目录，不进入生成模板。Windows/Linux 原生运行仍未验收，目标检查与 macOS 实机证据分别记录。
