//! axum server: server-rendered HTML (maud) + htmx, uPlot charts, SSE change ticks.

mod fmt;
mod pages;
mod queries;

use std::{collections::HashMap, convert::Infallible, sync::Arc, time::Duration};

use axum::{
    Router,
    body::Body,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{
        Html, IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::get,
};
use base64::Engine;
use futures_util::stream::Stream;
use rust_embed::RustEmbed;

use crate::app::App;

#[derive(RustEmbed)]
#[folder = "assets/"]
struct Assets;

type Params = Query<HashMap<String, String>>;

pub fn router(app: Arc<App>) -> Router {
    let auth = app.cfg.web.basic_auth.clone();
    Router::new()
        .route("/", get(overview))
        .route("/relays", get(relays))
        .route("/requests", get(requests))
        .route("/requests/{id}", get(request_detail))
        .route("/profit", get(profit))
        .route("/market", get(market))
        .route("/events", get(events))
        .route("/sse", get(sse))
        .route("/healthz", get(|| async { "ok" }))
        .route("/assets/{*path}", get(asset))
        .layer(middleware::from_fn(security_headers))
        .layer(middleware::from_fn(move |req: Request, next: Next| {
            let auth = auth.clone();
            async move { basic_auth(auth.as_deref(), req, next).await }
        }))
        .with_state(app)
}

pub async fn serve(app: Arc<App>) -> anyhow::Result<()> {
    let bind = app.cfg.web.bind;
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "web UI listening");
    axum::serve(listener, router(app)).await?;
    Ok(())
}

async fn basic_auth(expected: Option<&str>, req: Request, next: Next) -> Response {
    let Some(expected) = expected else {
        return next.run(req).await;
    };
    if req.uri().path() == "/healthz" {
        return next.run(req).await;
    }
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b.trim()).ok())
        .is_some_and(|got| constant_time_eq(&got, expected.as_bytes()));
    if ok {
        next.run(req).await
    } else {
        Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(header::WWW_AUTHENTICATE, "Basic realm=\"rbm\", charset=\"UTF-8\"")
            .body(Body::from("unauthorized"))
            .unwrap()
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn security_headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
             connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    );
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    res
}

async fn asset(Path(path): Path<String>) -> Response {
    match Assets::get(&path) {
        Some(f) => {
            let mime = match path.rsplit('.').next() {
                Some("js") => "text/javascript; charset=utf-8",
                Some("css") => "text/css; charset=utf-8",
                Some("svg") => "image/svg+xml",
                _ => "application/octet-stream",
            };
            ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "public, max-age=3600")], f.data.into_owned())
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn page(res: anyhow::Result<maud::Markup>) -> Response {
    match res {
        Ok(m) => Html(m.into_string()).into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "page render failed");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("error: {e}")).into_response()
        }
    }
}

fn is_htmx(h: &HeaderMap) -> bool {
    h.contains_key("hx-request")
}

async fn overview(State(app): State<Arc<App>>, h: HeaderMap) -> Response {
    page(pages::overview(&app, is_htmx(&h)).await)
}
async fn relays(State(app): State<Arc<App>>, q: Params, h: HeaderMap) -> Response {
    page(pages::relays(&app, &q.0, is_htmx(&h)).await)
}
async fn requests(State(app): State<Arc<App>>, q: Params, h: HeaderMap) -> Response {
    page(pages::requests(&app, &q.0, is_htmx(&h)).await)
}
async fn request_detail(State(app): State<Arc<App>>, Path(id): Path<i64>) -> Response {
    page(pages::request_detail(&app, id).await)
}
async fn profit(State(app): State<Arc<App>>, q: Params, h: HeaderMap) -> Response {
    page(pages::profit(&app, &q.0, is_htmx(&h)).await)
}
async fn market(State(app): State<Arc<App>>, h: HeaderMap) -> Response {
    page(pages::market(&app, is_htmx(&h)).await)
}
async fn events(State(app): State<Arc<App>>, q: Params, h: HeaderMap) -> Response {
    page(pages::events(&app, &q.0, is_htmx(&h)).await)
}

/// SSE: one event per changed topic, coalesced to at most one burst per 2 s.
async fn sse(State(app): State<Arc<App>>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = app.events.subscribe();
    let stream = async_stream(move |tx| async move {
        loop {
            let first = match rx.recv().await {
                Ok(t) => t,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => "health",
                Err(_) => return,
            };
            let mut topics = std::collections::BTreeSet::from([first]);
            let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            while let Ok(Ok(t)) = tokio::time::timeout_at(deadline, rx.recv()).await {
                topics.insert(t);
            }
            for t in topics {
                if tx.send(Ok(Event::default().event(t).data(t))).await.is_err() {
                    return;
                }
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(20)))
}

/// Tiny channel-backed stream helper (avoids pulling async-stream).
fn async_stream<T, F, Fut>(f: F) -> impl Stream<Item = T>
where
    T: Send + 'static,
    F: FnOnce(tokio::sync::mpsc::Sender<T>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    tokio::spawn(f(tx));
    tokio_stream::wrappers::ReceiverStream::new(rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq() {
        assert!(constant_time_eq(b"a:b", b"a:b"));
        assert!(!constant_time_eq(b"a:b", b"a:c"));
        assert!(!constant_time_eq(b"a:b", b"a:bc"));
    }
}
