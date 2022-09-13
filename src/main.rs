mod ratelimit;

use deadpool_redis::{Config, Connection, Runtime};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::message::BorrowedMessage;
use rdkafka::{ClientConfig, Message as KafkaMessage};
use std::borrow::Cow;
use std::env;
use std::error::Error;
use std::fmt::Display;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use todel::models::Message;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task;
use tokio::time::{interval, Instant};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message as WebSocketMessage;
use tokio_tungstenite::{accept_async, WebSocketStream};

use crate::ratelimit::Ratelimiter;

/// A simple struct that stores data about a Pandemonium instance.
#[derive(Debug, Clone)]
struct Context {
    brokers: String,
    topic: String,
}

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
async fn handle_connection(ctx: Context, stream: TcpStream, addr: SocketAddr, cache: Connection) {
    let consumer: StreamConsumer = ClientConfig::new()
        .set("group.id", &format!("pandemonium:{}", addr))
        .set("bootstrap.servers", &ctx.brokers)
        .create()
        .unwrap();

    consumer
        .subscribe(&[&ctx.topic])
        .expect("Couldn't subscribe to \"oprish\" topic");

    let socket = accept_async(stream)
        .await
        .unwrap_or_else(|_| panic!("Couldn't establish websocket connection with {}", addr));

    let (tx, mut rx) = socket.split();
    let tx = Arc::new(Mutex::new(tx));

    let last_ping = Arc::new(Mutex::new(Instant::now()));

    let handle_rx = async {
        let mut ratelimiter =
            Ratelimiter::new(cache, addr, RATELIMIT_RESET, RATELIMIT_PAYLOAD_LIMIT);
        while let Some(msg) = rx.next().await {
            log::debug!("New gateway message:\n{:#?}", msg);
            if ratelimiter.process_ratelimit().await.is_err() {
                ratelimiter.clear_bucket().await;
                log::info!("Disconnected a client: {}, reason: Hit ratelimit", addr);
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
        consumer
            .stream()
            .for_each(|msg| async {
                if let Ok(msg) = msg {
                    match deserialize_message(msg) {
                        Ok(msg) => {
                            if let Err(err) = tx
                                .lock()
                                .await
                                .send(WebSocketMessage::Text(
                                    serde_json::to_string(&msg)
                                        .expect("Couldn't serialize payload"),
                                ))
                                .await
                            {
                                log::warn!("Failed to send payload to {}: {}", addr, err);
                            }
                        }
                        Err(err) => log::warn!("Failed to deserialize event payload: {}", err),
                    }
                }
            })
            .await;
    };

    tokio::select! {
        _ = check_connection(last_ping.clone()) => {
            log::info!("Dead connection with client {}", addr);
            close_socket(tx, rx, CloseFrame { code: CloseCode::Error, reason: Cow::Borrowed("Client timed out.") }, addr).await
        }
        _ = handle_rx => {
            close_socket(tx, rx, CloseFrame { code: CloseCode::Error, reason: Cow::Borrowed("Client got ratelimited.") }, addr).await;
        },
        _ = handle_events => {
            close_socket(tx, rx, CloseFrame { code: CloseCode::Error, reason: Cow::Borrowed("Server Error.") }, addr).await;
        },
    };
}

async fn close_socket(
    tx: Arc<Mutex<SplitSink<WebSocketStream<TcpStream>, WebSocketMessage>>>,
    rx: SplitStream<WebSocketStream<TcpStream>>,
    frame: CloseFrame<'_>,
    addr: SocketAddr,
) {
    let tx = Arc::try_unwrap(tx).expect("Couldn't obtain tx from MutexLock");
    let tx = tx.into_inner();

    if let Err(err) = tx
        .reunite(rx)
        .expect("Couldn't reunite WebSocket stream")
        .close(Some(frame))
        .await
    {
        log::warn!("Couldn't close socket with {}: {}", addr, err);
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
fn deserialize_message(message: BorrowedMessage) -> Result<Message, Box<dyn Error + Send + Sync>> {
    Ok(serde_json::from_str::<Message>(&String::from_utf8(
        message.payload().ok_or(PayloadNotFound {})?.to_vec(),
    )?)?)
}

#[tokio::main]
async fn main() {
    env_logger::init();

    let brokers = env::var("BROKERS").unwrap_or_else(|_| "127.0.0.1:9092".to_string());
    let topic = env::var("TOPIC").unwrap_or_else(|_| "oprish".to_string());
    let redis_url = env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1".to_string());
    let gateway_address = format!(
        "{}:{}",
        env::var("GATEWAY_ADDRESS").unwrap_or_else(|_| "127.0.0.1".to_string()),
        env::var("GATEWAY_PORT").unwrap_or_else(|_| "9000".to_string())
    );

    let ctx = Context { brokers, topic };

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
        task::spawn(handle_connection(
            ctx.clone(),
            stream,
            addr,
            pool.get()
                .await
                .expect("Couldn't spawn a connection from the Cache pool"),
        ));
    }
}
