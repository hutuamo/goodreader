# 将书库与阅读数据保存在 Application Support

正式 macOS 应用将静态书籍包保存在 `~/Library/Application Support/GoodReader/Books/`，将 SQLite 数据库保存在同一应用数据目录，并提供在 Finder 中打开书库的入口；应用包内部不保存或修改用户内容。这个决定取代开发目录内固定 `books/` 的早期设想，避免应用签名、升级或替换影响用户书籍和阅读状态，也让删除应用本体与永久删除用户数据保持为两个明确动作。
