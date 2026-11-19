# lintel-plugin-iced-api

`lintel-plugin-iced-api` 是 Iced 插件体系的接口层 crate。

它的职责是为业务模块和上层应用提供稳定的公共类型与 trait，避免调用方直接依赖 `lintel-plugin-iced` 的运行时实现细节。

## 提供的能力

- `IcedAppPluginApi`
  - 面向业务侧暴露创建 / 关闭 Iced Program 的统一接口
- `IcedPluginEvent`
  - Iced 插件运行时使用的公共事件类型
- `lintel-lib-iced` 的必要 re-export
  - 供调用方复用 `IcedApplication`、`ProgramId`、`AnyMessage` 等抽象

## 分层定位

- `lintel-plugin-iced-api`：接口层
- `lintel-plugin-iced`：实现层

推荐依赖方向：

- 业务 crate 如果只需要创建、关闭或描述 Iced Program，优先依赖 `lintel-plugin-iced-api`
- 只有在需要 runner、运行时桥接或插件实现细节时，才依赖 `lintel-plugin-iced`

## 设计目的

将 UI 插件能力拆成 `api` / `impl` 两层后，可以获得：

- 更稳定的依赖边界
- 更低的编译耦合
- 更清晰的架构分层
- 更方便的后续演进（例如菜单、托盘、UI 插件继续沿用同一模式拆分）
