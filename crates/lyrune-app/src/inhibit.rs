use std::collections::HashMap;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use async_channel::{Receiver, Sender};
use mpris_server::zbus::{
    self, Connection, Proxy, zvariant::OwnedObjectPath, zvariant::Value,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

const PORTAL_DEST: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const INHIBIT_INTERFACE: &str = "org.freedesktop.portal.Inhibit";
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";

/// portal Inhibit 的 flags 位：1 = Logout、2 = UserSwitch、4 = Suspend、8 = Idle。
/// 音乐播放期间只阻止系统挂起（睡眠），允许屏幕正常熄屏以省电。
const INHIBIT_SUSPEND: u32 = 4;

const INHIBIT_REASON: &str = "Lyrune 正在播放音乐";

#[derive(Clone)]
pub struct InhibitHandle {
    updates: Sender<bool>,
}

impl InhibitHandle {
    pub fn set_active(&self, active: bool) {
        let _ = self.updates.try_send(active);
    }
}

pub struct InhibitService {
    handle: InhibitHandle,
    shutdown: Sender<()>,
    thread: Option<thread::JoinHandle<()>>,
}

impl InhibitService {
    pub fn handle(&self) -> InhibitHandle {
        self.handle.clone()
    }
}

impl Drop for InhibitService {
    fn drop(&mut self) {
        let _ = self.shutdown.try_send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn install() -> Result<InhibitService> {
    let (updates, update_events) = async_channel::unbounded::<bool>();
    let (shutdown, shutdown_events) = async_channel::bounded::<()>(1);
    let (startup, startup_result) = std::sync::mpsc::sync_channel(1);

    let thread = thread::Builder::new()
        .name("lyrune-inhibit".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = startup.send(Err(format!("无法创建 Inhibit 运行时：{error}")));
                    return;
                }
            };
            runtime.block_on(async move {
                let connection = match Connection::session().await {
                    Ok(connection) => connection,
                    Err(error) => {
                        let _ = startup.send(Err(format!("无法连接会话 D-Bus：{error}")));
                        return;
                    }
                };
                let portal = match Proxy::new(
                    &connection,
                    PORTAL_DEST,
                    PORTAL_PATH,
                    INHIBIT_INTERFACE,
                )
                .await
                {
                    Ok(portal) => portal,
                    Err(error) => {
                        let _ = startup.send(Err(format!(
                            "无法访问 xdg-desktop-portal 的 Inhibit 接口：{error}"
                        )));
                        return;
                    }
                };
                let _ = startup.send(Ok(()));
                serve(&connection, portal, update_events, shutdown_events).await;
            });
        })
        .context("无法启动 Inhibit 服务线程")?;

    match startup_result.recv_timeout(STARTUP_TIMEOUT + Duration::from_secs(1)) {
        Ok(Ok(())) => Ok(InhibitService {
            handle: InhibitHandle { updates },
            shutdown,
            thread: Some(thread),
        }),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(anyhow::anyhow!(error))
        }
        Err(error) => {
            let _ = shutdown.try_send(());
            let _ = thread.join();
            Err(error).context("等待 Inhibit 服务启动失败")
        }
    }
}

async fn serve(
    connection: &Connection,
    portal: Proxy<'_>,
    updates: Receiver<bool>,
    shutdown: Receiver<()>,
) {
    // 持有 Inhibit 调用返回的 Request handle 即代表抑制生效；
    // 解除时对它调用 org.freedesktop.portal.Request.Close。
    let mut held_handle: Option<OwnedObjectPath> = None;
    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            update = updates.recv() => {
                let Ok(should_inhibit) = update else {
                    break;
                };
                if should_inhibit == held_handle.is_some() {
                    continue;
                }
                if should_inhibit {
                    match inhibit_session(&portal).await {
                        Ok(handle) => held_handle = Some(handle),
                        Err(error) => eprintln!("无法请求阻止系统睡眠：{error:#}"),
                    }
                } else if let Some(handle) = held_handle.take()
                    && let Err(error) = release_inhibit(connection, handle).await
                {
                    eprintln!("无法解除系统睡眠阻止：{error:#}");
                }
            }
        }
    }
    if let Some(handle) = held_handle.take()
        && let Err(error) = release_inhibit(connection, handle).await
    {
        eprintln!("退出时解除系统睡眠阻止失败：{error:#}");
    }
}

async fn inhibit_session(portal: &Proxy<'_>) -> zbus::Result<OwnedObjectPath> {
    // window 标识留空：本应用未沙盒化，Inhibit 不依赖父窗口。
    // handle_token 让 Request handle 路径可识别，便于总线监控与调试。
    let token = format!(
        "lyrune_{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos()
    );
    let options: HashMap<&str, Value> = HashMap::from([
        ("handle_token", Value::from(token)),
        ("reason", Value::from(INHIBIT_REASON)),
    ]);
    portal
        .call("Inhibit", &("", INHIBIT_SUSPEND, &options))
        .await
}

async fn release_inhibit(connection: &Connection, handle: OwnedObjectPath) -> zbus::Result<()> {
    let request = Proxy::new(connection, PORTAL_DEST, handle, REQUEST_INTERFACE).await?;
    let _: () = request.call("Close", &()).await?;
    Ok(())
}
