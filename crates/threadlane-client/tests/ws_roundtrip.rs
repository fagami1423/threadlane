//! End-to-end protocol roundtrip: a real `DaemonCore` served over the real
//! WebSocket transport, driven by `RemoteDaemon`.

use std::time::Duration;

use tokio::net::TcpListener;

use threadlane_client::{DaemonClient, RemoteDaemon};
use threadlane_protocol::daemon::{SessionCommand, SessionEvent};

async fn next_event(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
    predicate: impl Fn(&SessionEvent) -> bool,
) -> SessionEvent {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(event)) if predicate(&event) => return event,
            Ok(Some(_)) | Ok(None) => {}
            Err(_) => panic!("timed out waiting for a matching SessionEvent"),
        }
    }
}

#[test]
fn remote_client_speaks_protocol_end_to_end() {
    let executor = threadlane_daemon::chat::executor().expect("daemon executor");
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    executor.spawn(async move {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("listener addr");
        let core = threadlane_daemon::core::DaemonCore::new().expect("daemon core");
        tokio::spawn(threadlane_daemon::server::serve(listener, core, None));
        done_tx.send(addr).expect("send addr");
    });
    let addr = done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("server did not start");

    executor.block_on(async move {
        let client = RemoteDaemon::connect(format!("ws://{addr}"), None);
        let mut events = client.subscribe();

        // Commands fail fast until the dial completes; retry until connected.
        let command = SessionCommand::CancelRun {
            session_id: "no-such-session".to_string(),
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if client.command(command.clone()).await.is_ok() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "client never connected to the daemon"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        // The dispatch's DaemonError comes back over the live event stream.
        let event = next_event(&mut events, |event| {
            matches!(event, SessionEvent::DaemonError { .. })
        })
        .await;
        let SessionEvent::DaemonError { message, .. } = event else {
            unreachable!()
        };
        assert!(
            message.contains("no live runtime"),
            "unexpected error text: {message}"
        );

        // A second attach replays the journal tail: the same DaemonError arrives.
        let second = RemoteDaemon::connect(format!("ws://{addr}"), None);
        let mut second_events = second.subscribe();
        let replayed = next_event(&mut second_events, |event| {
            matches!(event, SessionEvent::DaemonError { .. })
        })
        .await;
        let SessionEvent::DaemonError { message, .. } = replayed else {
            unreachable!()
        };
        assert!(
            message.contains("no live runtime"),
            "journal tail did not replay the earlier error: {message}"
        );
    });
}
