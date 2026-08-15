# sisyphus-web

前端工程（ADR-0003：Vue 3 + Vue Flow）。web 批次在此落 Vue 工程，构建产物输出到 `dist/`。

当前 `dist/` 只含占位 `index.html`（票 B2a-T5）：sisyphus-server 经 rust-embed
内嵌该目录产物对外提供静态服务与 SPA fallback。真实前端构建进 `dist/` 后
server 侧零改动——release 编译期内嵌、debug 运行时读盘（ADR-0005）。

本地覆盖目录（同名文件压过内嵌资源）在 Server 数据目录的 `web/` 子目录。
