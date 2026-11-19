# Lintel 平台能力补齐

继续既有设计与多 agent 分工，禁止 AI Dev Loop，不提交或发布；证据保存在 `build/checklist-platform/`。沿用原渲染、窗口路由和插件生命周期，平台差异通过可选适配层处理。

## Windows WebView

- [x] 复用既有 CPU OSR 的 BGRA 帧上传；公开 Windows 默认不用未验证的 GPU texture 互操作。
- [x] 按锁定的 CEF143 官方五参数 RunWinMain ABI 接收 bootstrap sandbox/version context，检查版本和作用域；缺少上下文时返回错误，保留原非 public 路径。
- [x] 生成同名 bootstrap.exe/App.dll 布局与模板，验证 PE、资源、ABI 输入与对应架构。API-only 编译、真实链接与实机验证分别记账。

## Windows 安装生命周期

- [x] 以安装 receipt 和文件哈希为边界补卸载与旧版本回收；不自动删除业务数据、未知文件、活动/回滚版本或仍被进程使用的版本。
- [x] 补显式 opt-in 健康握手，绑定启动挑战、实际 Child handle、PID/创建时间及版本；默认不消费旧业务 stdin/stdout。
- [x] 模型验证超时、伪造确认、并发操作、失败回滚和清理边界，运行可用 Windows 目标检查。

## Linux 系统后端

- [x] 通过 SNI/DBusMenu 提供 winit 兼容托盘和菜单，不启动独立 GTK loop。
- [x] X11 检查每个 XGrabKey 结果和连接状态；Wayland portal 绑定由业务显式发起，支持取消与 session 关闭。
- [x] 后台事件经有界队列与 wake 回主循环；Runtime drop 关闭任务/session，私有 D-Bus fixture 不接用户总线。

## 原生无障碍

- [x] 在窗口首次显示前安装 AccessKit，所有 WindowEvent 先交适配器；适配器在原生窗口释放前析构。
- [x] 为首批文本、按钮、输入框和复选框提供轻量 helper，将明确的语义、真实布局和操作回调接到现有 iced widget；不凭空推断未知自定义控件。
- [x] 支持键盘焦点、Tab/Shift+Tab、读屏 Focus/Click/SetValue；隐藏/禁用/已移除节点拒绝操作，安全输入不发布明文值。
- [x] 通过真实控件示例与主机可用的 AX 客户端验收，记录无法执行的 Windows/Linux 原生部分。

## 集成

- [x] 汇总平台实现、检查与原生验收状态，更新公开文档。
- [x] 重建嵌入源码的 CLI，生成独立应用，检查归档完整性与有效依赖来源。

上述勾选表示源码、模型、协议与可用目标检查完成；Windows SDK链接/原生执行、Linux桌面与实际读屏语音未计为通过。详细范围见 build/checklist-platform/verification.md。
