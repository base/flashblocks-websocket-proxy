use crate::client::ClientConnection;
use crate::metrics::Metrics;
use crate::rate_limit::{RateLimit, RateLimitError};
use crate::registry::Registry;
use axum::body::Body;
use axum::extract::{ConnectInfo, Path, State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Error, Router};
use http::{HeaderMap, HeaderValue};
use metrics::counter;
use serde_json::json;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

#[derive(Clone)]
struct ServerState {
    registry: Registry,
    rate_limiter: Arc<dyn RateLimit>,
    metrics: Arc<Metrics>,
    ip_addr_http_header: String,
    api_keys: Vec<String>,
}

#[derive(Clone)]
pub struct Server {
    listen_addr: SocketAddr,
    registry: Registry,
    rate_limiter: Arc<dyn RateLimit>,
    metrics: Arc<Metrics>,
    ip_addr_http_header: String,
    api_keys: Vec<String>,
}

impl Server {
    pub fn new(
        listen_addr: SocketAddr,
        registry: Registry,
        metrics: Arc<Metrics>,
        rate_limiter: Arc<dyn RateLimit>,
        ip_addr_http_header: String,
        api_keys: Vec<String>,
    ) -> Self {
        Self {
            listen_addr,
            registry,
            rate_limiter,
            metrics,
            ip_addr_http_header,
            api_keys,
        }
    }

    pub async fn listen(&self, cancellation_token: CancellationToken) {
        let router = Router::new()
            .route("/healthz", get(healthz_handler))
            .route("/ws", any(websocket_handler))
            .route("/ws/{api_key}", any(websocket_handler_with_key))
            .with_state(ServerState {
                registry: self.registry.clone(),
                rate_limiter: self.rate_limiter.clone(),
                metrics: self.metrics.clone(),
                ip_addr_http_header: self.ip_addr_http_header.clone(),
                api_keys: self.api_keys.clone(),
            });

        let listener = tokio::net::TcpListener::bind(self.listen_addr)
            .await
            .unwrap();

        info!(
            message = "starting server",
            address = listener.local_addr().unwrap().to_string()
        );

        if self.api_keys.is_empty() {
            info!(message = "API key authentication is disabled");
        } else {
            info!(
                message = "API key authentication is enabled",
                key_count = self.api_keys.len()
            );
            info!(message = "API keys", keys = ?self.api_keys);
        }

        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(cancellation_token.cancelled_owned())
        .await
        .unwrap()
    }
}

async fn healthz_handler() -> impl IntoResponse {
    StatusCode::OK
}

async fn websocket_handler(
    State(state): State<ServerState>,
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    // If API keys are required but none provided in URL path, reject
    if !state.api_keys.is_empty() {
        state.metrics.unauthorized_requests.increment(1);
        return Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .body(Body::from(
                json!({"message": "API key required"}).to_string(),
            ))
            .unwrap();
    }

    handle_websocket_connection(state, ws, addr, headers, None)
}

async fn websocket_handler_with_key(
    State(state): State<ServerState>,
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(api_key): Path<String>,
) -> Response {
    // If API keys are required, validate the provided key
    if !state.api_keys.is_empty() && !state.api_keys.contains(&api_key) {
        state.metrics.unauthorized_requests.increment(1);
        return Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .body(Body::from(
                json!({"message": "Invalid API key"}).to_string(),
            ))
            .unwrap();
    }

    handle_websocket_connection(state, ws, addr, headers, Some(api_key))
}

// Common handler logic for both authenticated and unauthenticated paths
fn handle_websocket_connection(
    state: ServerState,
    ws: WebSocketUpgrade,
    addr: SocketAddr,
    headers: HeaderMap,
    api_key: Option<String>, // Track this API key in metrics
) -> Response {
    let connect_addr = addr.ip();

    let client_addr = match headers.get(state.ip_addr_http_header) {
        None => connect_addr,
        Some(value) => extract_addr(value, connect_addr),
    };

    let ticket = match state.rate_limiter.try_acquire(client_addr) {
        Ok(ticket) => ticket,
        Err(RateLimitError::Limit { reason }) => {
            state.metrics.rate_limited_requests.increment(1);

            return Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .body(Body::from(json!({"message": reason}).to_string()))
                .unwrap();
        }
    };

    // Record API key usage with a label for tracking
    let key_value = match api_key.clone() {
        Some(key) => {
            // For security, only use the first 8 chars of the API key in metrics
            if key.len() > 8 {
                format!("{}...", &key[0..8])
            } else {
                key
            }
        }
        None => "none".to_string(),
    };
    counter!("websocket_proxy.connections_by_api_key", "key" => key_value).increment(1);

    ws.on_failed_upgrade(move |e: Error| {
        info!(
            message = "failed to upgrade connection",
            error = e.to_string(),
            client = addr.to_string()
        )
    })
    .on_upgrade(async move |socket| {
        let client = ClientConnection::new(client_addr, ticket, socket);
        state.registry.subscribe(client).await;
    })
}

fn extract_addr(header: &HeaderValue, fallback: IpAddr) -> IpAddr {
    if header.is_empty() {
        return fallback;
    }

    match header.to_str() {
        Ok(header_value) => {
            let raw_value = header_value
                .split(',')
                .map(|ip| ip.trim().to_string())
                .last();

            if let Some(raw_value) = raw_value {
                return raw_value.parse::<IpAddr>().unwrap_or(fallback);
            }

            fallback
        }
        Err(e) => {
            warn!(
                message = "could not get header value",
                error = e.to_string()
            );
            fallback
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[tokio::test]
    async fn test_header_addr() {
        let fb = Ipv4Addr::new(127, 0, 0, 1);

        let test = |header: &str, expected: Ipv4Addr| {
            let hv = HeaderValue::from_str(header).unwrap();
            let result = extract_addr(&hv, IpAddr::V4(fb));
            assert_eq!(result, expected);
        };

        test("129.1.1.1", Ipv4Addr::new(129, 1, 1, 1));
        test("129.1.1.1,130.1.1.1", Ipv4Addr::new(130, 1, 1, 1));
        test("129.1.1.1  ,  130.1.1.1   ", Ipv4Addr::new(130, 1, 1, 1));
        test("nonsense", fb);
        test("400.0.0.1", fb);
        test("120.0.0.1.0", fb);
    }
}
