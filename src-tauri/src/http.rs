use crate::{model::*, state::AppState};
use axum::{
    extract::{DefaultBodyLimit, Path, State},
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
    let identity = app.public_identity();
    Json(
        json!({"protocolVersion":2,"hostId":identity.host_id,"displayName":identity.display_name,"publicKeyPin":identity.public_key_pin}),
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
fn copy_owner(app: &AppState, headers: &HeaderMap) -> Result<(ClientInfo, String)> {
    if headers
        .get("maxa-device-protocol")
        .and_then(|v| v.to_str().ok())
        != Some("2")
    {
        return Err("PROTOCOL_MISMATCH".into());
    }
    let client = auth(app, headers)?;
    let chat: String = serde_json::from_str(
        headers
            .get("maxa-device-chat")
            .and_then(|v| v.to_str().ok())
            .ok_or("copy chat required")?,
    )
    .map_err(|_| "invalid copy chat")?;
    if chat.is_empty() {
        return Err("copy chat required".into());
    }
    Ok((client, chat))
}
async fn copy_download(
    State(app): State<Arc<AppState>>,
    Path((id, index)): Path<(String, usize)>,
    headers: HeaderMap,
) -> Response {
    let prepared = async {
        let (client, chat) = copy_owner(&app, &headers)?;
        app.modules.ready("system").await?;
        let session = app.transfers.get(&client.id, &chat, &id)?;
        let source = session.source(&app.files, index)?;
        Ok::<_, String>((client, source))
    }
    .await;
    let (client, source) = match prepared {
        Ok(v) => v,
        Err(e) => return error(e),
    };
    let file = match source.file.try_clone() {
        Ok(v) => v,
        Err(e) => return error(e.to_string()),
    };
    let mut reader = tokio::fs::File::from_std(file);
    let length = source.expected();
    let stream = async_stream::try_stream! {
        use tokio::io::AsyncReadExt;
        use sha2::{Digest,Sha256};
        let failure=|e:String|std::io::Error::other(e);
        let mut buffer=vec![0u8;crate::transfer::BUFFER_BYTES];let mut bytes=0u64;let mut hash=Sha256::new();
        loop {
            source.check().map_err(failure)?;
            app.store.active(&client.id).map_err(failure)?;
            app.modules.ready("system").await.map_err(failure)?;
            let n=reader.read(&mut buffer).await?;if n==0{break}
            bytes+=n as u64;if bytes>length{Err(std::io::Error::other("COPY_SOURCE_CHANGED"))?;}
            hash.update(&buffer[..n]);
            yield axum::body::Bytes::copy_from_slice(&buffer[..n]);
        }
        source.finish(bytes,format!("{:x}",hash.finalize())).map_err(failure)?;
    };
    let stream: std::pin::Pin<
        Box<
            dyn futures_util::Stream<Item = std::result::Result<axum::body::Bytes, std::io::Error>>
                + Send,
        >,
    > = Box::pin(stream);
    // Chunked framing is intentional: source validation must finish before a successful EOF.
    (
        [("content-type", "application/octet-stream")],
        axum::body::Body::from_stream(stream),
    )
        .into_response()
}
async fn copy_upload(
    State(app): State<Arc<AppState>>,
    Path((id, index)): Path<(String, usize)>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Response {
    use futures_util::StreamExt;
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncWriteExt;
    let result = async {
        let (client, chat) = copy_owner(&app, &headers)?;
        app.modules.ready("system").await?;
        let session = app.transfers.get(&client.id, &chat, &id)?;
        let destination = session.destination(&app.files, &app.store, index)?;
        let file = destination.file.try_clone().map_err(|e| e.to_string())?;
        let mut writer = tokio::fs::File::from_std(file);
        let mut hash = Sha256::new();
        let mut bytes = 0u64;
        let mut stream = body.into_data_stream();
        loop {
            destination.check()?;
            app.store.active(&client.id)?;
            app.modules.ready("system").await?;
            let chunk = tokio::select! {
                value=stream.next()=>value,
                _=tokio::time::sleep(std::time::Duration::from_millis(250))=>continue,
            };
            let Some(chunk) = chunk else { break };
            let chunk = chunk.map_err(|e| e.to_string())?;
            bytes = bytes
                .checked_add(chunk.len() as u64)
                .ok_or("COPY_SIZE_OVERFLOW")?;
            if bytes > destination.expected() || bytes > crate::transfer::MAX_FILE_BYTES {
                return Err("COPY_SIZE_MISMATCH".into());
            }
            writer.write_all(&chunk).await.map_err(|e| e.to_string())?;
            hash.update(&chunk);
        }
        writer.flush().await.map_err(|e| e.to_string())?;
        drop(writer);
        destination.finish(bytes, format!("{:x}", hash.finalize()))
    }
    .await;
    match result {
        Ok(v) => Json(v).into_response(),
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
        .route("/v1/copy/{id}/{index}", get(copy_download).put(copy_upload))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .with_state(app.clone());
    let listener = std::net::TcpListener::bind(address).map_err(|e| e.to_string())?;
    let bound = listener.local_addr().map_err(|e| e.to_string())?;
    let discovery_error =
        match crate::discovery::Advertisement::publish(&app.public_identity(), bound.port()) {
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
