//! geobox HTTP server. Configuration: `GEOBOX_*` env vars (see README).

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use axum::Router;
use axum::extract::{Query as QueryParams, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use geobox_server::{Config, Index, Query, SearchError};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

const ATTRIBUTION: &str = "Data © OpenStreetMap contributors, ODbL (https://www.openstreetmap.org/copyright)";

#[derive(Clone)]
struct App {
    /// Set once the index is open (after the prebuilt-index download on a first start).
    index: Arc<std::sync::OnceLock<Arc<Index>>>,
    metrics: PrometheusHandle,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_target(false).with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout())).init();
    let cfg = Config::from_env()?;
    // Few async threads: searches run on the blocking pool sized by `search_threads`.
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(cfg.search_threads + 4)
        .enable_all()
        .build()?
        .block_on(run(cfg))
}

async fn run(cfg: Config) -> anyhow::Result<()> {
    let metrics = PrometheusBuilder::new()
        .set_buckets(&[0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0])?
        .install_recorder()?;
    let bind = cfg.bind.clone();
    let poll = Duration::from_secs(cfg.poll_secs.max(10));
    // Serve (health checks, 503 for searches) right away; the index arrives in the background.
    let cell: Arc<std::sync::OnceLock<Arc<Index>>> = Arc::default();

    let slot = cell.clone();
    tokio::spawn(async move {
        // first start: install the published prebuilt index instead of indexing all tiles here
        match geobox_server::bootstrap(&cfg).await {
            Ok(true) | Ok(false) => {}
            Err(e) => warn!("prebuilt index unavailable ({e:#}): building the index from tiles"),
        }
        let index = match Index::open(cfg) {
            Ok(i) => i,
            Err(e) => {
                error!("cannot open the index: {e:#}");
                std::process::exit(1);
            }
        };
        info!("index opened: {} roads in {} tiles, ready={}", index.road_count(), index.tile_count(), index.is_ready());
        let _ = slot.set(index.clone());
        let sync_index = index;
        let mut backoff = Duration::from_secs(30);
        loop {
            let wait = match sync_index.sync().await {
                Ok(r) if r.failed == 0 => {
                    backoff = Duration::from_secs(30);
                    metrics::gauge!("geobox_last_sync_success_timestamp_seconds").set(unix_now());
                    poll
                }
                Ok(r) => {
                    warn!("sync: {} tiles failed, retrying in {backoff:?}", r.failed);
                    backoff
                }
                Err(e) => {
                    error!("sync failed: {e:#}, retrying in {backoff:?}");
                    backoff
                }
            };
            if wait == backoff {
                backoff = (backoff * 2).min(poll);
            }
            tokio::time::sleep(wait).await;
        }
    });

    let app = Router::new()
        .route("/v1/search", get(search))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics_handler))
        .with_state(App { index: cell, metrics })
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            StatusCode::GATEWAY_TIMEOUT,
            Duration::from_secs(10),
        ))
        .layer(tower_http::compression::CompressionLayer::new())
        .layer(tower_http::cors::CorsLayer::permissive())
        .layer(
            // 503 (overloaded / still syncing) is an expected answer, not an error worth an ERROR line;
            // real search failures are logged by the handler
            tower_http::trace::TraceLayer::new_for_http()
                .on_failure(tower_http::trace::DefaultOnFailure::new().level(tracing::Level::DEBUG)),
        );
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    info!("listening on {bind}");
    axum::serve(listener, app).with_graceful_shutdown(shutdown()).await?;
    info!("shut down");
    Ok(())
}

fn unix_now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

async fn shutdown() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! { _ = ctrl_c => {}, _ = term.recv() => {} }
    }
    #[cfg(not(unix))]
    let _ = ctrl_c.await;
}

#[derive(Deserialize)]
struct SearchParams {
    q: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    limit: Option<usize>,
    lang: Option<String>,
}

fn error(status: StatusCode, msg: &str) -> Response {
    (status, axum::Json(serde_json::json!({ "error": msg }))).into_response()
}

async fn search(
    State(app): State<App>,
    params: Result<QueryParams<SearchParams>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let started = Instant::now();
    let Ok(QueryParams(p)) = params else { return error(StatusCode::BAD_REQUEST, "invalid query parameters") };
    let q = p.q.unwrap_or_default();
    if q.trim().is_empty() || q.chars().count() > 200 {
        return error(StatusCode::BAD_REQUEST, "q is required (1-200 characters)");
    }
    let point = match (p.lat, p.lon) {
        (Some(lat), Some(lon)) if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) => Some((lat, lon)),
        (None, None) => None,
        _ => return error(StatusCode::BAD_REQUEST, "lat and lon must be given together, within -90..90 / -180..180"),
    };
    let limit = p.limit.unwrap_or(10);
    if !(1..=50).contains(&limit) {
        return error(StatusCode::BAD_REQUEST, "limit must be 1-50");
    }
    if p.lang
        .as_ref()
        .is_some_and(|l| l.len() > 12 || !l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    {
        return error(StatusCode::BAD_REQUEST, "invalid lang");
    }
    let index = match app.index.get() {
        Some(i) if i.is_ready() => i,
        // answering from a half-built index would return confidently wrong streets
        other => {
            let msg = match other {
                Some(i) => {
                    format!("initial data sync in progress ({} tiles loaded so far), retry later", i.tile_count())
                }
                None => "starting: downloading the prebuilt index, retry later".to_string(),
            };
            let mut r = error(StatusCode::SERVICE_UNAVAILABLE, &msg);
            r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("30"));
            return r;
        }
    };
    let result = index.search(Query { q, point, limit, lang: p.lang }).await;
    let status = match &result {
        Ok(_) => "ok",
        Err(SearchError::Overloaded) => "overloaded",
        Err(_) => "error",
    };
    metrics::counter!("geobox_search_requests_total", "status" => status).increment(1);
    metrics::histogram!("geobox_search_seconds").record(started.elapsed().as_secs_f64());
    match result {
        Ok(results) => axum::Json(serde_json::json!({
            "results": results,
            "data_timestamp": index.meta("osm_timestamp").and_then(|t| t.parse::<i64>().ok()),
            "attribution": ATTRIBUTION,
        }))
        .into_response(),
        Err(SearchError::Overloaded) => {
            let mut r = error(StatusCode::SERVICE_UNAVAILABLE, "overloaded, retry shortly");
            r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
            r
        }
        Err(SearchError::Other(e)) => {
            error!("search failed: {e:#}");
            error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

async fn readyz(State(app): State<App>) -> Response {
    match app.index.get() {
        Some(i) if i.is_ready() => (StatusCode::OK, "ready").into_response(),
        Some(_) => (StatusCode::SERVICE_UNAVAILABLE, "initial sync in progress").into_response(),
        None => (StatusCode::SERVICE_UNAVAILABLE, "starting: downloading the prebuilt index").into_response(),
    }
}

async fn metrics_handler(State(app): State<App>) -> String {
    app.metrics.run_upkeep();
    app.metrics.render()
}
