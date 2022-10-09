mod ratelimit;

use deadpool_redis::redis::aio::PubSub;
use deadpool_redis::redis::Msg;
use deadpool_redis::{Config, Connection, Runtime};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use std::borrow::Cow;
use std::env;
use std::error::Error;
use std::fmt::Display;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use todel::models::Message;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task;
use tokio::time::{interval, Instant};
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message as WebSocketMessage;
use tokio_tungstenite::{accept_hdr_async, WebSocketStream};

use crate::ratelimit::Ratelimiter;

/// The duration it takes for a connection to be inactive in for it to be regarded as zombified and
/// disconnected.
const TIMEOUT_DURATION: Duration = Duration::from_secs(20);

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
async fn handle_connection(stream: TcpStream, addr: SocketAddr, cache: Connection, pubsub: PubSub) {
    let mut rl_address = IpAddr::from_str("127.0.0.1").unwrap();
    let mut ratelimiter =
        Ratelimiter::new(cache, rl_address, RATELIMIT_RESET, RATELIMIT_PAYLOAD_LIMIT);
    if let Err(()) = ratelimiter.process_ratelimit().await {
        log::info!(
            "Disconnected a client: {}, reason: Hit ratelimit",
            rl_address
        );
        return;
    }

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
        while let Some(msg) = rx.next().await {
            log::debug!("New gateway message:\n{:#?}", msg);
            if let Err(()) = ratelimiter.process_ratelimit().await {
                log::info!(
                    "Disconnected a client: {}, reason: Hit ratelimit",
                    rl_address
                );
                break;
            }
            match msg {
                Ok(data) => match data {
                    WebSocketMessage::Ping(x) => {
                        let mut last_ping = last_ping.lock().await;
                        *last_ping = Instant::now();
                        tx.lock()
                            .await
                            .send(WebSocketMessage::Pong(x))
                            .await
                            .expect("Couldn't send pong");
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
            close_socket(tx, rx, CloseFrame { code: CloseCode::Error, reason: Cow::Borrowed("Client timed out.") }, rl_address).await
        }
        _ = handle_rx => {
            close_socket(tx, rx, CloseFrame { code: CloseCode::Error, reason: Cow::Borrowed("Client got ratelimited.") }, rl_address).await;
        },
        _ = handle_events => {
            close_socket(tx, rx, CloseFrame { code: CloseCode::Error, reason: Cow::Borrowed("Server Error.") }, rl_address).await;
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
        log::warn!("Couldn't close socket with {}: {}", rl_address, err);
    }
}

/// An Error that represents a Payload not being found.
#[derive(Debug)]
struct PayloadNotFound {}

impl Display for PayloadNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Payload Not Found")
    }
}

impl Error for PayloadNotFound {}

/// A function that simplifies deserializing a message Payload.
fn deserialize_message(message: Msg) -> Result<Message, Box<dyn Error + Send + Sync>> {
    Ok(serde_json::from_str::<Message>(
        &message
            .get_payload::<String>()
            .map_err(|_| PayloadNotFound {})?,
    )?)
}

#[tokio::main]
async fn main() {
    env_logger::init();

    let redis_url = env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1".to_string());
    let gateway_address = format!(
        "{}:{}",
        env::var("GATEWAY_ADDRESS").unwrap_or_else(|_| "127.0.0.1".to_string()),
        env::var("GATEWAY_PORT").unwrap_or_else(|_| "7160".to_string())
    );

    let cfg = Config::from_url(redis_url);
    let pool = cfg
        .create_pool(Some(Runtime::Tokio1))
        .expect("Couldn't connect to Cache");

    let socket = TcpListener::bind(&gateway_address)
        .await
        .unwrap_or_else(|_| panic!("Couldn't start a websocket on {}", gateway_address));

    log::info!("Gateway started at {}", gateway_address);

    while let Ok((stream, addr)) = socket.accept().await {
        log::info!("New connection on ip {}", addr);
        let pubsub = match pool.get().await {
            Ok(pool) => pool,
            Err(err) => {
                log::warn!("Couldn't generate a new connection: {:?}", err);
                continue;
            }
        };
        let mut pubsub = Connection::take(pubsub).into_pubsub();
        if let Err(err) = pubsub.subscribe("oprish-events").await {
            log::warn!("Couldn't subscribe to oprish-events: {:?}", err);
            continue;
        }
        let cache = match pool.get().await {
            Ok(pool) => pool,
            Err(err) => {
                log::warn!("Couldn't generate a new connection: {:?}", err);
                continue;
            }
        };
        task::spawn(handle_connection(stream, addr, cache, pubsub));
    }
}
