# lintel-plugin-iced

本 Crate 是 Iced 插件体系的实现层，用于把 `winit` / `iced` 运行时接入应用事件循环，并支持动态多窗口应用管理。

> 接口层已拆分到 `lintel-plugin-iced-api`。
> 如果业务侧只需要 `IcedAppPluginApi`、`IcedPluginEvent` 或 `IcedApplication` 等公共抽象，优先依赖 `lintel-plugin-iced-api`，不要直接依赖本 crate。

## 功能特性

- **动态应用管理**：支持在运行时动态创建和销毁 Iced 应用程序。
- **事件循环集成**：将 `winit` 事件循环与 `iced` 运行时桥接。
- **多窗口支持**：在单个运行时实例下管理多个独立的 Iced 窗口/应用。

## 分层说明

- `lintel-plugin-iced-api`
  - 对外暴露接口层能力
  - 放置 `IcedAppPluginApi`、`IcedPluginEvent` 以及必要的 Iced 公共抽象
- `lintel-plugin-iced`
  - 承担 runner、订阅管理、插件注册、运行时桥接与兼容 re-export
  - 面向 `app` 和运行时装配层

## 架构与实现说明

### 订阅管理与 Channel 持久化

插件使用 `tokio::sync::mpsc` 通道接收来自系统其他部分的指令（如 `Create` 创建新应用）。在 Iced 的订阅模型中管理该通道的 `Receiver` 端是一个关键的实现细节。

#### 挑战
Iced 的 `subscription()` 方法会在应用生命周期中被多次调用（例如在 UI 更新时）。标准的实现通常会在第一次调用时消耗掉 `Receiver`（使用 `Option::take()`）。如果 `subscription()` 再次被调用，由于 `Receiver` 已经不存在，Iced 会丢弃原始的流（Stream），导致通道被关闭，从而无法接收后续的创建指令（导致 "Closed channel" 错误）。

#### 解决方案
为了确保指令通道永久保持打开状态，我们使用了 `Arc<Mutex<Option<Receiver>>>` 共享状态模式：

1.  **共享所有权**：将 `Receiver` 包装在 `Arc<Mutex<Option<...>>>` 中，并存储在 `IcedAppProxy` 里。
2.  **稳定哈希**：`ProgramRx` 配方（Recipe）使用 `TypeId` 进行哈希。这确保了 Iced 在更新时将其识别为**同一个**订阅。
3.  **持久化流**：
    - 在第一次调用时，`stream()` 锁定互斥锁，取走 `Receiver`，并启动流。
    - 在后续调用 `subscription()` 时，会创建一个包含 `Arc` 克隆的新 `ProgramRx`。
    - 因为哈希值与当前活动的订阅匹配，Iced 会**保持原始流运行**，而不会再次执行 `stream()`（如果再次执行会因为 Receiver 已被取走而失败）。

这种模式保证了“创建应用”的通道在应用程序的整个生命周期内都保持活跃，无论订阅逻辑被重新评估多少次。
