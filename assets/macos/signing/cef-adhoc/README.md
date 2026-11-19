# 本地 CEF Hardened Runtime 权限

这些文件用于 CEF 143.0.14 / Chromium 143.0.7499.193 在 macOS arm64 上的本地 ad-hoc 签名验收。使用时显式启用 Hardened Runtime，CEF Renderer/GPU 继续使用现有 Seatbelt 沙箱。

| 文件 | 适用进程 | 权限 |
| --- | --- | --- |
| loader.plist | 主程序、default 与 Alerts Helper | disable-library-validation |
| jit.plist | Renderer 与 GPU Helper | disable-library-validation、allow-jit |
| plugin.plist | Plugin Helper | disable-library-validation、allow-unsigned-executable-memory |

本地 ad-hoc 代码没有 Team ID，库验证会拒绝加载随包的 CEF 与托管 dylib，因此本地测试需要显式的库验证例外。不要把此例外直接用作 Developer ID 发行默认值。签名器本身不会自动加入这些权限，也不会把 Helper 权限应用到主程序、Framework、动态库或独立可执行工具。

Renderer、GPU 与 Plugin 的角色权限按固定版本的 [Chromium 签名定义](https://raw.githubusercontent.com/chromium/chromium/143.0.7499.193/chrome/installer/mac/signing/parts.py)配置。库验证的限制见 [Apple 文档](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.cs.disable-library-validation)。升级 CEF 后应重新核对权限与实跑结果。
