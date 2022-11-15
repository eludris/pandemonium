mod handle_connection;
mod ratelimit;
mod utils;

use deadpool_redis::{Config, Connection, Runtime};
use std::env;
use tokio::{net::TcpListener, task};

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
        task::spawn(handle_connection::handle_connection(
            stream, addr, cache, pubsub,
        ));
    }
}
