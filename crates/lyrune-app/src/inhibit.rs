use std::collections::HashMap;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, anyhow};
use async_channel::{Receiver, Sender};
use futures_util::StreamExt as _;
use mpris_server::zbus::{
    self, Connection, Proxy,
    zvariant::{OwnedFd, OwnedObjectPath, OwnedValue, Value},
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

/// 后端拒绝某类抑制（如 xdg-desktop-portal-hyprland 只支持 Idle）时，
/// Inhibit 调用本身成功，但随后会对 Request 对象异步发出 Response(2)。
/// 留出一个短暂竞争窗口来识别这种拒绝，再回退到 logind 直连。
const RESPONSE_RACE_TIMEOUT: Duration = Duration::from_secs(2);

const PORTAL_DEST: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const INHIBIT_INTERFACE: &str = "org.freedesktop.portal.Inhibit";
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";

const LOGIN1_DEST: &str = "org.freedesktop.login1";
const LOGIN1_PATH: &str = "/org/freedesktop/login1";
const LOGIN1_INTERFACE: &str = "org.freedesktop.login1.Manager";

/// portal Inhibit 的 flags 位：1 = Logout、2 = UserSwitch、4 = Suspend、8 = Idle。
/// 音乐播放期间只阻止系统挂起（睡眠），允许屏幕正常熄屏以省电。
const INHIBIT_SUSPEND: u32 = 4;

const INHIBIT_APP_ID: &str = "Lyrune";
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
                serve(&connection, &portal, update_events, shutdown_events).await;
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

/// 当前生效的睡眠抑制。
enum Inhibition {
    /// portal 路径：持有 Inhibit 返回的 Request handle 即代表抑制生效；
    /// 解除时对它调用 Request.Close。
    Portal(OwnedObjectPath),
    /// logind 直连路径（Hyprland 等回退）：抑制的生效等价于 fd 的存活，
    /// Drop 关闭 fd 后 logind 自动撤销条目；进程被杀也不会留下幽灵抑制。
    Logind(OwnedFd),
}

async fn serve(
    connection: &Connection,
    portal: &Proxy<'_>,
    updates: Receiver<bool>,
    shutdown: Receiver<()>,
) {
    let mut held: Option<Inhibition> = None;
    // logind 回退路径的系统总线连接：首次回退时创建，之后复用。
    let mut system_connection: Option<Connection> = None;

    loop {
        let should_inhibit = tokio::select! {
            _ = shutdown.recv() => break,
            update = updates.recv() => {
                match update {
                    Ok(should_inhibit) => should_inhibit,
                    Err(_) => break,
                }
            }
        };
        if should_inhibit == held.is_some() {
            continue;
        }
        if should_inhibit {
            // acquire_inhibit 返回 Option：两路都失败时保持 None，
            // 下一次播放状态同步会再次尝试。
            held = acquire_inhibit(connection, portal, &mut system_connection).await;
        } else if let Some(inhibition) = held.take() {
            release_inhibit(connection, inhibition).await;
        }
    }
    if let Some(inhibition) = held.take() {
        release_inhibit(connection, inhibition).await;
    }
}

/// 优先走 portal；若后端明确拒绝（Hyprland 会回 Response(2)，broker 日志出现
/// "Inhibiting other than idle not supported"），回退到 systemd-logind 直连
/// ——这是 wlroots 系桌面的通行做法，与浏览器等应用的行为一致。
async fn acquire_inhibit(
    connection: &Connection,
    portal: &Proxy<'_>,
    system_connection: &mut Option<Connection>,
) -> Option<Inhibition> {
    match portal_inhibit(connection, portal).await {
        Ok(handle) => return Some(Inhibition::Portal(handle)),
        Err(error) => {
            eprintln!("portal 阻止睡眠不可用：{error:#}");
        }
    }
    match logind_inhibit(system_connection).await {
        Ok(fd) => Some(Inhibition::Logind(fd)),
        Err(error) => {
            eprintln!("无法请求阻止系统睡眠：{error:#}");
            None
        }
    }
}

async fn portal_inhibit(connection: &Connection, portal: &Proxy<'_>) -> Result<OwnedObjectPath> {
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
    let handle: OwnedObjectPath = portal
        .call("Inhibit", &("", INHIBIT_SUSPEND, &options))
        .await
        .context("调用 portal Inhibit 失败")?;

    // portal Inhibit 约定上"立即生效、无 Response"，但后端不支持 suspend 时
    // （Hyprland）会异步补发 Response(2) 撤销抑制。在短窗口内竞争该信号：
    // 超时说明后端已接受；收到非 0 状态则按拒绝处理。
    let request = Proxy::new(connection, PORTAL_DEST, handle.clone(), REQUEST_INTERFACE)
        .await
        .context("无法访问抑制请求对象")?;
    let mut response = request
        .receive_signal("Response")
        .await
        .context("无法订阅抑制请求的结束信号")?;
    if let Ok(Some(message)) =
        tokio::time::timeout(RESPONSE_RACE_TIMEOUT, response.next()).await
    {
        let (status, _): (u32, HashMap<String, OwnedValue>) = message
            .body()
            .deserialize()
            .context("解析抑制请求 Response 失败")?;
        if status != 0 {
            return Err(anyhow!(
                "桌面后端拒绝阻止睡眠（Response 状态 {status}），将回退到 logind"
            ));
        }
    }
    Ok(handle)
}

async fn logind_inhibit(system_connection: &mut Option<Connection>) -> Result<OwnedFd> {
    let connection = match system_connection.as_ref() {
        Some(connection) => connection.clone(),
        None => {
            let connection = Connection::system()
                .await
                .context("无法连接系统 D-Bus（logind 回退路径）")?;
            *system_connection = Some(connection.clone());
            connection
        }
    };
    // mode 用 "block"：与 portal suspend 抑制语义一致，硬阻止挂起
    // （"delay" 只是延迟，超时后仍会睡）。fd 由本进程持有，
    // 进程退出或被杀时内核关闭 fd，logind 自动回收条目。
    let login1 = Proxy::new(&connection, LOGIN1_DEST, LOGIN1_PATH, LOGIN1_INTERFACE)
        .await
        .context("无法访问 systemd-logind")?;
    let fd: OwnedFd = login1
        .call(
            "Inhibit",
            &("sleep", INHIBIT_APP_ID, INHIBIT_REASON, "block"),
        )
        .await
        .context("systemd-logind 拒绝了睡眠抑制请求")?;
    Ok(fd)
}

async fn release_inhibit(session: &Connection, inhibition: Inhibition) {
    match inhibition {
        // Drop 关闭 fd 即解除抑制，无需总线往返。
        Inhibition::Logind(fd) => drop(fd),
        Inhibition::Portal(handle) => {
            let request =
                match Proxy::new(session, PORTAL_DEST, handle.clone(), REQUEST_INTERFACE).await {
                    Ok(request) => request,
                    Err(error) => {
                        eprintln!("无法解除系统睡眠阻止：{error:#}");
                        return;
                    }
                };
            if let Err(error) = request.call::<_, _, ()>("Close", &()).await {
                // 后端发过 Response 之后可能已经移除 Request 对象（Hyprland
                // 拒绝 suspend 时即如此）；此时抑制本就不存在，Close 报错属正常，
                // 降级为提示，避免"无法解除系统睡眠阻止"的吓人刷屏。
                if is_object_gone_error(&error) {
                    eprintln!("系统睡眠抑制请求已失效，无需解除：{}", handle.as_str());
                } else {
                    eprintln!("无法解除系统睡眠阻止：{error:#}");
                }
            }
        }
    }
}

/// Close 因「对象/方法不存在」失败：意味着后端早已撤销该 Request。
fn is_object_gone_error(error: &zbus::Error) -> bool {
    let text = error.to_string();
    text.contains("UnknownObject")
        || text.contains("UnknownMethod")
        || text.contains("org.freedesktop.DBus.Error.NotFound")
}
