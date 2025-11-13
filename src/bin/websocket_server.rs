use std::collections::HashSet;
use env_logger::{Builder, Target};
use futures_util::{SinkExt, StreamExt};
use log::{LevelFilter, error, info, warn};
use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::broadcast;
use tokio::time::{Duration, interval};
use sqlx::postgres::{PgPoolOptions, PgListener};
use sqlx::PgPool;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::{Html, IntoResponse},
    routing::{get, get_service},
    Router,
};
use std::net::SocketAddr;
use tower_http::services::ServeFile;


#[derive(Debug, Clone, Serialize, Deserialize)]
struct PriceUpdate {
    symbol: String,
    price: f64,
    source: String,
    timestamp: i64,
}

type SharedStateCounter = Arc<AtomicUsize>;

#[derive(Clone)]
struct AppState {
    counter: SharedStateCounter,
    tx: broadcast::Sender<PriceUpdate>,
    pool: PgPool,
}

#[axum::debug_handler]
async fn client_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    info!("Client a demandé une mise à niveau WebSocket");
    let rx = state.tx.subscribe();
    ws.on_upgrade(move |socket| {
        handle_socket(socket, rx, state.counter)
    })
}

async fn handle_socket(
    mut socket: WebSocket,
    mut rx: broadcast::Receiver<PriceUpdate>,
    state: SharedStateCounter,
) {
    // Incrémenter le compteur de clients
    let count = state.fetch_add(1, Ordering::SeqCst) + 1;
    info!("New client connected! (Total clients: {})", count);
    let mut subscriptions: HashSet<String> = HashSet::new();
    let welcome = serde_json::json!({
        "type": "connected",
        "message": "Connected! Please send subscription message."
    });
    
    if socket
        .send(Message::Text(welcome.to_string().into()))
        .await
        .is_err()
    {
        // Décrémenter si l'envoi échoue
        let count = state.fetch_sub(1, Ordering::SeqCst) - 1;
        info!("Client disconnected (welcome send failed). (Total clients: {})", count);
        return;
    }

    loop {
        tokio::select! {
            // Broadcast de prix
            Ok(price_update) = rx.recv() => {
                if subscriptions.is_empty() || subscriptions.contains(&price_update.symbol) {
                    let json = match serde_json::to_string(&price_update) {
                        Ok(j) => j,
                        Err(e) => {
                            error!("Failed to serialize price update: {}", e);
                            continue;
                        }
                    };
                    if socket.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }
            
            // Messages du client
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        info!("Received from client: {}", text);

                        if text == "/stats" {
                            let count = state.load(Ordering::SeqCst);
                            let stats_msg = format!("Active clients: {}", count);
                            if socket.send(Message::Text(stats_msg.into())).await.is_err() {
                                break;
                            }
                        } else if let Ok(json_msg) = serde_json::from_str::<serde_json::Value>(&text) {
                            
                            if json_msg["action"] == "subscribe" && json_msg["symbols"].is_array() {
                                info!("Client is subscribing to symbols: {:?}", json_msg["symbols"]);
                                subscriptions.clear(); // On réinitialise les abos
                                for symbol in json_msg["symbols"].as_array().unwrap() {
                                    if let Some(symbol_str) = symbol.as_str() {
                                        subscriptions.insert(symbol_str.to_string());
                                    }
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        break;
                    }
                    Some(Err(_)) => {
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
    let count = state.fetch_sub(1, Ordering::SeqCst) - 1;
    info!("Connection handler finished. (Total clients: {})", count);
}

async fn poll_database(
    pool: &PgPool,
    tx: &broadcast::Sender<PriceUpdate>,
) -> Result<(), sqlx::Error> {
    info!("Polling database for new prices...");
    let prices = sqlx::query_as!(
        PriceUpdate,
        r#"
        SELECT DISTINCT ON (symbol, source)
            symbol, 
            price::float8 as "price!",
            source, 
            timestamp
        FROM stock_prices
        ORDER BY symbol, source, timestamp DESC
        "#
    )
    .fetch_all(pool)
    .await?;

    info!("Found {} distinct prices in DB. Broadcasting...", prices.len());
    for update in prices {
        let _ = tx.send(update);
    }
    Ok(())
}

// Pour update uniquement quand on reçoit un NOTIFY
async fn database_listener(pool: PgPool, tx: broadcast::Sender<PriceUpdate>) {
    info!("Database listener starting...");

    let mut listener = match PgListener::connect_with(&pool).await {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to connect PgListener: {}", e);
            return;
        }
    };

    if let Err(e) = listener.listen("new_price").await {
        error!("Failed to listen on channel 'new_price': {}", e);
        return;
    }

    info!("Listening on 'new_price' channel...");

    loop {
        match listener.recv().await {
            Ok(_notification) => {
                info!("Received 'new_price' notification!");
                // On a été réveillé, on lance notre fonction de polling
                if let Err(e) = poll_database(&pool, &tx).await {
                    error!("Database poll error after NOTIFY: {}", e);
                }
            }
            Err(e) => {
                error!("PgListener `recv` error: {}", e);
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    Builder::new()
        .target(Target::Stdout)
        .filter_level(LevelFilter::Info)
        .init();
    dotenv::dotenv().ok();

    let state_counter = Arc::new(AtomicUsize::new(0));
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set in .env");

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await?;
    info!("Connected to database");

    let (tx, _rx) = broadcast::channel::<PriceUpdate>(100);

    let app_state = AppState {
        counter: state_counter,
        tx: tx.clone(),
        pool: pool.clone(), // On clone le pool pour le poller
    };

    let tx_clone = tx.clone();
    tokio::spawn(async move {
        database_listener(pool, tx_clone).await;
    });

    let app = Router::new()
        .route("/ws", get(client_handler))
        .route("/", get_service(ServeFile::new("dashboard.html")))
        .with_state(app_state);

    // Lancer le serveur Axum
    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    info!("WebSocket server + HTTP server listening on http://{}", addr);
    axum::serve(
        tokio::net::TcpListener::bind(addr).await?,
        app.into_make_service(),
    )
    .await?;

    Ok(())
}