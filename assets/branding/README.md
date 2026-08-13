# Branding assets

这里是图标设计源的唯一目录。透明品牌 Master 只包含 Logo 本体；macOS App Icon
另有一个明确的白色 Squircle 派生资源，Windows App Icon 和 Tray 不复用这个白底。

```text
assets/branding/
├── app/
│   ├── app-icon-master.svg
│   ├── app-icon-master.png   # 1024×1024 RGBA，透明品牌 Master / Windows 输入
│   ├── app-icon-macos.svg
│   └── app-icon-macos.png   # 1024×1024 RGBA，832px 白色 Squircle + 96px 透明边距
└── tray/
    ├── tray-macos.svg
    ├── tray-macos.png        # 64×64 RGBA，黑色 Template
    ├── tray-windows.svg
    └── tray-windows.png      # 64×64 RGBA，品牌色
```

## 生成规则

- `app-icon-master.png` 是透明品牌 Master，也是 Windows App Icon 的输入，Logo 保留安全边距。
- `app-icon-macos.png` 是 macOS App Icon 的派生资源：1024×1024 透明画布、832×832 白色圆角容器、Logo+Badge 约 580px；Dock/Applications/Finder 使用它。
- Tray 与 App Icon 分开维护；macOS Tray 只使用黑色/透明 Template，Windows Tray 可以使用品牌色。
- `src-tauri/icons/` 只保存 Tauri 实际构建或自动生成的资源；未被当前配置引用的冗余 `icon.png` 不保留。不要把 `32x32.png`、`128x128.png`、`icon.icns`、`icon.ico` 或 Tray 输出反向作为设计源。
- 透明 `app-icon-master.png` 用于 Windows/Store/其他平台；只有 `app-icon-macos.png` 在透明画布内包含 832×832 白色 Squircle 和约 580px 的 Logo 组合。

从设计源生成全部 Tauri 资源和 Tray 输出：

```bash
pnpm run branding:generate
```

生成器会分别使用 `app-icon-macos.png` 生成 macOS `icon.icns`，使用透明
`app-icon-master.png` 生成 Windows/Store/其他平台资源；不会把 macOS 白色
容器泄漏到 Windows App Icon。

检查所有品牌 PNG 的外围像素是否为 `Alpha=0`：

```bash
pnpm run branding:verify
```
