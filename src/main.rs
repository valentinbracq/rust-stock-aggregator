use reqwest;
use serde::{Deserialize, Serialize};
use std::env;
use std::str::FromStr;
use std::future::IntoFuture;
use chrono::Utc;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use sqlx::types::BigDecimal;
use tokio::time::{interval, Duration};
use tokio::signal;
use tracing::{info, error, instrument, Level};
use tracing_subscriber::EnvFilter;
use anyhow::Result;
use rand::Rng;
use tokio::time::sleep;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
    Router, routing::get,
};
use serde_json::json;
use std::net::SocketAddr;
use clap::Parser;

#[derive(Debug, Clone)]
struct StockPrice {
    symbol: String,
    price: f64,
    source: String,
    timestamp: i64,
}

struct Config {
    symbols: Vec<String>,
    database_url: String,
}


#[derive(Deserialize, Debug)]
struct AlphaVantageResponse {
    #[serde(rename = "Global Quote")]
    quote: AlphaQuote,
}

#[derive(Deserialize, Debug)]
struct AlphaQuote {
    #[serde(rename = "01. symbol")]
    symbol: String,
    #[serde(rename = "05. price")]
    price: String,
}

#[derive(Deserialize, Debug)]
struct FinnhubResponse {
    c: f64,
}

// Pour fetch une seule fois et tester
#[derive(Parser, Debug)]
#[command(version, about = "A stock price aggregator", long_about = None)]
struct CliArgs {
    #[arg(long, default_value_t = false)]
    fetch_once: bool,
}

// Pour afficher en localhost
#[derive(Debug, Serialize)]
struct ApiPriceResponse {
    symbol: String,
    price: f64,
    source: String,
    timestamp: i64,
}

type AppState = PgPool; // Alias pour l'état de l'application

#[instrument(skip(pool))]
async fn save_price(pool: &PgPool, price: &StockPrice) -> Result<()> {
        let price_decimal = BigDecimal::from_str(&price.price.to_string())?;
    sqlx::query!(
        r#"
        INSERT INTO stock_prices (symbol, price, source, timestamp)
        VALUES ($1, $2, $3, $4)
        "#,
        price.symbol,
        price_decimal,
        price.source,
        price.timestamp
    )
    .execute(pool)
    .await?;
    info!("Saved {}: ${} from {}", price.symbol, price.price, price.source);
    Ok(())
}

// Récupération des données depuis alpha_vantage
#[instrument]
async fn fetch_alpha_vantage(symbol: String) -> Result<StockPrice> {
    info!("Fetching {} from Alpha Vantage", symbol);
    let api_key = env::var("ALPHA_VANTAGE_KEY")?;
    let url = format!(
        "https://www.alphavantage.co/query?function=GLOBAL_QUOTE&symbol={}&apikey={}",
        symbol, api_key
    );

    let resp = reqwest::get(&url)
        .await?
        .json::<AlphaVantageResponse>()
        .await?;
    
    Ok(StockPrice {
        symbol: resp.quote.symbol.clone(),
        price: resp.quote.price.parse()?,
        source: "alpha_vantage".to_string(),
        timestamp: Utc::now().timestamp(),
    })
}

// Récupération des données depuis finnhub
#[instrument]
async fn fetch_finnhub(symbol: String) -> Result<StockPrice> {
    info!("Fetching {} from Finnhub", symbol);
    let api_key = env::var("FINNHUB_KEY")?;
    let url = format!("https://finnhub.io/api/v1/quote?symbol={}", symbol);
    
    let client = reqwest::Client::new();
    let resp = client.get(&url)
        .header("X-Finnhub-Token", api_key)
        .send().await?
        .json::<FinnhubResponse>().await?;

    Ok(StockPrice {
        symbol: symbol.to_string(),
        price: resp.c,
        source: "finnhub".to_string(),
        timestamp: Utc::now().timestamp(),
    })
}

// Récupération des données depuis une source mock (pour tests)
#[instrument]
async fn fetch_mock_source(symbol: String) -> Result<StockPrice> {
    info!("(Mock) Fetching {} from MockSource...", symbol);
    
    let delay = rand::thread_rng().gen_range(100..150); // Delay d'attente aléatoire
    sleep(Duration::from_millis(delay)).await;
    let price_f64 = rand::thread_rng().gen_range(100.0..500.0); // Prix aléatoire
    info!("(Mock) Fetched {} price: ${}", symbol, price_f64);

    Ok(StockPrice {
        symbol,
        price: price_f64,
        source: "mock_source".to_string(),
        timestamp: Utc::now().timestamp(),
    })
}

// Fonction pour récupérer et sauvegarder tous les prix dans la base de données
#[instrument(skip_all)]
async fn fetch_and_save_all(pool: &PgPool, symbols: &[String]) -> Result<()> {
    info!("Starting fetch cycle for {} symbols", symbols.len());

    let mut tasks = vec![];

    for symbol in symbols {
        let alpha_task = fetch_alpha_vantage(symbol.clone());
        let finnhub_task = fetch_finnhub(symbol.clone());
        let mock_task = fetch_mock_source(symbol.clone());

        tasks.push(tokio::spawn(async move { // spawn lance une tâche en parallèle
            tokio::join!(alpha_task, finnhub_task, mock_task)
        }));
    }

    for task in tasks {
        match task.await {
            Ok((alpha_result, finnhub_result, mock_result)) => {
                
                match alpha_result {
                    Ok(price) => {
                        if let Err(e) = save_price(pool, &price).await {
                            error!("Failed to save alpha_vantage price: {}", e);
                        }
                    }
                    Err(e) => {
                        error!("Failed to fetch alpha_vantage price: {}", e);
                    }
                }
                
                match finnhub_result {
                    Ok(price) => {
                        if let Err(e) = save_price(pool, &price).await {
                            error!("Failed to save finnhub price: {}", e);
                        }
                    }
                    Err(e) => {
                        error!("Failed to fetch finnhub price: {}", e);
                    }
                }

                match mock_result {
                    Ok(price) => {
                        if let Err(e) = save_price(pool, &price).await {
                            error!("Failed to save mock_source price: {}", e);
                        }
                    }
                    Err(e) => {
                        error!("Failed to fetch mock_source price: {}", e);
                    }
                }
            }
            Err(e) => {
                error!("A spawned task failed: {}", e);
            }
        }
    }

    info!("Completed fetch cycle");
    Ok(())
}

// Pour afficher les prix dans http://localhost:3000/latest-prices
async fn get_latest_prices(
    State(pool): State<AppState>,
) -> impl IntoResponse {
    info!("GET /latest-prices - Querying prices for all symbols");

    let result = sqlx::query_as!(
        ApiPriceResponse,
        r#"
        SELECT DISTINCT ON (symbol)
            symbol, 
            price::float8 as "price!", 
            source, 
            timestamp 
        FROM stock_prices 
        ORDER BY symbol, timestamp DESC
        "#
    )
    .fetch_all(&pool)
    .await;

    match result {
        Ok(prices) => {
            (StatusCode::OK, Json(prices)).into_response()
        }
        Err(e) => {
            error!("Failed to query latest prices: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to query database" })),
            )
                .into_response()
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok(); // Charger .env

    let args = CliArgs::parse();

    // Setup tracing
    let filter = EnvFilter::builder()
        .with_default_directive(Level::INFO.into())
        .from_env_lossy();
        
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .init();

    info!("Starting stock price aggregator service...");

    // Configuration
    let config = Config {
        symbols: vec!["AAPL".to_string(), "GOOGL".to_string(), "MSFT".to_string()],
        database_url: env::var("DATABASE_URL")?,
    };

    // Setup database connection pool
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&config.database_url)
        .await?;
    info!("Connected to database");

    // Si fetch_once est activé, faire un seul cycle de récupération et sauvegarde
    if args.fetch_once {
        info!("Running in fetch-once mod...");
        
        if let Err(e) = fetch_and_save_all(&pool, &config.symbols).await {
            error!("Error during fetch-once cycle: {}", e);
        } else {
            info!("Fetch-once cycle complete.");
        }
    // Sinon lancer le serveur web et le fetch périodique
    } else {
        info!("Running all...");
        // Setup Axum web server
        let app = Router::new()
            .route("/latest-prices", get(get_latest_prices))
            .with_state(pool.clone());
        let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
        info!("Web server listening on {}", addr);
        info!("http://localhost:3000/latest-prices");
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let server = axum::serve(listener, app.into_make_service());

        // Convert to a boxed future
        let mut server_future = Box::pin(server.into_future());

        // Create interval for periodic fetching
        let mut fetch_interval = interval(Duration::from_secs(60));

        // Main loop
        info!("Service started. Waiting for interval ticks or shutdown signal...");
        loop {
            tokio::select! {
                _ = fetch_interval.tick() => {
                    info!("Fetch interval triggered");
                    if let Err(e) = fetch_and_save_all(&pool, &config.symbols).await {
                        error!("Error during fetch cycle: {}", e);
                    }
                }
                _ = signal::ctrl_c() => {
                    info!("Shutdown signal received");
                    break;
                }

                res = &mut server_future => {
                    match res {
                        Ok(_) => info!("Server shut down gracefully."),
                        Err(e) => error!("Server crashed: {}", e),
                    }
                    break;
                }
            }
        }
    }
    info!("Shutting down... Closing database connections.");
    pool.close().await;
    info!("Shutdown complete");

    Ok(())
}