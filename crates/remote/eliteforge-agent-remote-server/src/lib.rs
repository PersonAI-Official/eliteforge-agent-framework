//! Axum remote listener: pairing, device registry, Noise secure channel and RPC dispatch.

use async_trait::async_trait;
use axum::{
    extract::{
        ConnectInfo, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use eliteforge_agent_remote_protocol::{
    AuthDevice, DeviceRecord, DeviceSummary, IDENTITY_PATH, NOISE_SUITE, NodeIdentity, PairingView,
    ProductIdentity, RemoteIdentity, RemoteSettings, RemoteStatus, SecureTransportAdvert,
    TicketRecord, TransportMode, WireFrame, generate_device_id, generate_node_id, generate_ticket,
    hash_secret, local_lan_ip, node_fingerprint, node_key_id, noise_prologue, now_ms,
    pairing_uri_with_endpoints,
};
use eliteforge_agent_remote_rpc::{
    EventEmitter, RemotePack, RemotePrincipal, RpcError, RpcRegistry, SessionCreateParams,
    SessionHistoryParams, SessionListParams, SessionRpc, SessionSendMessageParams,
    SessionStopRunParams, SessionUnwatchEventsParams, SessionWatchEventsParams,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::warn;

mod file_store;
pub use file_store::FileRemoteStore;

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("remote access is not configured: {0}")]
    Store(String),
    #[error("invalid key material: {0}")]
    Key(String),
    #[error("noise handshake failed: {0}")]
    Noise(String),
    #[error("remote access is disabled")]
    Disabled,
    #[error("device not found")]
    DeviceNotFound,
    #[error("invalid ticket")]
    InvalidTicket,
    #[error("invalid frame: {0}")]
    Frame(String),
    #[error("rpc failed: {0}")]
    Rpc(String),
}

impl IntoResponse for RemoteError {
    fn into_response(self) -> Response {
        let status = match self {
            Self::Disabled => axum::http::StatusCode::FORBIDDEN,
            Self::DeviceNotFound => axum::http::StatusCode::FORBIDDEN,
            Self::InvalidTicket => axum::http::StatusCode::UNAUTHORIZED,
            _ => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

#[async_trait]
pub trait RemoteStore: Send + Sync {
    async fn load_settings(&self) -> Result<RemoteSettings, String>;
    async fn save_settings(&self, settings: &RemoteSettings) -> Result<(), String>;
    async fn load_node_identity(&self) -> Result<Option<NodeIdentity>, String>;
    async fn save_node_identity(&self, identity: &NodeIdentity) -> Result<(), String>;
    async fn load_devices(&self) -> Result<Vec<DeviceRecord>, String>;
    async fn save_devices(&self, devices: &[DeviceRecord]) -> Result<(), String>;
}

#[async_trait]
pub trait RemoteProduct: Send + Sync {
    fn product(&self) -> ProductIdentity;
    fn profile(&self) -> Option<eliteforge_agent_remote_rpc::ProfileSummary>;
}

struct TicketState {
    raw: String,
    record: TicketRecord,
}

pub struct RemoteAccessService {
    product: Arc<dyn RemoteProduct>,
    store: Arc<dyn RemoteStore>,
    registry: RpcRegistry,
    settings: RwLock<RemoteSettings>,
    node: RwLock<NodeIdentity>,
    devices: RwLock<Vec<DeviceRecord>>,
    ticket: RwLock<Option<TicketState>>,
    active: AtomicBool,
    port: u16,
}

impl RemoteAccessService {
    pub async fn start(
        product: Arc<dyn RemoteProduct>,
        store: Arc<dyn RemoteStore>,
        pack: Arc<dyn RemotePack>,
        port: u16,
    ) -> Result<Arc<Self>, RemoteError> {
        let settings = store.load_settings().await.map_err(RemoteError::Store)?;
        let node = match store.load_node_identity().await.map_err(RemoteError::Store)? {
            Some(node) => node,
            None => {
                let identity = generate_server_node_identity()?;
                store
                    .save_node_identity(&identity)
                    .await
                    .map_err(RemoteError::Store)?;
                identity
            }
        };
        let devices = store.load_devices().await.map_err(RemoteError::Store)?;
        let mut registry = RpcRegistry::new();
        if let Some(session) = pack.session_rpc() {
            registry.set_session_rpc(session);
        }
        pack.register(&mut registry);
        let service = Arc::new(Self {
            product,
            store,
            registry,
            settings: RwLock::new(settings),
            node: RwLock::new(node),
            devices: RwLock::new(devices),
            ticket: RwLock::new(None),
            active: AtomicBool::new(false),
            port,
        });
        if service.settings.read().await.enabled {
            service.set_enabled(true).await?;
        }
        Ok(service)
    }

    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/health", get(health))
            .route(IDENTITY_PATH, get(identity))
            .route(eliteforge_agent_remote_protocol::SECURE_PATH, get(secure_socket))
            .with_state(self)
    }

    pub async fn status(&self) -> RemoteStatus {
        let settings = self.settings.read().await.clone();
        let node = self.node.read().await.clone();
        let devices = self.devices.read().await;
        let ticket = self.ticket.read().await;
        let base_url = self.base_url(&settings).await;
        let lan_base_url = self.lan_base_url(&settings).await;
        let pairing_ticket_expires_at = ticket.as_ref().map(|t| t.record.expires_at_ms);
        RemoteStatus {
            enabled: settings.enabled,
            active: self.active.load(Ordering::Relaxed),
            transport: settings.transport(),
            base_url: base_url.clone(),
            lan_base_url,
            public_base_url: settings.public_base_url.clone(),
            node_key_fingerprint: node_fingerprint(&node.public_key),
            paired_device_count: devices.iter().filter(|d| !d.revoked).count(),
            pairing_ticket_expires_at,
            pairing_uri: ticket.as_ref().map(|t| {
                let lan = self.lan_base_url_sync(&settings);
                pairing_uri_with_endpoints(
                    &base_url,
                    Some(&lan),
                    (!settings.public_base_url.trim().is_empty()).then_some(settings.public_base_url.as_str()),
                    &settings.product_code,
                    &settings.product_name,
                    &node.node_id,
                    &settings.machine_name,
                    &node.public_key,
                    &node.key_id,
                    &t.raw,
                )
                .unwrap_or_default()
            }),
            restart_required: settings.enabled && !self.active.load(Ordering::Relaxed),
        }
    }

    pub async fn identity(&self) -> RemoteIdentity {
        let settings = self.settings.read().await.clone();
        let node = self.node.read().await.clone();
        let mut product = self.product.product();
        product.node_id = node.node_id.clone();
        let lan_base_url = self.lan_base_url(&settings).await;
        RemoteIdentity {
            protocol: eliteforge_agent_remote_protocol::PROTOCOL.into(),
            product_code: product.product_code,
            product_name: product.product_name,
            node_id: node.node_id,
            machine_name: settings.machine_name.clone(),
            version: product.version,
            platform: product.platform,
            base_url: self.base_url(&settings).await,
            lan_base_url,
            public_base_url: settings.public_base_url.clone(),
            secure_transport: SecureTransportAdvert {
                cipher: NOISE_SUITE.into(),
                endpoint: eliteforge_agent_remote_protocol::SECURE_PATH.into(),
                key_id: node.key_id,
                public_key: node.public_key,
            },
        }
    }

    pub async fn status_json(&self) -> Value {
        serde_json::to_value(self.status().await).unwrap_or(Value::Null)
    }

    pub async fn pairing_json(&self) -> Value {
        let status = self.status().await;
        let node = self.node.read().await.clone();
        json!({
            "productCode": self.product.product().product_code,
            "productName": self.product.product().product_name,
            "nodeId": node.node_id,
            "machineName": self.settings.read().await.machine_name,
            "baseUrl": status.base_url,
            "lanBaseUrl": status.lan_base_url,
            "publicBaseUrl": status.public_base_url,
            "transport": status.transport,
            "nodeKeyFingerprint": status.node_key_fingerprint,
            "pairedDeviceCount": status.paired_device_count,
            "pairingTicketExpiresAt": status.pairing_ticket_expires_at,
            "tokenVisibleToAgent": false,
            "instructions": "请在桌面端设置页查看配对二维码；Agent 不会显示、复述或转发 ticket。"
        })
    }

    pub async fn refresh_pairing(&self) -> Result<PairingView, RemoteError> {
        let settings = self.settings.read().await.clone();
        let (raw, record, expires) = generate_ticket();
        *self.ticket.write().await = Some(TicketState { raw: raw.clone(), record });
        let node = self.node.read().await.clone();
        let base_url = self.base_url(&settings).await;
        let lan_base_url = self.lan_base_url(&settings).await;
        Ok(PairingView {
            uri: pairing_uri_with_endpoints(
                &base_url,
                Some(&lan_base_url),
                (!settings.public_base_url.trim().is_empty()).then_some(settings.public_base_url.as_str()),
                &settings.product_code,
                &settings.product_name,
                &node.node_id,
                &settings.machine_name,
                &node.public_key,
                &node.key_id,
                &raw,
            )
            .map_err(|e| RemoteError::Frame(e.to_string()))?,
            expires_at_ms: expires,
            transport: settings.transport(),
        })
    }

    pub async fn update_settings(&self, next: RemoteSettings) -> Result<RemoteStatus, RemoteError> {
        if next.is_public() == false && !next.public_base_url.trim().is_empty() {
            return Err(RemoteError::Store("publicBaseUrl must start with https://".into()));
        }
        self.store
            .save_settings(&next)
            .await
            .map_err(RemoteError::Store)?;
        *self.settings.write().await = next;
        if self.settings.read().await.enabled {
            self.refresh_pairing().await.ok();
        }
        Ok(self.status().await)
    }

    pub async fn set_enabled(&self, enabled: bool) -> Result<RemoteStatus, RemoteError> {
        let mut settings = self.settings.read().await.clone();
        settings.enabled = enabled;
        self.store
            .save_settings(&settings)
            .await
            .map_err(RemoteError::Store)?;
        *self.settings.write().await = settings;
        // The listener supervisor owns `active`: enabling settings is not the
        // same as having successfully bound the remote listener.
        if !enabled {
            self.active.store(false, Ordering::Relaxed);
            *self.ticket.write().await = None;
        }
        if enabled {
            self.refresh_pairing().await.ok();
        }
        Ok(self.status().await)
    }

    /// Records whether the dedicated listener is currently bound. Called by
    /// the host listener supervisor, never by settings changes directly.
    pub fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Relaxed);
    }

    /// Snapshot of the current service settings for host-side synchronization.
    pub async fn current_settings(&self) -> RemoteSettings {
        self.settings.read().await.clone()
    }

    pub async fn devices(&self) -> Vec<DeviceSummary> {
        self.devices
            .read()
            .await
            .iter()
            .map(DeviceRecord::summary)
            .collect()
    }

    pub async fn revoke_device(&self, device_id: &str) -> Result<(), RemoteError> {
        let mut devices = self.devices.write().await;
        let Some(device) = devices.iter_mut().find(|d| d.device_id == device_id) else {
            return Err(RemoteError::DeviceNotFound);
        };
        device.revoked = true;
        let snapshot = devices.clone();
        drop(devices);
        self.store
            .save_devices(&snapshot)
            .await
            .map_err(RemoteError::Store)
    }

    pub async fn revoke_all(&self) -> Result<(), RemoteError> {
        let mut devices = self.devices.write().await;
        for device in devices.iter_mut() {
            device.revoked = true;
        }
        let snapshot = devices.clone();
        drop(devices);
        self.refresh_pairing().await.ok();
        self.store
            .save_devices(&snapshot)
            .await
            .map_err(RemoteError::Store)
    }

    /// Permanently removes a pairing record. Revoking keeps the row for audit
    /// and blocks the device; removing drops the record after the user cleans
    /// up a stale pairing.
    pub async fn remove_device(&self, device_id: &str) -> Result<(), RemoteError> {
        let mut devices = self.devices.write().await;
        let original = devices.len();
        devices.retain(|device| device.device_id != device_id);
        if devices.len() == original {
            return Err(RemoteError::DeviceNotFound);
        }
        let snapshot = devices.clone();
        drop(devices);
        self.store
            .save_devices(&snapshot)
            .await
            .map_err(RemoteError::Store)
    }

    pub async fn shutdown(&self) {
        self.active.store(false, Ordering::Relaxed);
        *self.ticket.write().await = None;
    }

    pub fn methods(&self) -> Vec<String> {
        self.registry.method_names()
    }

    fn lan_base_url_sync(&self, settings: &RemoteSettings) -> String {
        if !settings.lan_base_url.trim().is_empty() {
            return settings.lan_base_url.trim().trim_end_matches('/').to_string();
        }
        let host = local_lan_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "127.0.0.1".into());
        format!("http://{host}:{}", self.port)
    }

    async fn lan_base_url(&self, settings: &RemoteSettings) -> String {
        self.lan_base_url_sync(settings)
    }

    async fn base_url(&self, settings: &RemoteSettings) -> String {
        if settings.is_public() {
            return settings
                .public_base_url
                .trim()
                .trim_end_matches('/')
                .to_string();
        }
        self.lan_base_url_sync(settings)
    }

    /// Classify a request as public (Host matches publicBaseUrl and a trusted
    /// proxy terminated TLS) or LAN. A configured public URL no longer makes
    /// the node public-only: the same node must accept the LAN candidate while
    /// the phone is at home and the public candidate when it leaves.
    async fn transport_for(
        &self,
        headers: &HeaderMap,
        remote: std::net::SocketAddr,
    ) -> Result<TransportMode, String> {
        let settings = self.settings.read().await.clone();
        if settings.public_base_url.trim().is_empty() {
            return Ok(TransportMode::Lan);
        }
        let public = url::Url::parse(&settings.public_base_url).map_err(|_| "invalid publicBaseUrl")?;
        let expected_host = public.host_str().ok_or("invalid publicBaseUrl")?;
        let host = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !host_matches(host, expected_host) {
            // Not the public host: this is the LAN candidate.
            return Ok(TransportMode::Lan);
        }
        if !trusted_proxy(remote.ip(), &settings.trusted_proxies) {
            return Err("public request did not come from a trusted proxy".into());
        }
        let forwarded = headers
            .get("x-forwarded-proto")
            .or_else(|| headers.get("forwarded"))
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !forwarded.to_ascii_lowercase().contains("https") {
            return Err("public access requires HTTPS terminated by a trusted proxy".into());
        }
        Ok(TransportMode::Public)
    }

    async fn verify_device(
        &self,
        auth: &WireFrame,
        remote_static: &[u8],
    ) -> Result<(DeviceRecord, RemotePrincipal), RemoteError> {
        let public_key = URL_SAFE_NO_PAD.encode(remote_static);
        let key_id = node_key_id(&public_key);
        let settings = self.settings.read().await.clone();
        let (ticket, device_id, device_info) = match auth {
            WireFrame::Auth {
                ticket,
                device_id,
                device,
                ..
            } => (ticket.clone(), device_id.clone(), device.clone()),
            _ => (None, None, None),
        };
        if let Some(device_id) = device_id {
            let mut devices = self.devices.write().await;
            let Some(record) = devices.iter_mut().find(|d| d.device_id == device_id) else {
                return Err(RemoteError::DeviceNotFound);
            };
            if record.revoked {
                return Err(RemoteError::DeviceNotFound);
            }
            if record.public_key != public_key {
                return Err(RemoteError::DeviceNotFound);
            }
            record.last_seen_at_ms = now_ms();
            let record = record.clone();
            drop(devices);
            let principal = RemotePrincipal {
                device_id: record.device_id.clone(),
                scopes: record.scopes.clone(),
                profile: self.product.profile(),
                transport: settings.transport(),
            };
            return Ok((record, principal));
        }
        if let Some(raw) = ticket {
            let mut ticket_state = self.ticket.write().await;
            let Some(current) = ticket_state.as_ref() else {
                return Err(RemoteError::InvalidTicket);
            };
            if current.record.expires_at_ms < now_ms() || current.record.hash != hash_secret(&raw) {
                return Err(RemoteError::InvalidTicket);
            }
            *ticket_state = None;
            drop(ticket_state);
            let info = device_info.unwrap_or(AuthDevice {
                name: "PersonAI Mobile".into(),
                platform: "unknown".into(),
                app_version: String::new(),
            });
            let record = DeviceRecord {
                device_id: generate_device_id(),
                name: info.name,
                platform: info.platform,
                public_key: public_key.clone(),
                key_id,
                created_at_ms: now_ms(),
                last_seen_at_ms: now_ms(),
                scopes: vec!["session.read".into(), "session.write".into()],
                revoked: false,
            };
            let mut devices = self.devices.write().await;
            devices.push(record.clone());
            let snapshot = devices.clone();
            drop(devices);
            self.store
                .save_devices(&snapshot)
                .await
                .map_err(RemoteError::Store)?;
            let principal = RemotePrincipal {
                device_id: record.device_id.clone(),
                scopes: record.scopes.clone(),
                profile: self.product.profile(),
                transport: settings.transport(),
            };
            return Ok((record, principal));
        }
        // No ticket and no device id: only an already paired key is accepted.
        let mut devices = self.devices.write().await;
        let Some(record) = devices
            .iter_mut()
            .find(|d| d.public_key == public_key && !d.revoked)
        else {
            return Err(RemoteError::DeviceNotFound);
        };
        record.last_seen_at_ms = now_ms();
        let record = record.clone();
        drop(devices);
        let principal = RemotePrincipal {
            device_id: record.device_id.clone(),
            scopes: record.scopes.clone(),
            profile: self.product.profile(),
            transport: settings.transport(),
        };
        Ok((record, principal))
    }
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "service": "remote" }))
}

async fn identity(
    State(service): State<Arc<RemoteAccessService>>,
    headers: HeaderMap,
    ConnectInfo(remote): ConnectInfo<std::net::SocketAddr>,
) -> Response {
    if let Err(error) = service.transport_for(&headers, remote).await {
        return transport_error(StatusCode::BAD_REQUEST, error);
    }
    let mut value = serde_json::to_value(service.identity().await).unwrap_or(Value::Null);
    // The anonymous LAN identity must not expose the current profile.
    if let Some(object) = value.as_object_mut() {
        object.remove("profile");
    }
    Json(value).into_response()
}

async fn secure_socket(
    ws: WebSocketUpgrade,
    State(service): State<Arc<RemoteAccessService>>,
    headers: HeaderMap,
    ConnectInfo(remote): ConnectInfo<std::net::SocketAddr>,
) -> Response {
    let transport = match service.transport_for(&headers, remote).await {
        Ok(mode) => mode,
        Err(error) => return transport_error(StatusCode::BAD_REQUEST, error),
    };
    let ws = ws.protocols(["tron-remote"]);
    ws.on_upgrade(move |socket| async move {
        if let Err(error) = handle_socket(socket, service, transport).await {
            warn!("remote secure socket ended: {error}");
        }
    })
}

async fn handle_socket(
    socket: WebSocket,
    service: Arc<RemoteAccessService>,
    transport_mode: TransportMode,
) -> Result<(), RemoteError> {
    if !service.settings.read().await.enabled {
        return Err(RemoteError::Disabled);
    }
    let (mut sender, mut receiver) = socket.split();
    let node = service.node.read().await.clone();
    let mut product = service.product.product();
    // The remote node identity is generated/persisted by this service and is
    // the one pinned by the pairing QR. The product's configured node id is
    // only a fallback; advertising a different id would make the mobile device
    // key lookup miss after pairing.
    product.node_id = node.node_id.clone();
    let prologue = noise_prologue(&node.node_id, &product.product_code, transport_mode);
    let private = node.private_key_bytes().map_err(|e| RemoteError::Key(e.to_string()))?;
    let mut responder = snow::Builder::new(
        NOISE_SUITE
            .parse()
            .map_err(|e| RemoteError::Noise(format!("{e:?}")))?,
    )
    .local_private_key(&private)
    .prologue(&prologue)
    .build_responder()
    .map_err(|e| RemoteError::Noise(format!("{e:?}")))?;
    let first = receive_binary(&mut receiver).await?;
    let mut buf = vec![0u8; 65535];
    responder
        .read_message(&first, &mut buf)
        .map_err(|e| RemoteError::Noise(format!("{e:?}")))?;
    let second_len = responder
        .write_message(&[], &mut buf)
        .map_err(|e| RemoteError::Noise(format!("{e:?}")))?;
    sender
        .send(Message::Binary(buf[..second_len].to_vec().into()))
        .await
        .map_err(|e| RemoteError::Frame(e.to_string()))?;
    let remote_static = responder
        .get_remote_static()
        .map(|value| value.to_vec())
        .ok_or_else(|| RemoteError::Noise("missing remote static key".into()))?;
    let transport = Arc::new(Mutex::new(
        responder
            .into_transport_mode()
            .map_err(|e| RemoteError::Noise(format!("{e:?}")))?,
    ));

    let auth_plain = match receive_noise(&mut receiver, transport.clone()).await {
        Ok(value) => value,
        Err(error) => {
            warn!("remote secure auth read failed: {error}");
            return Err(error);
        }
    };
    let auth: WireFrame = match serde_json::from_slice(&auth_plain) {
        Ok(value) => value,
        Err(error) => {
            warn!("remote secure auth frame invalid: {error}");
            return Err(RemoteError::Frame(format!("auth frame: {error}")));
        }
    };
    let (_device, principal) = match service.verify_device(&auth, &remote_static).await {
        Ok(value) => value,
        Err(error) => {
            warn!("remote secure device verify failed: {error}");
            return Err(error);
        }
    };

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<WireFrame>();
    let ready = WireFrame::Ready(eliteforge_agent_remote_protocol::ReadyFrame {
        device_id: principal.device_id.clone(),
        session_id: format!("sess-{}", node_key_id(&URL_SAFE_NO_PAD.encode(remote_static)).trim_start_matches("sha256:")),
        expires_in: 43_200,
        methods: service.methods(),
        product,
        events: "agui".into(),
        features: vec!["binary".into(), "fragment".into()],
    });
    let ready_plain = serde_json::to_vec(&ready).map_err(|e| RemoteError::Frame(e.to_string()))?;
    let ready_len = transport
        .lock()
        .await
        .write_message(&ready_plain, &mut buf)
        .map_err(|e| RemoteError::Noise(format!("{e:?}")))?;
    sender
        .send(Message::Binary(buf[..ready_len].to_vec().into()))
        .await
        .map_err(|e| RemoteError::Frame(e.to_string()))?;

    let writer_transport = transport.clone();
    let writer = tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        'writer: while let Some(frame) = out_rx.recv().await {
            let plain = match serde_json::to_vec(&frame) {
                Ok(value) => value,
                Err(_) => continue,
            };
            for plain in outgoing_frames(&frame, plain) {
                let cipher_len = {
                    let mut transport = writer_transport.lock().await;
                    match transport.write_message(&plain, &mut buf) {
                        Ok(value) => value,
                        Err(_) => break 'writer,
                    }
                };
                if sender
                    .send(Message::Binary(buf[..cipher_len].to_vec().into()))
                    .await
                    .is_err()
                {
                    break 'writer;
                }
            }
        }
    });

    let cancels: Arc<Mutex<HashMap<u64, CancellationToken>>> = Arc::new(Mutex::new(HashMap::new()));
    loop {
        let plain = match receive_noise(&mut receiver, transport.clone()).await {
            Ok(value) => value,
            Err(_) => break,
        };
        let frame: WireFrame = match serde_json::from_slice(&plain) {
            Ok(value) => value,
            Err(error) => {
                let _ = out_tx.send(WireFrame::RpcError {
                    id: 0,
                    code: "REMOTE_INVALID_FRAME".into(),
                    message: error.to_string(),
                    details: None,
                });
                continue;
            }
        };
        match frame {
            WireFrame::Rpc { id, method, params } => {
                let cancel = CancellationToken::new();
                cancels.lock().await.insert(id, cancel.clone());
                let service = service.clone();
                let principal = principal.clone();
                let out = out_tx.clone();
                let cancels = cancels.clone();
                tokio::spawn(async move {
                    let result = dispatch_rpc(&service, &principal, id, &method, params, out.clone(), cancel).await;
                    match result {
                        Ok(()) => {}
                        Err(error) => {
                            let _ = out.send(WireFrame::RpcError {
                                id,
                                code: error.code,
                                message: error.message,
                                details: error.details,
                            });
                        }
                    }
                    cancels.lock().await.remove(&id);
                });
            }
            WireFrame::Cancel { id } => {
                if let Some(token) = cancels.lock().await.remove(&id) {
                    token.cancel();
                }
            }
            WireFrame::Ping { ts } => {
                let _ = out_tx.send(WireFrame::Pong { ts });
            }
            _ => {}
        }
    }
    writer.abort();
    Ok(())
}

const MAX_REMOTE_FRAME_BYTES: usize = 48 * 1024;
const MAX_REMOTE_RESULT_BYTES: usize = 4 * 1024 * 1024;

fn outgoing_frames(frame: &WireFrame, plain: Vec<u8>) -> Vec<Vec<u8>> {
    if plain.len() <= MAX_REMOTE_FRAME_BYTES {
        return vec![plain];
    }
    let id = match frame {
        WireFrame::RpcResult { id, .. }
        | WireFrame::RpcEvent { id, .. }
        | WireFrame::RpcError { id, .. } => *id,
        _ => return Vec::new(),
    };
    if plain.len() > MAX_REMOTE_RESULT_BYTES {
        return vec![json!({
            "type": "rpc_error",
            "id": id,
            "code": "REMOTE_RESULT_TOO_LARGE",
            "message": "Remote response exceeds the maximum result size"
        }).to_string().into_bytes()];
    }

    let body = String::from_utf8(plain).expect("serde_json emits UTF-8");
    // Leave room for JSON escaping and fragment envelope fields.
    let chunk_bytes = (MAX_REMOTE_FRAME_BYTES - 256) / 6;
    let mut remaining = body.as_str();
    let mut chunks = Vec::new();
    while !remaining.is_empty() {
        let mut end = remaining.len().min(chunk_bytes);
        while !remaining.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push(&remaining[..end]);
        remaining = &remaining[end..];
    }
    let total = chunks.len();
    chunks.into_iter().enumerate().map(|(seq, data)| {
        json!({"type": "fragment", "id": id, "seq": seq, "total": total, "data": data})
            .to_string().into_bytes()
    }).collect()
}

/// Upper bound for draining queued events after a streaming call returns.
/// It is a safety valve, not a delay: the forwarder ends as soon as the
/// product drops its `EventEmitter`.
const EVENT_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Methods that emit a stream of AG-UI events in addition to their result.
fn is_streaming_rpc_method(method: &str) -> bool {
    matches!(
        method,
        eliteforge_agent_remote_rpc::SESSION_SEND_MESSAGE
            | eliteforge_agent_remote_rpc::SESSION_WATCH_EVENTS
    )
}

fn session_rpc(service: &Arc<RemoteAccessService>) -> Result<Arc<dyn SessionRpc>, RpcError> {
    service
        .registry
        .session_rpc()
        .ok_or_else(|| RpcError::unsupported("session RPC is not implemented by this product"))
}

/// Streaming calls parse their params before the event channel exists, so a
/// malformed request never leaves a sender open.
enum StreamingCall {
    SendMessage(SessionSendMessageParams),
    WatchEvents(SessionWatchEventsParams),
}

fn parse_streaming_call(method: &str, params: Value) -> Result<StreamingCall, RpcError> {
    match method {
        eliteforge_agent_remote_rpc::SESSION_SEND_MESSAGE => serde_json::from_value(params)
            .map(StreamingCall::SendMessage)
            .map_err(|e| RpcError::invalid_params(e.to_string())),
        eliteforge_agent_remote_rpc::SESSION_WATCH_EVENTS => serde_json::from_value(params)
            .map(StreamingCall::WatchEvents)
            .map_err(|e| RpcError::invalid_params(e.to_string())),
        other => Err(RpcError::unsupported(format!(
            "{other} is not a streaming method"
        ))),
    }
}

fn json_value<T: serde::Serialize>(value: T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|e| RpcError::internal(e.to_string()))
}

fn send_rpc_result(out: &mpsc::UnboundedSender<WireFrame>, id: u64, result: Value) {
    let _ = out.send(WireFrame::RpcResult { id, result });
    let _ = out.send(WireFrame::RpcEnd { id });
}

/// Dispatches one RPC on the path its method needs. Streaming and unary calls
/// used to share one event channel; the unary path then waited on a sender
/// that only closed when the request ended, adding a fixed 5s to every plain
/// call (workspaces, session groups, dudes, products, ...).
async fn dispatch_rpc(
    service: &Arc<RemoteAccessService>,
    principal: &RemotePrincipal,
    id: u64,
    method: &str,
    params: Value,
    out: mpsc::UnboundedSender<WireFrame>,
    cancel: CancellationToken,
) -> Result<(), RpcError> {
    if is_streaming_rpc_method(method) {
        dispatch_streaming_rpc(service, principal, id, method, params, out, cancel).await
    } else {
        dispatch_unary_rpc(service, principal, id, method, params, out).await
    }
}

/// One request, one result. These methods cannot emit AG-UI events, so there
/// is no channel to drain and no reason to wait.
async fn dispatch_unary_rpc(
    service: &Arc<RemoteAccessService>,
    principal: &RemotePrincipal,
    id: u64,
    method: &str,
    params: Value,
    out: mpsc::UnboundedSender<WireFrame>,
) -> Result<(), RpcError> {
    let session = session_rpc(service)?;
    let result = match method {
        eliteforge_agent_remote_rpc::SESSION_LIST => {
            let params = serde_json::from_value::<SessionListParams>(params).map_err(|e| RpcError::invalid_params(e.to_string()))?;
            serde_json::to_value(session.list(principal, params).await?).map_err(|e| RpcError::internal(e.to_string()))?
        }
        eliteforge_agent_remote_rpc::SESSION_HISTORY => {
            let params = serde_json::from_value::<SessionHistoryParams>(params).map_err(|e| RpcError::invalid_params(e.to_string()))?;
            serde_json::to_value(session.history(principal, params).await?).map_err(|e| RpcError::internal(e.to_string()))?
        }
        eliteforge_agent_remote_rpc::SESSION_CREATE => {
            let params = serde_json::from_value::<SessionCreateParams>(params).map_err(|e| RpcError::invalid_params(e.to_string()))?;
            serde_json::to_value(session.create(principal, params).await?).map_err(|e| RpcError::internal(e.to_string()))?
        }
        eliteforge_agent_remote_rpc::SESSION_STOP_RUN => {
            let params = serde_json::from_value::<SessionStopRunParams>(params).map_err(|e| RpcError::invalid_params(e.to_string()))?;
            serde_json::to_value(session.stop_run(principal, params).await?).map_err(|e| RpcError::internal(e.to_string()))?
        }
        eliteforge_agent_remote_rpc::SESSION_UNWATCH_EVENTS => {
            let params = serde_json::from_value::<SessionUnwatchEventsParams>(params).map_err(|e| RpcError::invalid_params(e.to_string()))?;
            session.unwatch_events(principal, params).await?;
            Value::Null
        }
        other => service.registry.dispatch(principal, other, params).await?,
    };
    send_rpc_result(&out, id, result);
    Ok(())
}

/// Forwards events while the product runs. When the call returns, its
/// `EventEmitter` is dropped, so awaiting the forwarder drains only the events
/// still queued; that ordering is what keeps `rpc_result`/`rpc_end` last.
async fn dispatch_streaming_rpc(
    service: &Arc<RemoteAccessService>,
    principal: &RemotePrincipal,
    id: u64,
    method: &str,
    params: Value,
    out: mpsc::UnboundedSender<WireFrame>,
    cancel: CancellationToken,
) -> Result<(), RpcError> {
    let session = session_rpc(service)?;
    let call = parse_streaming_call(method, params)?;

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Value>();
    let events = EventEmitter::new(event_tx, cancel);
    let out_for_events = out.clone();
    let mut forward = tokio::spawn(async move {
        let mut seq = 0u64;
        while let Some(data) = event_rx.recv().await {
            seq = seq.saturating_add(1);
            if out_for_events
                .send(WireFrame::RpcEvent {
                    id,
                    data,
                    seq: Some(seq),
                })
                .is_err()
            {
                break;
            }
        }
    });

    let outcome = match call {
        StreamingCall::SendMessage(params) => session
            .send_message(principal, params, events)
            .await
            .and_then(json_value),
        StreamingCall::WatchEvents(params) => session
            .watch_events(principal, params, events)
            .await
            .and_then(json_value),
    };
    // Never let a failure overtake events the product already emitted.
    if tokio::time::timeout(EVENT_DRAIN_TIMEOUT, &mut forward)
        .await
        .is_err()
    {
        forward.abort();
    }
    send_rpc_result(&out, id, outcome?);
    Ok(())
}

async fn receive_binary(
    receiver: &mut futures_util::stream::SplitStream<WebSocket>,
) -> Result<Vec<u8>, RemoteError> {
    loop {
        match receiver.next().await {
            Some(Ok(Message::Binary(bytes))) => {
                if bytes.len() > 64 * 1024 {
                    return Err(RemoteError::Frame("handshake frame too large".into()));
                }
                return Ok(bytes.to_vec());
            }
            Some(Ok(Message::Close(_))) | None => {
                return Err(RemoteError::Frame("socket closed during handshake".into()));
            }
            Some(Err(error)) => return Err(RemoteError::Frame(error.to_string())),
            _ => continue,
        }
    }
}

async fn receive_noise(
    receiver: &mut futures_util::stream::SplitStream<WebSocket>,
    transport: Arc<Mutex<snow::TransportState>>,
) -> Result<Vec<u8>, RemoteError> {
    loop {
        let bytes = receive_binary(receiver).await?;
        let mut plain = vec![0u8; 65535];
        let len = {
            let mut transport = transport.lock().await;
            transport
                .read_message(&bytes, &mut plain)
                .map_err(|e| RemoteError::Noise(format!("{e:?}")))?
        };
        plain.truncate(len);
        return Ok(plain);
    }
}

fn generate_server_node_identity() -> Result<NodeIdentity, RemoteError> {
    let keypair = snow::Builder::new(
        NOISE_SUITE
            .parse()
            .map_err(|e| RemoteError::Noise(format!("{e:?}")))?,
    )
    .generate_keypair()
    .map_err(|e| RemoteError::Noise(format!("{e:?}")))?;
    let public_key = URL_SAFE_NO_PAD.encode(keypair.public);
    let private_key = URL_SAFE_NO_PAD.encode(keypair.private);
    Ok(NodeIdentity {
        node_id: generate_node_id(),
        key_id: node_key_id(&public_key),
        public_key,
        private_key,
        created_at_ms: now_ms(),
    })
}


fn transport_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

fn host_matches(actual: &str, expected: &str) -> bool {
    let actual = actual.trim().to_ascii_lowercase();
    let expected = expected.trim().to_ascii_lowercase();
    if actual == expected {
        return true;
    }
    // Strip default or explicit ports for comparison.
    let actual_host = actual.split(':').next().unwrap_or("");
    let expected_host = expected.split(':').next().unwrap_or("");
    actual_host == expected_host
}

fn trusted_proxy(ip: std::net::IpAddr, cidrs: &[String]) -> bool {
    if ip.is_loopback() {
        return true;
    }
    cidrs.iter().any(|entry| {
        entry
            .parse::<ipnet::IpNet>()
            .map(|net| net.contains(&ip))
            .unwrap_or_else(|_| entry.trim() == ip.to_string())
    })
}
