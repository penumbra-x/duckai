mod client;
mod model;
mod route;
mod signal;

use crate::Result;
use crate::{config::Config, error::Error};
use axum::Json;
use axum::{
    extract::DefaultBodyLimit,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use axum_server::{tls_rustls::RustlsConfig, AddrIncomingConfig, Handle, HttpConfig};
use client::ClientLoadBalancer;
use serde::Serialize;
use std::{path::PathBuf, time::Duration};
use tower::limit::ConcurrencyLimitLayer;
use tower_http::{
    cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer},
    trace::{DefaultMakeSpan, DefaultOnFailure, DefaultOnResponse, TraceLayer},
};
use tracing::Level;
use tracing_subscriber::{EnvFilter, FmtSubscriber};
use typed_builder::TypedBuilder;

#[tokio::main]
pub async fn run(path: PathBuf) -> Result<()> {
    // init config
    let config = init_config(path).await?;

    // init logger
    init_logger(config.debug)?;

    // init boot message
    boot_message(&config);

    // init global layer provider
    let global_layer = tower::ServiceBuilder::new()
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO))
                .on_failure(DefaultOnFailure::new().level(Level::WARN)),
        )
        .layer(
            CorsLayer::new()
                .allow_credentials(true)
                .allow_headers(AllowHeaders::mirror_request())
                .allow_methods(AllowMethods::mirror_request())
                .allow_origin(AllowOrigin::mirror_request()),
        )
        .layer(DefaultBodyLimit::max(209715200))
        .layer(ConcurrencyLimitLayer::new(config.concurrent));

    let router = Router::new()
        .route("/ping", get(route::ping))
        .route("/v1/models", get(route::models))
        .route("/v1/chat/completions", post(route::chat_completions))
        .fallback(route::manual_hello)
        .with_state(ClientLoadBalancer::new(config.clone()).await)
        .layer(global_layer);

    // Signal the server to shutdown using Handle.
    let handle = Handle::new();

    // Spawn a task to gracefully shutdown server.
    tokio::spawn(signal::graceful_shutdown(handle.clone()));

    // http server tcp keepalive
    let tcp_keepalive = config.tcp_keepalive.map(Duration::from_secs);

    // http server config
    let http_config = HttpConfig::new()
        .http1_title_case_headers(true)
        .http1_preserve_header_case(true)
        .http2_keep_alive_interval(tcp_keepalive)
        .build();

    // http server incoming config
    let incoming_config = AddrIncomingConfig::new()
        .tcp_sleep_on_accept_errors(true)
        .tcp_keepalive(tcp_keepalive)
        .build();

    // Run http server
    match (config.tls_cert.as_ref(), config.tls_key.as_ref()) {
        (Some(cert), Some(key)) => {
            // Load TLS configuration
            let tls_config = RustlsConfig::from_pem_file(cert, key).await?;

            // Use TLS configuration to create a secure server
            axum_server::bind_rustls(config.bind, tls_config)
                .handle(handle)
                .addr_incoming_config(incoming_config)
                .http_config(http_config)
                .serve(router.into_make_service())
                .await
        }
        _ => {
            // No TLS configuration, create a non-secure server
            axum_server::bind(config.bind)
                .handle(handle)
                .addr_incoming_config(incoming_config)
                .http_config(http_config)
                .serve(router.into_make_service())
                .await
        }
    }
    .map_err(Into::into)
}

/// Print boot info message
fn boot_message(config: &Config) {
    // Server info
    tracing::info!("OS: {}", std::env::consts::OS);
    tracing::info!("Arch: {}", std::env::consts::ARCH);
    tracing::info!("Version: {}", env!("CARGO_PKG_VERSION"));
    tracing::info!("Timeout {} seconds", config.timeout);
    tracing::info!("Connect timeout {} seconds", config.connect_timeout);
    if let Some(tcp_keepalive) = config.tcp_keepalive {
        tracing::info!("Keepalive {} seconds", tcp_keepalive);
    }
    tracing::info!("Concurrent limit: {}", config.concurrent);
    config
        .proxies
        .iter()
        .for_each(|p| tracing::info!("Proxy: {:?}", p));
    tracing::info!("Bind address: {}", config.bind);
}

/// Initialize the logger with a filter that ignores WARN level logs for netlink_proto
fn init_logger(debug: bool) -> Result<()> {
    let filter = EnvFilter::from_default_env()
        .add_directive(if debug { Level::DEBUG } else { Level::INFO }.into())
        .add_directive("netlink_proto=error".parse()?);

    tracing::subscriber::set_global_default(
        FmtSubscriber::builder().with_env_filter(filter).finish(),
    )?;

    Ok(())
}

/// Init configuration
async fn init_config(path: PathBuf) -> Result<Config> {
    if !path.is_file() {
        tracing::warn!("Using the default configuration");
        return Ok(Config::default());
    }

    let data = tokio::fs::read(path).await?;
    serde_yaml::from_slice::<Config>(&data).map_err(Into::into)
}

impl IntoResponse for Error {
    fn into_response(self) -> axum::response::Response {
        #[derive(Serialize)]
        struct RootError {
            error: ResponseError,
        }

        #[derive(Serialize, TypedBuilder)]
        struct ResponseError {
            message: String,
            #[serde(rename = "type")]
            type_field: &'static str,
            #[builder(default)]
            param: Option<String>,
            #[builder(default)]
            code: Option<String>,
        }

        match self {
            Error::JsonExtractorRejection(json_rejection) => (
                StatusCode::BAD_REQUEST,
                Json(RootError {
                    error: ResponseError::builder()
                        .message(json_rejection.body_text())
                        .type_field("invalid_request_error")
                        .build(),
                }),
            )
                .into_response(),
            _ => (
                StatusCode::BAD_REQUEST,
                Json(RootError {
                    error: ResponseError::builder()
                        .message(self.to_string())
                        .type_field("server_error")
                        .build(),
                }),
            )
                .into_response(),
        }
    }
}
