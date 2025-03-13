mod client;
#[cfg(all(feature = "integration", test))]
mod integration;
mod metrics;
mod rate_limit;
mod registry;
mod server;
mod subscriber;

use crate::metrics::Metrics;
use crate::rate_limit::InMemoryRateLimit;
use crate::registry::Registry;
use crate::server::Server;
use crate::subscriber::WebsocketSubscriber;
use axum::http::Uri;
use clap::Parser;
use dotenv::dotenv;
use metrics_exporter_prometheus::PrometheusBuilder;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, trace, Level};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    #[arg(
        long,
        env,
        default_value = "0.0.0.0:8545",
        help = "The address and port to listen on for incoming connections"
    )]
    listen_addr: SocketAddr,

    #[arg(long, env, help = "WebSocket URI of the upstream server to connect to")]
    upstream_ws: Vec<Uri>,

    #[arg(
        long,
        env,
        default_value = "20",
        help = "Number of messages to buffer for lagging clients"
    )]
    message_buffer_size: usize,

    #[arg(
        long,
        env,
        default_value = "100",
        help = "Maximum number of concurrently connected clients"
    )]
    global_connections_limit: usize,

    #[arg(
        long,
        env,
        default_value = "10",
        help = "Maximum number of concurrently connected clients"
    )]
    per_ip_connections_limit: usize,

    #[arg(
        long,
        env,
        default_value = "X-Forwarded-For",
        help = "Header to use to determine the clients origin IP"
    )]
    ip_addr_http_header: String,

    #[arg(long, env, default_value = "info")]
    log_level: Level,

    /// Format for logs, can be json or text
    #[arg(long, env, default_value = "text")]
    log_format: String,

    // Enable Prometheus metrics
    #[arg(long, env, default_value = "true")]
    metrics: bool,

    /// Address to run the metrics server on
    #[arg(long, env, default_value = "0.0.0.0:9000")]
    metrics_addr: SocketAddr,

    /// Maximum backoff allowed for upstream connections
    #[arg(long, env, default_value = "20")]
    subscriber_max_interval: u64,
}

#[tokio::main]
async fn main() {
    dotenv().ok();
    let args = Args::parse();

    let log_format = args.log_format.to_lowercase();
    let log_level = args.log_level.to_string();

    if log_format == "json" {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(EnvFilter::new(log_level))
            .with_ansi(false)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new(log_level))
            .with_ansi(false)
            .init();
    }

    if args.metrics {
        info!(
            message = "starting metrics server",
            address = args.metrics_addr.to_string()
        );

        PrometheusBuilder::new()
            .with_http_listener(args.metrics_addr)
            .install()
            .expect("failed to setup Prometheus endpoint")
    }

    // Validate that we have at least one upstream URI
    if args.upstream_ws.is_empty() {
        error!(message = "no upstream URIs provided");
        panic!("No upstream URIs provided");
    }

    info!(message = "using upstream URIs", uris = ?args.upstream_ws);

    let metrics = Arc::new(Metrics::default());
    let metrics_clone = metrics.clone();

    let (send, _rec) = broadcast::channel(args.message_buffer_size);
    let sender = send.clone();

    let listener = move |data: String| {
        trace!(message = "received data", data = data);
        // Subtract one from receiver count, as we have to keep one receiver open at all times (see _rec)
        // to avoid the channel being closed. However this is not an active client connection.
        metrics_clone
            .active_connections
            .set((send.receiver_count() - 1) as f64);

        match send.send(data) {
            Ok(_) => (),
            Err(e) => error!(message = "failed to send data", error = e.to_string()),
        }
    };

    let token = CancellationToken::new();
    let mut subscriber_tasks = Vec::new();

    // Start a subscriber for each upstream URI
    for (index, uri) in args.upstream_ws.iter().enumerate() {
        let uri_clone = uri.clone();
        let listener_clone = listener.clone();
        let token_clone = token.clone();
        let metrics_clone = metrics.clone();

        let mut subscriber = WebsocketSubscriber::new(
            uri_clone,
            listener_clone,
            args.subscriber_max_interval,
            metrics_clone,
        );
        
        let task = tokio::spawn(async move {
            info!(message = "starting subscriber", index = index, uri = uri_clone.to_string());
            subscriber.run(token_clone).await;
        });

        subscriber_tasks.push(task);
    }

    let registry = Registry::new(sender, metrics.clone());

    let rate_limiter = Arc::new(InMemoryRateLimit::new(
        args.global_connections_limit,
        args.per_ip_connections_limit,
    ));

    let server = Server::new(
        args.listen_addr,
        registry.clone(),
        metrics,
        rate_limiter,
        args.ip_addr_http_header,
    );
    let server_task = server.listen(token.clone());

    let mut interrupt = signal(SignalKind::interrupt()).unwrap();
    let mut terminate = signal(SignalKind::terminate()).unwrap();

    tokio::select! {
        _ = futures::future::join_all(subscriber_tasks) => {
            info!("all subscriber tasks terminated");
            token.cancel();
        },
        _ = server_task => {
            info!("server task terminated");
            token.cancel();
        }
        _ = interrupt.recv() => {
            info!("process interrupted, shutting down");
            token.cancel();
        }
        _ = terminate.recv() => {
            info!("process terminated, shutting down");
            token.cancel();
        }
    }
}
