use crate::{model::*, state::AppState};
use axum::{
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::{net::SocketAddr, sync::Arc};
fn error(e: String) -> Response {
    let status = if e == "UNAUTHORIZED" {
        StatusCode::UNAUTHORIZED
    } else {
        StatusCode::BAD_REQUEST
    };
    (status, Json(json!({"error":e}))).into_response()
}
fn auth(app: &AppState, headers: &HeaderMap) -> Result<ClientInfo> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .ok_or("UNAUTHORIZED")?;
    app.store.authenticate(token)
}
async fn public(State(app): State<Arc<AppState>>) -> Json<Value> {
    Json(
        json!({"protocolVersion":2,"hostId":app.identity.public.host_id,"displayName":app.identity.public.display_name,"publicKeyPin":app.identity.public.public_key_pin}),
    )
}
async fn pair(State(app): State<Arc<AppState>>, Json(args): Json<Value>) -> Response {
    match app.pair_request(&args).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => error(e),
    }
}
async fn poll(State(app): State<Arc<AppState>>, Json(args): Json<Value>) -> Response {
    match app.poll_pair(&args).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => error(e),
    }
}
async fn cancel_pair(State(app): State<Arc<AppState>>, Json(args): Json<Value>) -> Response {
    match app.cancel_pair(&args).await {
        Ok(()) => Json(json!({"cancelled":true})).into_response(),
        Err(e) => error(e),
    }
}
async fn invoke(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(call): Json<Invocation>,
) -> Response {
    if headers
        .get("maxa-device-protocol")
        .and_then(|v| v.to_str().ok())
        != Some("2")
    {
        return error("PROTOCOL_MISMATCH: update App and connector to protocol 2".into());
    }
    let client = match auth(&app, &headers) {
        Ok(c) => c,
        Err(e) => return error(e),
    };
    match app.invoke(&client, &call).await {
        Ok(v) => Json(json!({"hostId":app.identity.public.host_id,"value":v})).into_response(),
        Err(e) => error(e),
    }
}
async fn artifact(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(args): Json<Value>,
) -> Response {
    let client = match auth(&app, &headers) {
        Ok(c) => c,
        Err(e) => return error(e),
    };
    if let Some(reference) = args.get("reference") {
        let Some(chat) = args["chatId"].as_str() else {
            return error("chatId required".into());
        };
        return match app.artifacts.get(&client.id, chat, reference) {
            Ok(bytes) => ([("content-type", "application/octet-stream")], bytes).into_response(),
            Err(e) => error(e),
        };
    }
    let (Some(chat), Some(job), Some(stream)) = (
        args["chatId"].as_str(),
        args["jobId"].as_str(),
        args["stream"].as_str(),
    ) else {
        return error("chatId/jobId/stream required".into());
    };
    match app.jobs.output(&client.id, chat, job, stream).await {
        Ok(v) => ([("content-type", "application/octet-stream")], v).into_response(),
        Err(e) => error(e),
    }
}
pub async fn serve(
    app: Arc<AppState>,
    address: SocketAddr,
    handle: axum_server::Handle,
) -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let tls = axum_server::tls_rustls::RustlsConfig::from_pem(
        app.identity.certificate.as_bytes().to_vec(),
        app.identity.private_key.as_bytes().to_vec(),
    )
    .await
    .map_err(|e| e.to_string())?;
    let router = Router::new()
        .route("/v1/identity", get(public))
        .route(
            "/v1/pair",
            post(pair).layer(DefaultBodyLimit::max(16 * 1024)),
        )
        .route(
            "/v1/pair/status",
            post(poll).layer(DefaultBodyLimit::max(4096)),
        )
        .route(
            "/v1/pair/cancel",
            post(cancel_pair).layer(DefaultBodyLimit::max(4096)),
        )
        .route("/v1/invoke", post(invoke))
        .route("/v1/artifact", post(artifact))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .with_state(app.clone());
    let listener = std::net::TcpListener::bind(address).map_err(|e| e.to_string())?;
    let bound = listener.local_addr().map_err(|e| e.to_string())?;
    let discovery_error =
        match crate::discovery::Advertisement::publish(&app.identity.public, bound.port()) {
            Ok(ad) => {
                *app.advertisement.lock().await = Some(ad);
                None
            }
            Err(_) => Some(
                "LAN discovery unavailable; manual IP:Port/HTTPS pairing remains available"
                    .to_string(),
            ),
        };
    // TLS identity, never the mDNS advertisement, is the trust root. No CORS is enabled.
    {
        let mut status = app.service.lock().await;
        status.state = "ready".into();
        status.address = Some(format!("https://{bound}"));
        status.error = discovery_error;
    }
    axum_server::from_tcp_rustls(listener, tls)
        .handle(handle)
        .serve(router.into_make_service())
        .await
        .map_err(|e| e.to_string())
}
