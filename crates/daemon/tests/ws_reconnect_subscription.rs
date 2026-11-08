//! Integration test for the WS reconnect bug.
//!
//! Symptom: web GUI's session list goes stale after a daemon restart;
//! SETs that fire after the daemon comes back up don't reach an
//! already-open client. Reproducer here:
//!
//!   1. Spin up CellServer V1 on a port (own tokio runtime in its
//!      own thread, so we can kill the entire runtime to mimic a
//!      process exit — aborting the JoinHandle of `server.run()`
//!      alone wouldn't close per-connection tasks, since they're
//!      detached from the accept loop).
//!   2. Connect a `MykoClient` and `watch_query::<GetAllSessions>()`.
//!   3. SET a session via V1's ctx — verify the client receives it.
//!   4. Shut down V1's runtime (simulates `kill` of the daemon).
//!   5. Wait for the client's autosocket to drop into Disconnected.
//!   6. Spin up CellServer V2 on the same port.
//!   7. SET another session via V2's ctx and assert the client's cell
//!      contains it.

use std::{
    net::SocketAddr,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use hyphae::Gettable;
use marshal_entities::{GetAllSessions, Session, SessionId};
use myko::{
    client::{ConnectionStatus, MykoClient, MykoProtocol},
    server::{MykoServerContext, Persister},
    wire::{MEvent, MEventType},
};
use myko_server::{BlackholePersister, MykoServer};

const POLL_TIMEOUT: Duration = Duration::from_secs(8);

/// Handle for a server running on its own tokio runtime in its own
/// thread. `shutdown()` tears the runtime down completely (which
/// closes all per-connection sockets, simulating `kill` of the
/// daemon process).
struct ServerHandle {
    ctx: MykoServerContext,
    shutdown: Option<mpsc::Sender<()>>,
    join: Option<thread::JoinHandle<()>>,
}

impl ServerHandle {
    fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Start a myko server on `bind`, or say why it couldn't: most likely another
/// process took the port after it was picked (MAR-21).
fn try_spawn_server(bind: SocketAddr) -> Result<ServerHandle, String> {
    let (ready_tx, ready_rx) = mpsc::sync_channel::<MykoServerContext>(1);
    let (failed_tx, failed_rx) = mpsc::channel::<String>();
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

    let join = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async move {
            let blackhole: Arc<dyn Persister> = Arc::new(BlackholePersister);
            let server = Arc::new(
                MykoServer::builder()
                    .with_bind_addr(bind)
                    .with_default_persister(blackhole)
                    .build(),
            );
            ready_tx.send(server.ctx()).expect("send ctx");
            tokio::select! {
                r = server.run() => {
                    if let Err(e) = r {
                        let _ = failed_tx.send(e.to_string());
                    }
                }
                _ = tokio::task::spawn_blocking(move || {
                    let _ = shutdown_rx.recv();
                }) => {}
            }
        });
        drop(rt);
    });

    let ctx = ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server thread came up");
    // A port another process holds accepts connections too, so a connect
    // alone doesn't say the server bound it. Its bind failure comes at once,
    // so it's waited for a moment after the port answers.
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::net::TcpStream::connect(bind).is_err() {
        if let Ok(why) = failed_rx.try_recv() {
            // The thread's runtime waits on the shutdown channel as it ends.
            drop(shutdown_tx);
            let _ = join.join();
            return Err(why);
        }
        if Instant::now() >= deadline {
            return Err(format!("nothing listening on {bind}"));
        }
        thread::sleep(Duration::from_millis(25));
    }
    if let Ok(why) = failed_rx.recv_timeout(Duration::from_millis(200)) {
        // The thread's runtime waits on the shutdown channel as it ends.
        drop(shutdown_tx);
        let _ = join.join();
        return Err(why);
    }

    Ok(ServerHandle {
        ctx,
        shutdown: Some(shutdown_tx),
        join: Some(join),
    })
}

/// A myko server on a free port. The port is picked by binding port 0 and
/// closing it, so another process can take it before the server binds it:
/// then it's picked again (MAR-21).
fn spawn_server_on_free_port() -> (ServerHandle, SocketAddr) {
    let mut lost = Vec::new();
    for _ in 0..5 {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("pick a free port")
            .port();
        let bind: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        match try_spawn_server(bind) {
            Ok(server) => return (server, bind),
            Err(why) => lost.push(format!("{bind}: {why}")),
        }
    }
    panic!("no free port held for the myko server: {lost:?}");
}

fn session_set_event(id: &str) -> MEvent {
    let session = Session {
        id: SessionId(Arc::from(id)),
        client_id: None,
        agent_id: None,
        pid: 0,
        cwd: "/tmp".into(),
        git_branch: None,
        current_task: None,
        session_name: None,
        activity: None,
        kind: None,
        connected_at: chrono::Utc::now().timestamp_millis(),
        last_activity_at: None,
        last_tool: None,
        last_tool_at: None,
        operator: None,
        host: None,
        project: None,
        channels_enabled: None,
        acks_pushes: None,
        harness: None,
        identity: None,
    };
    MEvent::from_item(&session, MEventType::SET, &uuid::Uuid::new_v4().to_string())
}

fn wait_for(label: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + POLL_TIMEOUT;
    while Instant::now() < deadline {
        if f() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for: {label}");
}

#[test]
fn client_subscription_resumes_after_server_restart() {
    let _ = env_logger::builder().is_test(true).try_init();
    marshal_entities::link();
    daemon::link();

    let (server_v1, bind) = spawn_server_on_free_port();
    let addr = format!("ws://{bind}");

    let client = MykoClient::new();
    client.set_protocol(MykoProtocol::JSON);
    client.set_address(Some(addr.clone()));

    let status = client.connection_status();
    let s1 = status.clone();
    wait_for("initial connection", move || {
        matches!(s1.get(), ConnectionStatus::Connected(_))
    });

    let cell = client.watch_query::<GetAllSessions>(GetAllSessions {});

    server_v1
        .ctx
        .apply_event_batch(vec![session_set_event("s-pre")])
        .expect("apply pre-restart session");

    let c1 = cell.clone();
    wait_for("pre-restart session on client", move || {
        c1.get().iter().any(|s| s.id.0.as_ref() == "s-pre")
    });

    server_v1.shutdown();

    let s2 = status.clone();
    wait_for("client observes disconnect", move || {
        !matches!(s2.get(), ConnectionStatus::Connected(_))
    });

    let mut server_v2 = None;
    for _ in 0..30 {
        match try_spawn_server(bind) {
            Ok(s) => {
                server_v2 = Some(s);
                break;
            }
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
    let server_v2 = server_v2.expect("rebind same port for server v2");

    let s3 = status.clone();
    wait_for("client reconnects", move || {
        matches!(s3.get(), ConnectionStatus::Connected(_))
    });

    thread::sleep(Duration::from_millis(500));

    server_v2
        .ctx
        .apply_event_batch(vec![session_set_event("s-post")])
        .expect("apply post-restart session");

    let c2 = cell.clone();
    wait_for("post-restart session on client", move || {
        c2.get().iter().any(|s| s.id.0.as_ref() == "s-post")
    });

    server_v2.shutdown();
}

/// Same dance, but with multiple concurrent subscriptions — same shape
/// as the web app, which subscribes to GetAllSessions and GetAllMessages.
#[test]
fn many_subscriptions_resume_together_after_restart() {
    let _ = env_logger::builder().is_test(true).try_init();
    marshal_entities::link();
    daemon::link();

    let (server_v1, bind) = spawn_server_on_free_port();
    let addr = format!("ws://{bind}");

    let client = MykoClient::new();
    client.set_protocol(MykoProtocol::JSON);
    client.set_address(Some(addr.clone()));
    let status = client.connection_status();
    let s = status.clone();
    wait_for("initial connection", move || {
        matches!(s.get(), ConnectionStatus::Connected(_))
    });

    let cells: Vec<_> = (0..5)
        .map(|_| client.watch_query::<GetAllSessions>(GetAllSessions {}))
        .collect();

    server_v1
        .ctx
        .apply_event_batch(vec![session_set_event("s-pre")])
        .expect("apply pre");

    for (i, cell) in cells.iter().enumerate() {
        let cell = cell.clone();
        wait_for(&format!("pre-restart on subscription #{i}"), move || {
            cell.get().iter().any(|s| s.id.0.as_ref() == "s-pre")
        });
    }

    server_v1.shutdown();
    let s2 = status.clone();
    wait_for("disconnect", move || {
        !matches!(s2.get(), ConnectionStatus::Connected(_))
    });

    let mut server_v2 = None;
    for _ in 0..30 {
        match try_spawn_server(bind) {
            Ok(s) => {
                server_v2 = Some(s);
                break;
            }
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
    let server_v2 = server_v2.expect("rebind");

    let s3 = status.clone();
    wait_for("reconnect", move || {
        matches!(s3.get(), ConnectionStatus::Connected(_))
    });
    thread::sleep(Duration::from_millis(500));

    server_v2
        .ctx
        .apply_event_batch(vec![session_set_event("s-post")])
        .expect("apply post");

    for (i, cell) in cells.iter().enumerate() {
        let cell = cell.clone();
        wait_for(&format!("post-restart on subscription #{i}"), move || {
            cell.get().iter().any(|s| s.id.0.as_ref() == "s-post")
        });
    }

    server_v2.shutdown();
}

/// Reproduces the leptos `live_query` lifecycle: subscribe a sink to
/// the `watch_query` cell, then drop the cell handle (let it go out
/// of scope). The sink should still receive updates because the
/// cell.own(guard) cycle keeps the cell alive.
#[test]
fn dropped_cell_handle_still_pumps_updates_after_reconnect() {
    let _ = env_logger::builder().is_test(true).try_init();
    marshal_entities::link();
    daemon::link();

    let (server_v1, bind) = spawn_server_on_free_port();
    let addr = format!("ws://{bind}");

    let client = MykoClient::new();
    client.set_protocol(MykoProtocol::JSON);
    client.set_address(Some(addr.clone()));
    let status = client.connection_status();
    let s = status.clone();
    wait_for("initial connection", move || {
        matches!(s.get(), ConnectionStatus::Connected(_))
    });

    use std::sync::Mutex;
    let mirror: Arc<Mutex<Vec<Arc<Session>>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let cell = client.watch_query::<GetAllSessions>(GetAllSessions {});
        let mirror_for_sub = mirror.clone();
        let guard = <hyphae::Cell<Vec<Arc<Session>>, hyphae::CellImmutable> as hyphae::Watchable<
            Vec<Arc<Session>>,
        >>::subscribe(&cell, move |signal| {
            if let hyphae::Signal::Value(items) = signal {
                let mut m = mirror_for_sub.lock().unwrap();
                *m = (**items).to_vec();
            }
        });
        cell.own(guard);
    }

    server_v1
        .ctx
        .apply_event_batch(vec![session_set_event("s-pre")])
        .expect("apply pre-restart session");

    let m1 = mirror.clone();
    wait_for("pre-restart session in mirror sink", move || {
        m1.lock()
            .unwrap()
            .iter()
            .any(|s| s.id.0.as_ref() == "s-pre")
    });

    server_v1.shutdown();
    let s2 = status.clone();
    wait_for("client observes disconnect", move || {
        !matches!(s2.get(), ConnectionStatus::Connected(_))
    });

    let mut server_v2 = None;
    for _ in 0..30 {
        match try_spawn_server(bind) {
            Ok(s) => {
                server_v2 = Some(s);
                break;
            }
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
    let server_v2 = server_v2.expect("rebind v2");

    let s3 = status.clone();
    wait_for("client reconnects", move || {
        matches!(s3.get(), ConnectionStatus::Connected(_))
    });

    thread::sleep(Duration::from_millis(500));

    server_v2
        .ctx
        .apply_event_batch(vec![session_set_event("s-post")])
        .expect("apply post-restart session");

    let m2 = mirror.clone();
    wait_for("post-restart session in mirror sink", move || {
        m2.lock()
            .unwrap()
            .iter()
            .any(|s| s.id.0.as_ref() == "s-post")
    });

    server_v2.shutdown();
}

/// MAR-21: a port another process holds is reported as lost, not taken for
/// the server's own, so the caller picks another or retries the bind.
#[test]
fn a_port_another_process_holds_is_reported() {
    marshal_entities::link();
    daemon::link();
    let held = std::net::TcpListener::bind("127.0.0.1:0").expect("hold a port");
    let bind = held.local_addr().unwrap();
    let Err(why) = try_spawn_server(bind) else {
        panic!("the server reported {bind} as its own while another socket held it");
    };
    assert!(why.to_lowercase().contains("in use"), "{why}");
    drop(held);
    let (server, _bind) = spawn_server_on_free_port();
    server.shutdown();
}
