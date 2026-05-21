# OpenClaw-Desktop

跨平台桌面客户端 — Tauri 2.x 加壳 + 内嵌 [OpenClaw](https://github.com/openclaw/openclaw)(MIT)Gateway 与 Control UI。双击安装,完全离线,无需用户预装 Node.js。

## 架构

```
Tauri Shell (Rust)
  └─ WebView → http://127.0.0.1:18789/  (OpenClaw Control UI, chat tab = WebChat)
  └─ Sidecar → <resources>/node/node + <resources>/openclaw/openclaw.mjs gateway --port 18789
```

- **前端**:直接复用 OpenClaw 自带的 Control UI(Lit + Vite,Gateway 内置 serve `dist/control-ui`)
- **后端**:OpenClaw Gateway 原样运行,通过 sidecar 启动
- **运行时**:portable Node.js 内嵌(打包时下载,运行时无需系统 Node)
- **数据**:`~/.openclaw/`(配置、会话、credentials 都在用户主目录)

## 项目结构

| 路径 | 用途 |
|---|---|
| `openclaw/` | 上游 OpenClaw 源码(`git clone`,被 `.gitignore`,作为构建输入) |
| `desktop/` | Tauri 桌面壳工程(Rust + minimal HTML splash) |
| `scripts/` | 构建脚本(下载 portable Node、build Control UI、prepare resources) |

## 开发与构建(摘要)

```powershell
# 1. 克隆 OpenClaw 上游(若 openclaw/ 不存在)
git clone https://github.com/openclaw/openclaw.git openclaw

# 2. 安装 OpenClaw 依赖 + 构建 Control UI 静态文件
cd openclaw
pnpm install
pnpm ui:build
cd ..

# 3. 准备 sidecar 资源(下载 portable Node + 复制 openclaw 到 desktop/src-tauri/resources/)
./scripts/prepare-bundle.ps1

# 4. 开发模式
cd desktop
pnpm tauri dev

# 5. 生产打包
pnpm tauri build
```

详细步骤见 `C:\Users\Admin\.claude\plans\openclaw-tauri-vue-3-snoopy-quilt.md`。

## License

桌面壳代码 MIT;内嵌的 OpenClaw 遵循其自身 MIT 协议(Copyright 2025 Peter Steinberger)。
