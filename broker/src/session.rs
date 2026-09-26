//! One WebSocket session: RDCleanPath handshake, then a raw byte pipe.
//!
//! The browser client (`ironrdp-web`) never speaks TLS to the RDP server. It sends a DER
//! `RDCleanPathPdu` carrying the X.224 Connection Request, then expects a DER response with
//! the server certificate and the X.224 Connection Confirm. After that, both sides exchange
//! the already-decrypted RDP byte stream.
//!
//! `ironrdp-acceptor` is the state machine for *being* an RDP server. This broker is not
//! that peer: it forwards the client's X.224 request, upgrades the server TCP socket with
//! `ironrdp-tls` (the same helper the connector uses), and then copies bytes.

use core::net::SocketAddr;
use std::sync::Arc;
use core::time::Duration;

use anyhow::{Context as _, bail};
use futures_util::{SinkExt as _, StreamExt as _};
use ironrdp_pdu::nego::ConnectionConfirm;
use ironrdp_pdu::tpkt::TpktHeader;
use ironrdp_pdu::x224::X224;
use ironrdp_rdcleanpath::{RDCleanPath, RDCleanPathPdu};
use ironrdp_tls::CertificateValidation;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};
use x509_cert::der::Encode as _;

use crate::config::{Config, split_host_port};
use crate::registry::{LiveSession, Registry};

const FIRST_MESSAGE: Duration = Duration::from_secs(30);
const SERVER_CONNECT: Duration = Duration::from_secs(10);
const SERVER_HANDSHAKE: Duration = Duration::from_secs(20);

pub async fn handle_connection(
    tcp: TcpStream,
    peer: SocketAddr,
    config: Arc<Config>,
    registry: Arc<Registry>,
    conn_id: u64,
) {
    let _close = CloseLog { conn_id, peer };
    if let Err(err) = serve(tcp, peer, &config, &registry, conn_id).await {
        warn!(conn_id, %peer, error = %err, "connection failed");
    }
}

async fn serve(
    tcp: TcpStream,
    peer: SocketAddr,
    config: &Config,
    registry: &Arc<Registry>,
    conn_id: u64,
) -> anyhow::Result<()> {
    let mut ws = tokio_tungstenite::accept_async(tcp)
        .await
        .context("websocket handshake")?;
    info!(conn_id, %peer, "websocket open");

    let first = match timeout(FIRST_MESSAGE, first_binary(&mut ws)).await {
        Ok(Ok(Some(bytes))) => bytes,
        Ok(Ok(None)) => return Ok(()),
        Ok(Err(err)) => return Err(err),
        Err(_) => bail!("timed out waiting for the RDCleanPath request"),
    };

    let pdu = match RDCleanPathPdu::from_der(&first) {
        Ok(pdu) => pdu,
        Err(err) => {
            warn!(conn_id, %peer, error = %err, "first message was not an RDCleanPath PDU");
            send_pdu(&mut ws, &RDCleanPathPdu::new_general_error()).await?;
            return Ok(());
        }
    };

    let (destination, proxy_auth, x224) = match pdu.into_enum() {
        Ok(RDCleanPath::Request {
            destination,
            proxy_auth,
            x224_connection_request,
            ..
        }) => (destination, proxy_auth, x224_connection_request),
        Ok(_) => {
            warn!(conn_id, %peer, "RDCleanPath message was not a request");
            send_pdu(&mut ws, &RDCleanPathPdu::new_general_error()).await?;
            return Ok(());
        }
        Err(err) => {
            warn!(conn_id, %peer, error = %err, "RDCleanPath request is missing fields");
            send_pdu(&mut ws, &RDCleanPathPdu::new_general_error()).await?;
            return Ok(());
        }
    };

    if proxy_auth.trim().is_empty() {
        warn!(conn_id, %peer, "rejected empty proxy_auth token");
        send_pdu(&mut ws, &RDCleanPathPdu::new_general_error()).await?;
        return Ok(());
    }
    info!(conn_id, %peer, token = %proxy_auth, "accepted proxy_auth token");

    let (host, port) = match split_host_port(&destination) {
        Ok(parsed) => parsed,
        Err(err) => {
            warn!(conn_id, %peer, destination = %destination, error = %err, "bad destination");
            send_pdu(&mut ws, &RDCleanPathPdu::new_general_error()).await?;
            return Ok(());
        }
    };

    if !config.allows(&host, port) {
        warn!(
            conn_id,
            %peer,
            destination = %destination,
            allowed = %config.target_label(),
            "destination is not the configured RDP target"
        );
        send_pdu(&mut ws, &RDCleanPathPdu::new_general_error()).await?;
        return Ok(());
    }

    let x224 = x224.as_bytes().to_vec();
    if x224.is_empty() {
        warn!(conn_id, %peer, "X.224 connection request is empty");
        send_pdu(&mut ws, &RDCleanPathPdu::new_general_error()).await?;
        return Ok(());
    }

    info!(conn_id, %peer, destination = %destination, "opening RDP tcp");
    let mut server = match timeout(SERVER_CONNECT, TcpStream::connect((host.as_str(), port))).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(err)) => {
            warn!(conn_id, %peer, destination = %destination, error = %err, "tcp connect failed");
            send_pdu(&mut ws, &RDCleanPathPdu::new_general_error()).await?;
            return Ok(());
        }
        Err(_) => {
            warn!(conn_id, %peer, destination = %destination, "tcp connect timed out");
            send_pdu(&mut ws, &RDCleanPathPdu::new_general_error()).await?;
            return Ok(());
        }
    };
    server.set_nodelay(true).context("set TCP_NODELAY")?;
    let server_addr = server.peer_addr().context("server peer address")?.ip().to_string();

    let confirm = match timeout(SERVER_HANDSHAKE, exchange_x224(&mut server, &x224)).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(err)) => {
            warn!(conn_id, %peer, error = %err, "X.224 exchange failed");
            send_pdu(&mut ws, &RDCleanPathPdu::new_general_error()).await?;
            return Ok(());
        }
        Err(_) => {
            warn!(conn_id, %peer, "timed out waiting for the X.224 Connection Confirm");
            send_pdu(&mut ws, &RDCleanPathPdu::new_general_error()).await?;
            return Ok(());
        }
    };

    if let Ok(X224(ConnectionConfirm::Failure { code })) = ironrdp_core::decode::<X224<ConnectionConfirm>>(&confirm) {
        warn!(conn_id, %peer, ?code, "RDP negotiation refused");
        let pdu = RDCleanPathPdu::new_negotiation_error(confirm).context("encode negotiation error")?;
        send_pdu(&mut ws, &pdu).await?;
        return Ok(());
    }

    let policy = if config.tls_insecure {
        CertificateValidation::DangerouslyAcceptInvalidCertificate
    } else {
        CertificateValidation::Strict
    };

    let (tls, cert) = match timeout(
        SERVER_HANDSHAKE,
        ironrdp_tls::upgrade_with_certificate_validation(server, &host, policy),
    )
    .await
    {
        Ok(Ok(upgraded)) => upgraded,
        Ok(Err(err)) => {
            warn!(conn_id, %peer, error = %err, "TLS upgrade failed");
            send_pdu(&mut ws, &RDCleanPathPdu::new_general_error()).await?;
            return Ok(());
        }
        Err(_) => {
            warn!(conn_id, %peer, "TLS upgrade timed out");
            send_pdu(&mut ws, &RDCleanPathPdu::new_general_error()).await?;
            return Ok(());
        }
    };

    let cert_der = cert.to_der().context("encode server certificate")?;
    let response =
        RDCleanPathPdu::new_response(server_addr.clone(), confirm, [cert_der]).context("build RDCleanPath response")?;
    send_pdu(&mut ws, &response).await?;
    info!(conn_id, %peer, destination = %destination, server_addr = %server_addr, "RDCleanPath complete, piping bytes");

    let destination_label = format!("{host}:{port}");
    let live = registry.register(destination_label, peer.ip().to_string());
    pipe(ws, tls, conn_id, peer, &live, config.idle_timeout).await
}

async fn exchange_x224(server: &mut TcpStream, request: &[u8]) -> anyhow::Result<Vec<u8>> {
    server.write_all(request).await.context("write X.224 request")?;
    server.flush().await.context("flush X.224 request")?;
    read_tpkt(server).await
}

/// One TPKT unit, framed with `ironrdp-pdu`'s `TpktHeader` rather than a hand-rolled length parse.
async fn read_tpkt(server: &mut TcpStream) -> anyhow::Result<Vec<u8>> {
    let mut header_bytes = [0u8; TpktHeader::SIZE];
    server.read_exact(&mut header_bytes).await.context("read TPKT header")?;
    let header = TpktHeader::read(&mut ironrdp_core::ReadCursor::new(&header_bytes))
        .map_err(|err| anyhow::anyhow!("TPKT header: {err}"))?;
    let length = usize::from(header.packet_length);
    let mut unit = vec![0u8; length];
    unit[..TpktHeader::SIZE].copy_from_slice(&header_bytes);
    server
        .read_exact(&mut unit[TpktHeader::SIZE..])
        .await
        .context("read TPKT body")?;
    Ok(unit)
}

async fn first_binary<S>(ws: &mut WebSocketStream<S>) -> anyhow::Result<Option<Vec<u8>>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        match ws.next().await {
            Some(Ok(Message::Binary(bytes))) => return Ok(Some(bytes.to_vec())),
            Some(Ok(Message::Ping(payload))) => {
                ws.send(Message::Pong(payload)).await.context("pong")?;
            }
            Some(Ok(Message::Close(_))) | None => return Ok(None),
            Some(Ok(_)) => continue,
            Some(Err(err)) => return Err(err).context("read websocket"),
        }
    }
}

async fn send_pdu<S>(ws: &mut WebSocketStream<S>, pdu: &RDCleanPathPdu) -> anyhow::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let der = pdu.to_der().context("DER-encode RDCleanPath PDU")?;
    ws.send(Message::Binary(der.into()))
        .await
        .context("send RDCleanPath PDU")?;
    Ok(())
}

/// Copy bytes until either side closes. Both directions live in this task; dropping
/// the `select` cancels the other copy, so nothing is left running after return.
async fn pipe<S>(
    ws: WebSocketStream<S>,
    tls: ironrdp_tls::TlsStream<TcpStream>,
    conn_id: u64,
    peer: SocketAddr,
    live: &LiveSession,
    idle_timeout: Duration,
) -> anyhow::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let session = Arc::clone(&live.session);
    let idle_session = Arc::clone(&live.session);
    let mut kill_rx = live.kill_rx.clone();
    let (mut sink, mut stream) = ws.split();
    let (mut server_read, mut server_write) = tokio::io::split(tls);
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Message>(32);
    let client_tx = out_tx.clone();
    let server_tx = out_tx.clone();
    drop(out_tx);

    let writer = async move {
        while let Some(message) = out_rx.recv().await {
            sink.send(message).await.context("write websocket")?;
        }
        sink.close().await.context("close websocket")?;
        Ok::<(), anyhow::Error>(())
    };

    let client_to_server = async move {
        loop {
            match stream.next().await {
                Some(Ok(Message::Binary(bytes))) => {
                    let n = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                    server_write.write_all(&bytes).await.context("write RDP")?;
                    server_write.flush().await.context("flush RDP")?;
                    session.add_sent(n);
                }
                Some(Ok(Message::Ping(payload))) => {
                    client_tx.send(Message::Pong(payload)).await.context("queue pong")?;
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(err)) => return Err(err).context("read websocket"),
            }
        }
        Ok(())
    };

    let server_session = Arc::clone(&live.session);
    let server_to_client = async move {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = server_read.read(&mut buf).await.context("read RDP")?;
            if n == 0 {
                break;
            }
            server_tx
                .send(Message::Binary(buf[..n].to_vec().into()))
                .await
                .context("queue RDP bytes")?;
            server_session.add_received(u64::try_from(n).unwrap_or(u64::MAX));
        }
        Ok(())
    };

    let idle_watch = async move {
        if idle_timeout.is_zero() {
            core::future::pending::<()>().await;
        }
        loop {
            let idle_for = idle_session.idle_for();
            if idle_for >= idle_timeout {
                info!(
                    session_id = %idle_session.id,
                    idle_secs = idle_for.as_secs(),
                    "session idle-timeout"
                );
                idle_session.request_stop();
                return;
            }
            tokio::time::sleep(idle_timeout - idle_for).await;
        }
    };

    let result = tokio::select! {
        result = writer => result,
        result = client_to_server => result,
        result = server_to_client => result,
        _ = kill_rx.wait_for(|killed| *killed) => Ok(()),
        _ = idle_watch => Ok(()),
    };
    info!(conn_id, %peer, "byte pipe stopped");
    result
}

struct CloseLog {
    conn_id: u64,
    peer: SocketAddr,
}

impl Drop for CloseLog {
    fn drop(&mut self) {
        info!(conn_id = self.conn_id, peer = %self.peer, "connection closed");
    }
}
