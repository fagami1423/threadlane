//! End-to-end protocol roundtrip: a real `DaemonCore` served over the real
//! WebSocket transport, driven by `RemoteDaemon`.

use std::time::Duration;

use tokio::net::TcpListener;

use threadlane_client::{DaemonClient, RemoteDaemon};
use threadlane_protocol::daemon::{
    CommandRequest, CommandResponse, SessionCommand, SessionEvent, TerminalEvent,
};

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

/// A `CommandRequest` carries the command inside a request_id envelope and
/// resolves to the daemon's `CommandResponse`: dispatch errors resolve the
/// caller's request (not just the broadcast DaemonError), and successful
/// commands resolve to `Ack` while the journal broadcast keeps flowing.
#[test]
fn remote_command_request_round_trips() {
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
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let result = loop {
            match client
                .command_request(CommandRequest {
                    request_id: 1,
                    command: SessionCommand::CancelQueuedMessage {
                        session_id: "no-such-session".to_string(),
                        entry_id: "entry-1".to_string(),
                        work_dir: None,
                    },
                })
                .await
            {
                Err(error) if error.contains("not connected") => {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "client never connected to the daemon"
                    );
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                result => break result,
            }
        };
        // A dispatch failure resolves the request's own Err — the caller's
        // pending intent can resolve without watching the broadcast.
        let error = result.expect_err("dispatch of a dead session must fail");
        assert!(
            error.contains("no live runtime"),
            "unexpected error text: {error}"
        );

        // The same failure still arrives on the broadcast stream.
        next_event(&mut events, |event| {
            matches!(event, SessionEvent::DaemonError { .. })
        })
        .await;

        // A dispatchable command resolves to Ack.
        let result = client
            .command_request(CommandRequest {
                request_id: 2,
                command: SessionCommand::GetProjectState {
                    work_dir: std::env::temp_dir(),
                },
            })
            .await;
        assert_eq!(result, Ok(CommandResponse::Ack));
    });
}

/// Terminal commands round-trip through the wire: a daemon-hosted PTY
/// opens, takes input, and reports its exit back over the event stream.
#[test]
fn remote_terminal_lifecycle_round_trips() {
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

        let terminal_id = "test-terminal-1".to_string();
        let cwd = std::env::temp_dir();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if client
                .command(SessionCommand::TerminalOpen {
                    terminal_id: terminal_id.clone(),
                    cwd: cwd.clone(),
                    cols: 80,
                    rows: 24,
                })
                .await
                .is_ok()
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "client never connected to the daemon"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        // `exit` ends /bin/sh and cmd alike; the daemon reports it as Exited.
        client
            .command(SessionCommand::TerminalInput {
                terminal_id: terminal_id.clone(),
                data: "exit\n".to_string(),
            })
            .await
            .expect("terminal input command");
        let event = next_event(&mut events, |event| {
            matches!(
                event,
                SessionEvent::TerminalEvent {
                    event: TerminalEvent::Exited { terminal_id: id, .. }
                } if *id == "test-terminal-1"
            )
        })
        .await;
        let SessionEvent::TerminalEvent {
            event: TerminalEvent::Exited { exit_code, .. },
        } = event
        else {
            unreachable!()
        };
        // The shell exited normally rather than by signal; the code itself
        // is shell-dependent.
        assert!(exit_code.is_some());

        // A rejected open surfaces scoped to the terminal id as Failed, so
        // the owning view — not a global DaemonError — learns about it.
        // A duplicate id is a deterministic rejection (a missing cwd is
        // shell-dependent).
        for cwd in [cwd.clone(), cwd.clone()] {
            client
                .command(SessionCommand::TerminalOpen {
                    terminal_id: "test-terminal-dup".to_string(),
                    cwd,
                    cols: 80,
                    rows: 24,
                })
                .await
                .expect("terminal open command");
        }
        let event = next_event(&mut events, |event| {
            matches!(
                event,
                SessionEvent::TerminalEvent {
                    event: TerminalEvent::Failed { terminal_id, .. }
                } if terminal_id == "test-terminal-dup"
            )
        })
        .await;
        let SessionEvent::TerminalEvent {
            event: TerminalEvent::Failed { message, .. },
        } = event
        else {
            unreachable!()
        };
        assert!(!message.is_empty());

        // A client that vanishes without TerminalClose leaves no orphan:
        // the server kills the PTY on disconnect and broadcasts Exited.
        let orphan = RemoteDaemon::connect(format!("ws://{addr}"), None);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if orphan
                .command(SessionCommand::TerminalOpen {
                    terminal_id: "test-terminal-orphan".to_string(),
                    cwd: cwd.clone(),
                    cols: 80,
                    rows: 24,
                })
                .await
                .is_ok()
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "orphan client never connected to the daemon"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        // First Output proves the server-side PTY exists before we cut it.
        next_event(&mut events, |event| {
            matches!(
                event,
                SessionEvent::TerminalEvent {
                    event: TerminalEvent::Output { terminal_id, .. }
                } if terminal_id == "test-terminal-orphan"
            )
        })
        .await;
        drop(orphan);
        next_event(&mut events, |event| {
            matches!(
                event,
                SessionEvent::TerminalEvent {
                    event: TerminalEvent::Exited { terminal_id, .. }
                } if terminal_id == "test-terminal-orphan"
            )
        })
        .await;
    });
}
