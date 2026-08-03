# 使用 Tauri、Rust、SQLite 与轻量 Web 前端

GoodReader V1 使用 Tauri 2 构建 macOS 应用壳和单一系统 WebView，Rust 进程内本地服务负责书籍扫描、校验、静态托管与阅读状态接口，单个 SQLite 数据库负责持久化，书架和统一阅读器使用原生 HTML、CSS 与少量 TypeScript 构建。相比 Electron，这避免随应用携带完整 Chromium；相比纯 SwiftUI，这让 HTTP、内容处理和数据库集中在同一 Rust 后端；V1 不引入 React、Vue 或 Python 运行时。
