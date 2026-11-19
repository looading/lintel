# Lintel 基础能力补齐计划

沿用已确认的框架设计与轻量接入约定。用户要求继续多 agent 协作，并禁止 AI Dev Loop；本轮直接运行针对性 Cargo、独立应用和平台检查，证据保存于 `build/checklist-followup/`。所有改动限独立 Lintel 项目，不提交或发布。

## 平台服务

- [x] 在 `crates/lintel-desktop/src/permissions.rs` 提供统一权限状态和显式申请入口，覆盖 Accessibility、通知、屏幕录制和输入监听。查询不能触发授权提示；平台不支持时返回明确错误。
- [x] 在桌面 owner 上提供 Windows 窗口菜单挂载和解除，窗口销毁时先释放绑定；验证错误句柄、重复调用和清理。
- [x] 接入 Linux 系统凭据服务并评估菜单、托盘和快捷键宿主；无真实桌面会话时保留准确的未验收状态。
- [x] 运行 desktop 单测、所有权编译测试及可用目标的检查，保留真实系统接口证据。

## 激活事件

- [x] 在 `src/runtime/macos_reopen.rs` 补公开 winit 缺少的原生 Reopen selector，保持现有 delegate 和 URL 路径。只添加缺失方法，拒绝覆盖未知实现。
- [x] 回调只写入有界主线程队列并唤醒事件循环，经现有 `ActivationInbox` 在 setup 后投递；guard 析构后撤销订阅。
- [x] 在 `src/runtime/mod.rs` 和 `event_loop.rs` 接入 guard 与排空队列，不改变原有依赖配置的 winit 回调。
- [x] 独立应用通过真实 macOS reopen AppleEvent 验证可见窗口标记、业务消息和退出清理。证据：`build/checklist-followup/reopen-result.json`。

实现依据为 Apple 的 [NSApplicationDelegate](https://developer.apple.com/documentation/appkit/nsapplicationdelegate) 与 [class_addMethod](https://developer.apple.com/documentation/objectivec/class_addmethod(_:_:_:_:))。本轮只添加公开 winit 缺少的方法，不修改已存在的实现。

## WebView 故障恢复

- [x] 在现有 WebView/CEF client handler 中定位 UTF-16 警告，针对实际错误修复，保留中文与 emoji 往返回归。
- [x] 扩展独立示例，通过真实 renderer 终止验证事件、reload/reopen、旧页面回调隔离和助手回收。
- [x] 核实 CEF 子进程 dump 的既有接口与沙箱约束，只有生成并检查真实报告后才记为通过。

## Windows 发行

- [x] 在 `lintel-build` 增加当前用户协议安装关联和撤销，使用所有权记录及修改前比较，不修改系统 UserChoice，不覆盖其他程序的关联。
- [x] 在 `lintel-updater` 实现独立 Windows 更新助手和受控版本切换，沿用签名清单、身份与归档校验，失败保留旧版本。
- [x] 在 CLI 暴露上述接口；补充 Windows CEF 的 PE、helper、资源和启动契约检查，缺少真实启动证据时明确保留验收缺口。
- [x] 用 PE/注册表存储夹具验证冲突、并发、失败回滚和同版本不覆盖，运行可用 Windows 目标交叉检查。

## 集成验收

- [x] 同伴源码稳定后，运行 root 公开配置单测及全部相关示例构建；检查原有依赖配置兼容性。
- [x] 重建 CLI，生成独立消费应用并启用新增服务，通过编译和关键源码哈希检查。
- [x] 更新能力文档和验收矩阵，分别写清已实现、实机验证、交叉检查及仍受环境限制的部分。

原生 owner 接入还增加了 `WindowManager::with_native_window`、可 await 的桌面 Pending、Windows scoped menu lease 与高层 accelerator hook。真实菜单/文件对话框已验收；全局快捷键回调和托盘点击仍保留未验收状态。
