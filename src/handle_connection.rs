use deadpool_redis::redis::aio::PubSub;
use deadpool_redis::Connection;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use std::borrow::Cow;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use todel::models::Payload;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::{interval, Instant};
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message as WebSocketMessage;
use tokio_tungstenite::{accept_hdr_async, WebSocketStream};

use crate::ratelimit::Ratelimiter;
use crate::utils::deserialize_message;

/// The duration it takes for a connection to be inactive in for it to be regarded as zombified and
/// disconnected.
const TIMEOUT_DURATION: Duration = Duration::from_secs(46); // 45 seconds + some time to account
                                                            // for jitter

/// The minimum duration of time which can get a client disconnected for spamming gateway pings.
const RATELIMIT_RESET: Duration = Duration::from_secs(10);
const RATELIMIT_PAYLOAD_LIMIT: u32 = 5;

/// A simple function that check's if a client's last ping was over TIMEOUT_DURATION seconds ago and
/// closes the gateway connection if so.
async fn check_connection(last_ping: Arc<Mutex<Instant>>) {
    let mut interval = interval(TIMEOUT_DURATION);
    loop {
        if Instant::now().duration_since(*last_ping.lock().await) > TIMEOUT_DURATION {
            break;
        }
        interval.tick().await;
    }
}

/// A function that handles one client connecting and disconnecting.
pub async fn handle_connection(
    stream: TcpStream,
    addr: SocketAddr,
    cache: Connection,
    pubsub: PubSub,
) {
    let mut rl_address = IpAddr::from_str("127.0.0.1").unwrap();

    let socket = accept_hdr_async(stream, |req: &Request, resp: Response| {
        let headers = req.headers();

        if let Some(ip) = headers.get("X-Real-Ip") {
            rl_address = IpAddr::from_str(ip.to_str().unwrap()).unwrap();
        } else if let Some(ip) = headers.get("CF-Connecting-IP") {
            rl_address = IpAddr::from_str(ip.to_str().unwrap()).unwrap();
        } else {
            rl_address = addr.ip();
        }

        Ok(resp)
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "Couldn't establish websocket connection with {}",
            rl_address
        )
    });

    let (tx, mut rx) = socket.split();
    let tx = Arc::new(Mutex::new(tx));

    let last_ping = Arc::new(Mutex::new(Instant::now()));

    let handle_rx = async {
        let mut ratelimiter =
            Ratelimiter::new(cache, rl_address, RATELIMIT_RESET, RATELIMIT_PAYLOAD_LIMIT);
        while let Some(msg) = rx.next().await {
            log::trace!("New gateway message:\n{:#?}", msg);
            if ratelimiter.process_ratelimit().await.is_err() {
                ratelimiter.clear_bucket().await;
                log::info!(
                    "Disconnected a client: {}, reason: Hit ratelimit",
                    rl_address
                );
                break;
            }
            match msg {
                Ok(data) => match data {
                    WebSocketMessage::Text(message) => {
                        match serde_json::from_str::<Payload>(&message) {
                            Ok(Payload::Ping) => {
                                let mut last_ping = last_ping.lock().await;
                                *last_ping = Instant::now();
                                tx.lock()
                                    .await
                                    .send(WebSocketMessage::Text(
                                        serde_json::to_string(&Payload::Pong).unwrap(),
                                    ))
                                    .await
                                    .expect("Couldn't send pong");
                            }
                            _ => log::debug!("Unknown gateway payload: {}", message),
                        }
                    }
                    _ => log::debug!("Unsupported Gateway message type."),
                },
                Err(_) => break,
            }
        }
    };

    let handle_events = async {
        pubsub
            .into_on_message()
            .for_each(|msg| async {
                match deserialize_message(msg) {
                    Ok(msg) => {
                        if let Err(err) = tx
                            .lock()
                            .await
                            .send(WebSocketMessage::Text(
                                serde_json::to_string(&msg).expect("Couldn't serialize payload"),
                            ))
                            .await
                        {
                            log::warn!("Failed to send payload to {}: {}", rl_address, err);
                        }
                    }
                    Err(err) => log::warn!("Failed to deserialize event payload: {}", err),
                }
            })
            .await;
    };

    tokio::select! {
        _ = check_connection(last_ping.clone()) => {
            log::info!("Dead connection with client {}", rl_address);
            close_socket(tx, rx, CloseFrame { code: CloseCode::Error, reason: Cow::Borrowed("Client ping timed out") }, rl_address).await
        }
        _ = handle_rx => {
            close_socket(tx, rx, CloseFrame { code: CloseCode::Error, reason: Cow::Borrowed("Client got ratelimited") }, rl_address).await;
        },
        _ = handle_events => {
            close_socket(tx, rx, CloseFrame { code: CloseCode::Error, reason: Cow::Borrowed("Server Error") }, rl_address).await;
        },
    };
}

async fn close_socket(
    tx: Arc<Mutex<SplitSink<WebSocketStream<TcpStream>, WebSocketMessage>>>,
    rx: SplitStream<WebSocketStream<TcpStream>>,
    frame: CloseFrame<'_>,
    rl_address: IpAddr,
) {
    let tx = Arc::try_unwrap(tx).expect("Couldn't obtain tx from MutexLock");
    let tx = tx.into_inner();

    if let Err(err) = tx
        .reunite(rx)
        .expect("Couldn't reunite WebSocket stream")
        .close(Some(frame))
        .await
    {
        log::debug!("Couldn't close socket with {}: {}", rl_address, err);
    }
}
