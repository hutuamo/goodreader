mod agent;
mod db;
mod generation;
mod importer;
mod library;
mod models;
mod server;

use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
pub fn run() {
    let shutdown = Arc::new(Mutex::new(None));
    let shutdown_for_setup = shutdown.clone();
    let shutdown_for_exit = shutdown.clone();

    let app = tauri::Builder::default()
        .setup(move |app| {
            let data_dir = if let Some(path) = std::env::var_os("GOODREADER_DATA_DIR") {
                std::path::PathBuf::from(path)
            } else {
                app.path()
                    .data_dir()
                    .context("无法解析 macOS Application Support")?
                    .join("GoodReader")
            };
            let books_dir = data_dir.join("Books");
            std::fs::create_dir_all(&books_dir)?;

            let database = Arc::new(db::Database::open(&data_dir)?);
            database.ensure_daily_backup()?;

            let (launch_tx, launch_rx) = mpsc::sync_channel(1);
            let thread_books = books_dir.clone();
            let thread_agent_tasks = data_dir.join("AgentTasks");
            let thread_database = database.clone();
            std::thread::Builder::new()
                .name("goodreader-http".to_string())
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()
                        .expect("创建 GoodReader HTTP 运行时");
                    runtime.block_on(async move {
                        match server::start(thread_books, thread_agent_tasks, thread_database).await
                        {
                            Ok(handle) => {
                                let _ = launch_tx.send(Ok((
                                    handle.bootstrap_url,
                                    handle.origin,
                                    handle.shutdown,
                                )));
                                std::future::pending::<()>().await;
                            }
                            Err(error) => {
                                let _ = launch_tx.send(Err(format!("{error:#}")));
                            }
                        }
                    });
                })?;

            let (bootstrap_url, origin, shutdown_sender) = launch_rx
                .recv_timeout(Duration::from_secs(15))
                .context("本地 HTTP 服务启动超时")?
                .map_err(anyhow::Error::msg)?;
            *shutdown_for_setup.lock().expect("退出锁") = Some(shutdown_sender);

            let allowed_origin = origin.clone();
            WebviewWindowBuilder::new(
                app,
                "main",
                WebviewUrl::External(bootstrap_url.parse().context("启动地址无效")?),
            )
            .title("GoodReader")
            .inner_size(1320.0, 860.0)
            .min_inner_size(920.0, 640.0)
            .center()
            .on_navigation(move |url| {
                let candidate = format!(
                    "{}://{}{}",
                    url.scheme(),
                    url.host_str().unwrap_or_default(),
                    url.port()
                        .map(|port| format!(":{port}"))
                        .unwrap_or_default()
                );
                candidate == allowed_origin || url.as_str() == "about:blank"
            })
            .build()?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("无法构建 GoodReader 应用");

    app.run(move |_handle, event| {
        if matches!(
            event,
            tauri::RunEvent::Exit | tauri::RunEvent::ExitRequested { .. }
        ) {
            if let Some(sender) = shutdown_for_exit.lock().expect("退出锁").take() {
                let _ = sender.send(());
            }
        }
    });
}
