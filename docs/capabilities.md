# Lintel 基础能力与接入

Lintel 继续使用迁出的运行时、窗口路由、插件管理和 CEF 绘制实现。新增接口把窗口、后台任务、系统服务和发行工具接到同一生命周期。

框架按能力提供可选模块，原生对象由主线程持有，业务通过句柄、任务和类型化消息调用。平台支持与实际系统授权分别查询，测试和实机验收结果保存在本地构建目录。

## 业务入口

原生应用继续使用 `application(boot, update, view)`。多窗口应用使用 `multi_window(boot, update, view)`，启动回调接收 `WindowManager`，视图回调接收 `WindowId`。开关窗口通过句柄操作，业务消息和异步任务继续使用 iced 的 `Message` 与 `Task`。

```rust
lintel::application(State::default, update, view)
    .id("dev.example.notes")
    .title("Notes")
    .desktop()
    .single_instance()
    .protocol_schemes(["notes"])
    .on_activation(Message::Activation)
    .on_desktop_event(Message::Desktop)
    .run()
```

桌面服务通过可选的 `desktop` feature 编译。`services` 提供配置、文件、网络和诊断插件，`sqlite` 增加 SQLite，`crash-reporting` 增加原生崩溃捕获，`agent` 增加应用授权的工具执行。已有原生与混合模板保持最小的默认运行路径。

需要在启动状态中取得服务时，使用 `application_with_context` 或 `multi_window_with_context`。初始化接收已经完成插件启动的 AppContext，可返回错误；成功后才提交程序，失败沿原有清理路径返回原错误。旧入口继续保留。详见[上下文启动](context-initialization.md)。

## 能力与接口

| 能力 | 公共入口与实现 | 验收方式 |
|---|---|---|
| 应用生命周期 | 启动、退出、清理后重启；单实例转发参数、cwd 和 URL；macOS 提供原生 URL 与类型化 Reopen。 | 首次/第二实例、冷/热 URL，以及可见/隐藏窗口的真实 reopen AppleEvent。 |
| 窗口管理 | 打开、关闭、聚焦、显隐、移动、缩放、最小化、最大化和标题；`with_native_window` 将真实 owner 交给原生集成，关闭策略可拒绝。 | 独立状态/画面、关闭确认、重开、最后窗口退出、队列满、失效句柄及 owner Task 实跑。 |
| 输入与无障碍 | 沿用输入/IME/DPI/拖拽；`.accessibility()` 与 `a11y` helpers、`semantic` 自定义包装器发布真实布局、角色、值和动作，支持焦点与 Tab。 | macOS 冷启动自定义控件的 AX 点击、正反 Tab、键盘/鼠标、禁用、输入恢复和安全输入已实测；另有动作撤销、稳定节点与事件批次回归。 |
| 任务和资源 | `TaskScope::{Application, Window, Page}`、`spawn_task`、取消句柄、`wait_scope`。关闭窗口包含其页面任务，关闭应用等待受管 Future 析构。 | 取消、重复退出、启动回滚、句柄丢弃后的归属及析构回归。 |
| WebView | 多本地页面共享 CEF，实例/来源策略/桥接会话独立；`reopen` 创建新实例，`reload` 刷新，退出先排空浏览器。 | 真实 renderer 崩溃、宿主存活、reload 新文档/画面、旧请求取消、全部关闭后重开与 Helper 回收。 |
| 命令和事件 | `command` 与 `async_command` 保留类型编解码。JS 支持 AbortSignal、超时和页面关闭取消。会话有 64 个请求槽，旧请求清理按唯一身份匹配。 | 同步/异步往返、无效类型、重名、取消、重用请求 ID、导航与 JS 测试。 |
| 系统交互 | 主线程有界队列；`Pending` 可 await；菜单、托盘、快捷键、剪贴板、对话框、通知和 opener。Windows 菜单用 scoped lease 绑定窗口，应用内菜单使用 `menu::menu_bar` 并自动提供原生语义。 | macOS 原生菜单/文件选择、托盘点击事件，以及应用内菜单 AX 选择、勾选/禁用、深层/长菜单和正常退出实测；另有事件批次回归与跨平台源码检查。 |
| 持久化 | 原 Store schema/迁移错误传播；SQLite 串行访问和事务迁移；Keychain、Credential Manager、Linux Secret Service 凭据后端。 | schema、失败保留数据、数据库事务、临时 Keychain roundtrip；Linux 会话仍待验收。 |
| 网络与下载 | `NetworkClient` 提供请求来源约束、代理、超时、有界重试、大小限制和进度。先校验 SHA-256 再替换目标，取消清理临时文件。 | 本地 HTTP 夹具验证跳转拒绝、哈希错误、超限、取消、IPv6 loopback 显式授权。 |
| 诊断与崩溃 | 轮转日志、字段脱敏、UI 心跳、显式导出；原生 Embark 与 CEF Crashpad 两个可选后端，CEF 沙箱子进程由官方 handler 捕获。 | 原生 abort、真实 renderer MDMP、CEF dump 导出、私有目录、记录/字节预算及正常退出。 |
| 权限 | 窗口主体命令授权、文件目录 capability、`NetworkClient` origin 约束；四类系统权限共用 `support/status/request`。 | 跨主体/主 frame/大小检查、符号链接边界、导航撤销；四项系统状态被动真机查询，申请由业务显式触发。 |
| 插件 | `on_start` 可失败并兼容旧 `on_init`，启动前检查名称，回滚和停止只执行一次。回调在管理锁外运行，插件可以安全查询其他服务。 | 重名、部分启动、失败回滚、重复和并发关闭、服务 getter 重入测试。 |
| 多进程业务服务 | 可选 `multiprocess`，复用 Mojo SDK 0.3.0：typed 服务目录/调用、专用监管线程、默认拒绝授权、就绪/有限重启/健康策略、退出回收；CLI 构建并打包声明的服务与显式原生库。 | 默认 31 项、启用服务 crash reporting 34 项运行时回归；typed connect+bind 保留 generation，目录竞争立即拒绝，构建前检查所有服务声明。macOS 实机验证原生与 CEF/JS 联合运行、多窗口取消重开、重启重连、5 次合成回读及完整回收。首批重定位、超时额度、动态 Add-Stop 与父进程异常退出证据保留。Windows/Linux 仅目标检查；压力示例隔离系统钥匙串。独立服务不等于 OS 沙箱，CEF 子进程继续由 CEF 管理。 |
| AI 工具执行 | 可选 `agent`：typed ToolRegistry、默认拒绝/显式 grants、policy-filtered catalog、受管 ToolSession/Invocation、数字进度、超时和取消。 | 17 项定向回归；外部单依赖 macOS 应用验证工具、Store 持久化和退出/初始化失败清理。没有模型 provider 或远程 transport 验收结论。 |
| AI 开发协作 | AGENTS、短 llms 索引、同源 lintel.project.json；`lintel describe --json` 只读发现，new 生成消费项目指南。 | CLI 24 项、生成器 8 项检查，真实只读 cwd/环境 sentinel/子进程未启动测试与三模板指南检查。 |
| 发行 | CLI 生成/构建/运行/打包、第三方库、签名、公证、更新；Windows 当前用户安装、稳定启动器、协议 ownership receipt 和签名版本切换。 | macOS 成品/签名实跑；Windows PE、事务/更新模型与交叉检查；Windows 原生执行仍需目标环境。 |

## 原生服务接入

启用 `.desktop()` 后，多窗口 boot 中的 `windows.desktop()` 可取得服务。`desktop.create_menu(spec)?.await`、`desktop.show_file_dialog(...)?.await` 等操作放进 iced Task，再将结果映射为业务消息。`wait` 和 `wait_timeout` 保留给同步 worker，主线程不能阻塞等待。

Windows 在窗口 Opened 后调用 `windows.bind_menu(id, menu)`，获得返回 scoped lease 的 Task。业务把 lease 存在窗口状态中，在 iced 逻辑 `WindowEvent::Closed` 时移除；最后一个 lease 释放会唤醒主线程拆菜单并释放 owner，取消未完成的绑定也会清理。清理不依赖命令队列还有空位。不要等待依赖最后 owner 释放的 Win32 Destroyed 事件。更新已绑定菜单复用 `lease.target()`，同一窗口的重复注册会被拒绝。

macOS 使用全局 `set_application_menu`。完整业务示例见 [desktop_window.rs](../examples/desktop_window.rs)，额外原生集成见 [native_window.rs](../examples/native_window.rs)。高层 Windows `.desktop()` 入口会安装 Win32 菜单 accelerator hook；独立 winit 宿主使用 `WindowsMenuMessageHook` 接入自己的消息循环。

系统权限先用 `permissions::support(kind)` 查询平台能力，再用 `status(kind, &context).await` 读取当前状态。`request` 由明确的用户操作触发；通知 completion 可返回决定，屏幕/输入/AX 的 prompt 提交后仍需后续查询。Linux portal/session 授权、Windows toast 申请没有通用等价接口时，返回明确的 unsupported。详见 [desktop 包说明](../crates/lintel-desktop/README.md)。

## 原生控件语义

在原生 builder 上启用 `.accessibility()`，并用 `lintel::a11y` 的按钮、复选框、文本/安全输入 helper。每个 helper 接收窗口内唯一的稳定 key，仍返回普通 iced element 并发布业务 Message。例如 `a11y::button("save", "Save", Message::Save)` 与 `a11y::text_input("name", "Name", &state.name, Message::Name)`。

自定义控件使用 `a11y::semantic(key, label, role, content)` 包装，保留原有布局、渲染与鼠标处理，并将辅助操作映射为同一业务 Message：

```rust
a11y::semantic("save", "Save", a11y::SemanticRole::Button, custom_save_control)
    .enabled(state.can_save)
    .on_action(|action| match action {
        a11y::SemanticAction::Click => Some(Message::Save),
        _ => None,
    })
```

首批角色为 Button、CheckBox、TextInput、PasswordInput 和 Label。文本输入通过 `.value(...)` 声明当前值，复选框通过 `.checked(...)` 声明当前状态；PasswordInput 不保留传给 `.value(...)` 的明文。只有显式注册动作处理器的交互节点才提供辅助操作并加入 Tab 顺序。`.enabled(false)` 同时停止内部输入和辅助操作，禁用样式由内部控件决定。包装器代表一个原子节点，内部语义不会重复展开；复合容器应分别包装其交互子控件。

适配器在原生窗口首次显示前创建，在 owner 释放前销毁。语义根据真实 widget layout 构建，处理嵌套滚动偏移和 viewport 裁剪；不可见或禁用项不参与 Tab 顺序。Focus/Click/SetValue 经过弱引用和有界队列，旧节点拒绝操作；焦点节点和 revision 使用同一个锁内 token。Tab 在 iced 事件分发层处理，优先尊重菜单或 overlay 已消费的事件，同一个事件只推进一次焦点。安全输入不会将明文放入语义 snapshot。完整示例见 [accessibility.rs](../examples/accessibility.rs)。

## Linux 服务

启用 `linux-desktop` 或单独的 `linux-tray`、`linux-x11-shortcuts`、`linux-portal-shortcuts`。托盘使用 StatusNotifierItem/DBusMenu，不为 iced 创建另一套 GTK loop；菜单复用 MenuSpec。X11 控制器由业务显式选择，检查每个 grab 与冲突，退出时释放；Wayland 使用真实 portal session，绑定由用户动作触发，保留实际绑定子集及用户改键结果。

`windows.desktop().linux()` 提供显式接口。后台回调进入有界事件队列并 wake，Root 主循环会在 drain 后投递类型化事件并报告 overflow。Linux 不再在启动时自动创建旧 X11 hotkey manager。私有 D-Bus 测试覆盖托盘菜单、provider 恢复、portal 拒绝/取消/超时/session close，Linux target 检查与真实桌面 UI 验收分别记录。原生窗口/application menubar 挂载仍由 winit 宿主返回 Unsupported。详见 [Linux API](../crates/lintel-desktop/docs/linux-backends.md)。

需要窗口菜单的原生 iced 应用可以使用 `menu::menu_bar(&state.menu, Message::Menu, content)`。`MenuModel::new(MenuSpec)` 复用同一菜单描述，选择结果为普通 `MenuAction`；勾选状态由业务 update 回写。支持子菜单、禁用、F10、方向键、Home/End、Enter/Space、Escape/Tab，打开菜单时捕获键盘和指针，模型替换后撤销旧操作。长菜单在窗口内裁剪和滚动。菜单绘制在应用窗口内，不注册桌面全局菜单；accelerator 只显示文字，快捷键应显式注册。见 [client_menubar.rs](../examples/client_menubar.rs)。

启用原生语义时，MenuSpec 自动生成 MenuBar、Menu、MenuItem 与 MenuItemCheckBox 层级，使用实际 header/popup 布局，声明禁用、勾选与 submenu 状态。macOS/Linux 的原生 Press/Click 与 Focus、Windows 的 ExpandCollapse 按锁定 adapter 的实际映射处理，动作经容量 64 的 Weak mailbox 交回 UI，再由既有回调产生普通 Message。业务无需维护 NodeId 或另一套动作注册。详见[菜单无障碍](menu-accessibility.md)。

菜单开关会同步清理旧辅助操作并调整准入；关闭后立即恢复有效的弱路由，正常文字和 IME 输入不等待下一次语义收集。菜单焦点与后台恢复 token 分开，关闭后新接受的动作不会被后续收集误删。多个菜单各自记录来源，最后一个菜单关闭后才恢复后台准入。关闭 popup 会撤销旧节点和请求，模型更新不能把旧路径重定向成新动作。菜单外点击的对应松开事件也会被拦截，避免触发背景控件的独立 release 处理器。macOS 已实测原生 AX 菜单选择和状态；屏幕阅读器语音与 Windows/Linux 原生 provider 动作仍需各自验收。

## 服务插件

```rust
app.plugin(lintel::services::store(lintel::services::StoreConfig {
    name: "settings".into(),
    ..Default::default()
}))
.plugin(lintel::services::network(lintel::services::NetworkOptions {
    origins: vec!["https://api.example.com".into()],
    ..Default::default()
}))
.plugin(lintel::services::diagnostics(Default::default()))
.run()
```

服务按注册顺序启动，后续插件可以通过 `AppContext::get_service` 拿到已注册服务。已有日志订阅器的应用可选择 `install_tracing=false`，把 `Diagnostics::layer()` 接入自己的 tracing registry。同步命令应保持有界；可取消的异步操作使用 `async_command`。Rust 正在执行的同步函数由其自身保证结束，框架的取消会停止等待并阻止过期回复，最终 runtime 销毁另有时限。

`command_timeout` 配置宿主的命令期限。JS 可用 `invoke(name, input, { signal, timeoutMs })` 配置本次等待，默认期限均为 15 秒。需要较长时间的业务操作应同时配置宿主期限和客户端等待。

文件能力通过 `FileAccess::read_only` 或 `read_write` 显式获得目录句柄，读写只接受目录内的相对路径。写入先完成临时文件同步，再替换目标。系统凭据的明文只交给 OS 后端，返回的 `Secret` 缓冲会清零且 Debug 脱敏。诊断和崩溃报告默认只落本地，由应用决定是否导出。

SQLite 通过 `services::sqlite(name, migrations)` 装配，`Migration::new(version, sql)` 声明版本。`SqliteDatabase::call` 在专用线程串行执行查询，全部待执行迁移及版本记录在同一事务中提交。迁移 SQL 的事务控制和保留元数据修改会被 authorizer 拒绝。`interrupt()` 可中断 SQLite 查询，正在执行的任意 Rust 闭包仍由调用方保证有界。

Rust 插件与宿主共享进程和 OS 身份，按受信任组件装配。命令授权在 WebView 宿主入口执行，文件和网络句柄进一步约束所授予的资源。不可信扩展应使用独立进程及单独定义的通信接口。

## AI 原生工具与协作

应用工具在可信启动阶段声明 ID、schema 元数据和 effect，并注册 Rust 输入/输出类型与业务校验。ToolSession 绑定现有 TaskScope，只有应用授予的工具可被发现和执行；UI 变化仍使用普通 Message/Task 或桌面服务。限制输入/输出、并发、进度、超时和取消，不创建另一套执行循环。详见[工具执行](agent-tools.md)。

项目机器目录与 CLI 发现保持同源，描述实现状态和平台边界；不扫描 cwd 配置或环境秘密，也不把发现结果当作执行授权。生成的应用有自己的协作入口。模型 provider、token streaming、会话/RAG 与 MCP/HTTP transport 属于后续独立适配层；普通开发不默认启用 AI Dev Loop。详见[AI 接入约定](ai-native.md)与[CLI 发现](cli-describe.md)。

## 崩溃助手与协议

应用可以通过 `.crash_reporting(CrashConfig)` 在 UI 前安装 reporter，并保留到正常清理后。混合应用先识别 CEF/crash helper 和第二实例，主进程的 reporter 保留到 CEF shutdown 之后。reporter 要求应用自己的绝对报告目录和对应架构的助手可执行文件。

```bash
lintel bundle --crash-reporter --protocol-scheme notes --sign -
```

也可以用 `--crash-helper PATH` 显式提供助手。`--crash-reporter` 从 CLI 同目录取得安装的助手，macOS 成品放入 `Contents/Helpers/lintel-crash-helper`。打包在签名前写入协议元数据并校验 Helper 架构和动态库依赖。Windows 使用同架构的独立助手，协议安装器关联按平台支持报告结果。

## CEF 子进程崩溃报告

混合应用可用 `.cef_crash_reports(absolute_directory)` 选择 CEF 自带 Crashpad，再用 `lintel bundle --cef-crash-reports --sign -` 打包。配置在签名前写入主包 Resources，Helper 被标记为 background-only。服务器地址固定为空，浏览器 crash forwarding 关闭，默认只保存本地报告。

该模式与 `.crash_reporting(...)` 的 Embark 后端互斥。CEF 初始化后会检查报告功能确实开启；bundle 有配置而 builder 未显式选择时也会拒绝运行。报告根目录保持私有且只能由一个活动 CEF 进程独占。浏览器上下文使用内存缓存；旧 initializer 的 session/default 路径不变。CEF 仍可在持久根保存安装级元数据，这不等于网页启用了持久缓存。

Mojo 业务服务通过独立的 `CrashReportingWorker` 显式安装原生捕获，报告增加服务 identity 和监管 attempt；CLI 使用 `--worker-crash-helper PATH` 打包独立角色，可与浏览器 CEF Crashpad 并存。默认模板不启用，配置见[服务崩溃报告](multiprocess.md#服务崩溃报告)。

诊断配置的 `crash_directory` 可显式授权原生报告目录或 CEF Crashpad 根目录。导出仅包含批准的报告文件，限制为最多 16 条记录及 `max_crash_bytes`，CEF 只从 pending/completed 读取 UUID 命名的 MDMP 普通文件。日志字段脱敏不会改写二进制 dump，报告是否分享由应用决定。示例见 [diagnostic_export.rs](../examples/diagnostic_export.rs)。

## 平台边界

macOS arm64 已完成实际 UI、CEF、系统状态查询、原生崩溃与 CEF 沙箱 renderer dump 验收。真实 renderer 故障后，同一窗口可 reload 恢复新页面和画面，过期请求会取消。日志中空 userfree 字符串触发的 UTF-16 警告已经修复。

Windows 原生框架、桌面包、协议关联及 managed update 分别保留模型测试和交叉检查证据，不能代替实际 DLL 装载、菜单操作、注册表事务及助手启动验收。公开 Windows CEF 已有官方五参 bootstrap/cdylib 入口和 CPU OSR 源码，`new --template windows-webview` 可生成显式 experimental 项目。缺少 bootstrap sandbox/version context 会报错；API_ONLY 检查不等于 SDK 链接或运行。`windows inspect-cef` 检查输入布局并报告缺口，不会据此启用尚未验收的 CEF 打包。普通 portable 目录和 managed 安装的更新能力应按各自入口使用，详见 [Windows 打包说明](windows-bundles.md)和[更新说明](updater.md)。

Linux Secret Service、SNI 托盘、X11 与 portal 快捷键后端已接入，仍需要目标机器的用户 D-Bus、真实桌面与凭据服务验收。应用内窗口菜单已有跨平台 iced 接入，GTK/桌面全局菜单挂载仍需对应宿主。可行路径见 [Linux 后端评估](../crates/lintel-desktop/docs/linux-backends.md)。Developer ID、公证和 Windows 发行签名的执行需要对应发行环境。

首批 iced helper 和应用内菜单已接通原生语义与动作，任意自定义控件仍需显式发布语义；未进行屏幕阅读器语音与所有控件种类验收。菜单、文件选择及托盘点击/菜单事件有 macOS 真实记录；全局快捷键已注册但尚无原生递送证据，可见通知、剪贴板写入和读屏语音仍需验收。系统权限申请 UI 不会在默认示例或启动过程中自动触发。

插件按启动前注册装配；运行中的动态加载和热重启不属于首版接口。任务作用域管理通过受管 API 创建的任务，直接 `runtime.spawn` 的任务不会自动加入。取消不能强制终止不让出执行权的 Future 或任意同步 Rust 闭包；原生集成仍需遵守各自的主线程和资源生命周期约定。

## Windows 源码与更新接入

公开 CEF Windows 项目通过 `lintel::windows_cef_entry!(app_main)` 导出锁定版本的 RunWinMain，由同版官方 bootstrap 执行。普通 EXE 入口不能代替它；SDK/bootstrap/DLL/版本信息与沙箱指针必须匹配。CPU 主帧和 popup 沿用已有上传/绘制路径。见 [Windows starter](../templates/windows-webview/WINDOWS-CEF.md)。公开 CEF 默认不再注入绕过源隔离、混合内容或证书检查的开关，也不会自动开启调试 TCP 端口；原非公开模式保持兼容。

启用 `lintel` 的 `updater` feature 可用 `lintel::updater`。Managed 安装支持 inspect/prune/uninstall，维护只处理 ownership 与哈希匹配的受管内容，保留业务数据、未知/修改文件以及活动、回滚和占用中的版本。健康确认是显式可选策略：更新时设置 WindowsStartupHealth，在新应用业务就绪点调用 `confirm_windows_startup()`；普通启动与 CEF helper 不读握手 stdin。确认绑定本次挑战、实际 Child、创建时间和候选版本，退出码 0 不算健康。具体保护范围和失败恢复见 [更新器文档](updater.md)。

原生 Linux 框架的 `public-iced,linux-desktop` 组合已完成 x86_64 GNU 目标整包检查，C 依赖使用隔离的目标编译器和 sysroot；这验证源码和类型/依赖，不代替桌面运行。Windows CEF 的 API_ONLY 检查不包含 SDK 链接；bundled SQLite 的 Windows C 构建仍需要真实 MSVC SDK。

`windows inspect-cef --archive` 现在会按内置 pin 校验实际官方压缩归档和完整 SDK 文件树，拒绝额外 DLL/资源及链接。已实际核对锁定 CEF 143 的 x86、x64、ARM64 minimal 归档，bootstrap 的字节来源与运行验收分别报告；`native_execution_verified` 和 `packaging_enabled` 继续为 false。来源与哈希见 [CEF 归档校验](windows-cef-provenance.md)。

Windows Authenticode 接入使用显式 SignTool 路径、证书库 thumbprint 和 HTTPS RFC3161 时间戳服务。`windows sign` 默认只输出输入/工具哈希和命令计划，`--execute` 才在 Windows 执行。签名在受保护的副本内完成，sign、timestamp、verify 任一阶段失败都不发布输出；业务代码字节变化会拒绝。先完成 Authenticode，再生成 Ed25519 更新 release，避免签名改变 payload 后使旧 release 哈希失效。见 [Windows 签名](windows-signing.md)。本轮不生成 MSI/NSIS 成品。

离线分发使用 `windows distribution stage/inspect` 与 `windows install-distribution`。真实目录包含最终 portable、同架构 launcher/updater、公钥与 domain/schema 隔离的 Ed25519 清单，覆盖身份、版本、channel、schemes 和每个文件的角色/大小/SHA256。检查必须使用调用方独立信任的公钥；包内公钥只作一致性校验。安装消费受保护的私有已验快照，沿用原有 LocalAppData、ACL、协议 ownership 与失败清理路径。macOS 上已完成真实签名目录和 CLI 正反例验证，Windows 安装执行、Authenticode 链信任及 MSI/NSIS 不据此视为通过。详见[Windows 离线分发](windows-distribution.md)。
