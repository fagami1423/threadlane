//! LAN device pairing: exposes the embedded [`DaemonCore`] on the local
//! network so a thin client (the mobile app) can attach over the same
//! WebSocket transport [`server`] speaks.
//!
//! "Share with mobile" binds an ephemeral port on every interface behind a
//! freshly generated bearer token, then renders the [`PairingInfo::uri`]
//! deep link as a QR code. The token is what makes the LAN bind safe — the
//! command surface includes prompt submission, so an unauthenticated port
//! on a routable interface is remote code execution (the standalone
//! daemon's `main` enforces the same rule).

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use base64::Engine;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::core::DaemonCore;

/// URL scheme the mobile app registers; iOS Camera offers to open a QR
/// carrying this scheme in the pairing app directly.
pub const PAIRING_SCHEME: &str = "threadlane";

/// Connection details a thin client needs to attach, encoded into the
/// [`uri`](PairingInfo::uri) deep link behind the QR code.
#[derive(Clone, Debug)]
pub struct PairingInfo {
    /// LAN IPv4 the listener is reachable on.
    pub host: String,
    pub port: u16,
    /// Bearer token every attaching client must present at handshake.
    pub token: String,
}

impl PairingInfo {
    /// The WebSocket endpoint a native thin client dials.
    pub fn ws_url(&self) -> String {
        format!("ws://{}:{}", self.host, self.port)
    }

    /// The pairing deep link. The mobile app parses `host`, `port`, and
    /// `token` out of the query and dials [`ws_url`](Self::ws_url) with the
    /// token as its `Authorization: Bearer` credential.
    pub fn uri(&self) -> String {
        format!(
            "{}://pair?host={}&port={}&token={}",
            PAIRING_SCHEME, self.host, self.port, self.token
        )
    }
}

/// Handle for a live pairing listener. [`stop`](Self::stop) (or drop) shuts
/// the listener down and disconnects attached clients.
pub struct PairingServer {
    info: PairingInfo,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl PairingServer {
    /// Bind a LAN socket and start serving `core` behind a fresh bearer
    /// token.
    ///
    /// Binding needs the shared tokio reactor, so callers run this inside
    /// the daemon executor (`chat::executor().spawn(PairingServer::start(core))`)
    /// and await the join handle from the UI future.
    pub async fn start(core: Arc<DaemonCore>) -> Result<Self, String> {
        // Ephemeral port: the QR carries the actual port, so a fixed one
        // would only add collision handling.
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))
            .await
            .map_err(|error| format!("could not bind pairing listener: {error}"))?;
        let port = listener
            .local_addr()
            .map_err(|error| format!("could not read pairing listener address: {error}"))?
            .port();
        let token = pairing_token();
        // Without a routable interface there is nothing useful to show a
        // phone — fall back to loopback so the flow still works end-to-end
        // for a mobile client running in a simulator on the same machine.
        let host = primary_ipv4()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| Ipv4Addr::LOCALHOST.to_string());
        let (shutdown, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(crate::server::serve_until(
            listener,
            core,
            Some(token.clone()),
            async move {
                let _ = shutdown_rx.await;
            },
        ));
        tracing::info!(%host, %port, "pairing listener started");
        Ok(Self {
            info: PairingInfo { host, port, token },
            shutdown: Some(shutdown),
            task,
        })
    }

    pub fn info(&self) -> &PairingInfo {
        &self.info
    }

    /// Stop accepting new clients and disconnect every attached one.
    pub async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = (&mut self.task).await;
    }
}

impl Drop for PairingServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.task.abort();
    }
}

/// A URL-safe bearer token with enough entropy that a network attacker
/// cannot guess it during a pairing session.
fn pairing_token() -> String {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).expect("secure randomness should be available for pairing tokens");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The IPv4 outbound traffic would leave on. A UDP `connect` sends nothing —
/// it only asks the routing table which interface would be used — so this
/// works on a LAN without internet access.
fn primary_ipv4() -> Option<Ipv4Addr> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(8, 8, 8, 8), 80)).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_link_local() => Some(ip),
        _ => None,
    }
}
