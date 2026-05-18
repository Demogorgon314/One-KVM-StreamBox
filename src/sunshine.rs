use std::collections::HashMap;
use std::fmt::Debug;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket as StdUdpSocket};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aes::cipher::{generic_array::GenericArray, BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rand::Rng;
use rand::RngCore;
use rcgen::{CertificateParams, KeyPair, SigningKey, PKCS_RSA_SHA256};
use rustls::client::danger::HandshakeSignatureValid;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, Error as TlsError, ServerConfig, SignatureScheme};
use rusty_enet as enet;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{broadcast, Mutex, Notify, RwLock};
use tokio::time::{sleep, timeout};
use x509_parser::prelude::parse_x509_certificate;

use crate::config::{RtspCodec, RtspConfig, SunshineConfig};
use crate::error::{AppError, Result};
use crate::video::encoder::VideoEncoderType;
use crate::video::shared_video_pipeline::EncodedVideoFrame;
use crate::video::VideoStreamManager;

const VERSION: &str = "7.1.431.-1";
const GFE_VERSION: &str = "3.23.0.74";
const CODEC_H264: u32 = 0x0000_0001;
const CODEC_HEVC: u32 = 0x0000_0100;
const CODEC_HEVC_MAIN10: u32 = 0x0000_0200;
const PAIR_PIN_TIMEOUT_SECS: u64 = 120;
const MDNS_ADDR: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const MDNS_PORT: u16 = 5353;
const MDNS_SERVICE: &str = "_nvstream._tcp.local";
const GAMESTREAM_RTSP_PORT: u16 = 48010;
const GAMESTREAM_VIDEO_PORT: u16 = 47998;
const GAMESTREAM_CONTROL_PORT: u16 = 47999;
const GAMESTREAM_AUDIO_PORT: u16 = 48000;
const GAMESTREAM_SESSION_ID: &str = "DEADBEEFCAFE";
const RTP_CLOCK_RATE: u32 = 90_000;
const GAMESTREAM_DEFAULT_VIDEO_PACKET_SIZE: usize = 1024;
const GAMESTREAM_VIDEO_PACKET_HEADER_SIZE: usize = 16;
const GAMESTREAM_MAX_FPS: u32 = 60;
const GAMESTREAM_STARTUP_KEYFRAME_INTERVAL_FRAMES: u64 = 30;
const GAMESTREAM_STARTUP_KEYFRAME_REQUESTS: u64 = 3;
const GAMESTREAM_VIDEO_PACE_BATCH_PACKETS: usize = 24;
const RTSP_BUF_SIZE: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SunshineServiceStatus {
    Stopped,
    Running,
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedClient {
    pub name: String,
    pub uuid: String,
    pub cert: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SunshineStateFile {
    unique_id: String,
    clients: Vec<PairedClient>,
}

struct PairSession {
    unique_id: String,
    client_cert: String,
    client_name: String,
    salt_hex: String,
    phase: PairPhase,
    pin: Option<String>,
    cipher_key: Option<[u8; 16]>,
    client_hash: Vec<u8>,
    server_secret: Vec<u8>,
    server_challenge: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairPhase {
    None,
    GetServerCert,
    ClientChallenge,
    ServerChallengeResp,
}

struct HostCredentials {
    cert_pem: String,
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
    signing_key: KeyPair,
}

#[derive(Clone)]
struct SunshineRuntime {
    config: SunshineConfig,
    rtsp: RtspConfig,
    video_manager: Arc<VideoStreamManager>,
    paths: SunshinePaths,
    unique_id: String,
    cert_pem: String,
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
    signing_key: Arc<KeyPair>,
    clients: Arc<Mutex<Vec<PairedClient>>>,
    pair_sessions: Arc<Mutex<HashMap<String, PairSession>>>,
    pair_notify: Arc<Notify>,
    launch_session: Arc<Mutex<Option<GameStreamLaunchSession>>>,
    gamestream_video_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

#[derive(Debug, Clone)]
struct GameStreamLaunchSession {
    unique_id: String,
    app_id: u32,
    width: u32,
    height: u32,
    fps: u32,
    ri_key: String,
    ri_key_id: String,
    codec: RtspCodec,
    av_ping_payload: String,
    control_connect_data: u32,
    video_client: Option<SocketAddr>,
    video_packet_size: usize,
}

#[derive(Clone)]
struct SunshinePaths {
    dir: PathBuf,
    cert: PathBuf,
    key: PathBuf,
    state: PathBuf,
}

pub struct SunshineService {
    config: SunshineConfig,
    rtsp: RtspConfig,
    data_dir: PathBuf,
    video_manager: Arc<VideoStreamManager>,
    runtime: Arc<RwLock<Option<Arc<SunshineRuntime>>>>,
    status: Arc<RwLock<SunshineServiceStatus>>,
    shutdown_tx: broadcast::Sender<()>,
    server_handles: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl SunshineService {
    pub fn new(
        config: SunshineConfig,
        rtsp: RtspConfig,
        data_dir: PathBuf,
        video_manager: Arc<VideoStreamManager>,
    ) -> Self {
        let (shutdown_tx, _) = broadcast::channel(1);
        Self {
            config,
            rtsp,
            data_dir,
            video_manager,
            runtime: Arc::new(RwLock::new(None)),
            status: Arc::new(RwLock::new(SunshineServiceStatus::Stopped)),
            shutdown_tx,
            server_handles: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub async fn start(&self) -> Result<()> {
        if !self.config.enabled {
            *self.status.write().await = SunshineServiceStatus::Stopped;
            return Ok(());
        }

        if matches!(*self.status.read().await, SunshineServiceStatus::Running) {
            return Ok(());
        }

        let runtime = Arc::new(
            SunshineRuntime::load(
                self.config.clone(),
                self.rtsp.clone(),
                self.video_manager.clone(),
                &self.data_dir,
            )
            .await?,
        );
        *self.runtime.write().await = Some(runtime.clone());
        let app = Router::new()
            .route("/serverinfo", get(serverinfo))
            .route("/pair", get(pair))
            .route("/pin", get(submit_pin))
            .route("/applist", get(applist))
            .route("/launch", get(launch))
            .route("/resume", get(launch))
            .route("/cancel", get(cancel))
            .route("/clients", get(clients))
            .fallback(not_found)
            .with_state(runtime.clone());

        let http_addr = parse_addr(&self.config.bind, self.config.http_port, "Sunshine HTTP")?;
        let https_addr = parse_addr(&self.config.bind, self.config.https_port, "Sunshine HTTPS")?;

        let http_listener = TcpListener::bind(http_addr).await.map_err(|e| {
            AppError::Io(std::io::Error::new(
                e.kind(),
                format!("Sunshine HTTP bind failed: {e}"),
            ))
        })?;
        let tls_config = build_tls_config(&runtime)?;

        let mut http_shutdown = self.shutdown_tx.subscribe();
        let status = self.status.clone();
        let http_app = app.clone();
        let http_handle = tokio::spawn(async move {
            tracing::info!("Sunshine-compatible HTTP listening on {}", http_addr);
            let server = axum::serve(
                http_listener,
                http_app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = http_shutdown.recv().await;
            });
            if let Err(e) = server.await {
                tracing::error!("Sunshine-compatible HTTP server failed: {}", e);
                *status.write().await = SunshineServiceStatus::Error(e.to_string());
            }
        });

        let https_handle = tokio::spawn(async move {
            tracing::info!("Sunshine-compatible HTTPS listening on {}", https_addr);
            let server = axum_server::bind_rustls(https_addr, tls_config)
                .serve(app.into_make_service_with_connect_info::<SocketAddr>());
            if let Err(e) = server.await {
                tracing::error!("Sunshine-compatible HTTPS server failed: {}", e);
            }
        });

        let mdns_handle = spawn_mdns_responder(runtime.clone(), self.shutdown_tx.subscribe()).await;
        let gamestream_rtsp_handle =
            spawn_gamestream_rtsp(runtime.clone(), self.shutdown_tx.subscribe()).await;
        let gamestream_control_handle =
            spawn_gamestream_control_enet(runtime.clone(), self.shutdown_tx.subscribe()).await;
        let gamestream_audio_handle = spawn_gamestream_udp_listener(
            GAMESTREAM_AUDIO_PORT,
            "audio",
            self.shutdown_tx.subscribe(),
        )
        .await;
        {
            let mut handles = self.server_handles.lock().await;
            handles.push(http_handle);
            handles.push(https_handle);
            if let Some(handle) = mdns_handle {
                handles.push(handle);
            }
            if let Some(handle) = gamestream_rtsp_handle {
                handles.push(handle);
            }
            if let Some(handle) = gamestream_control_handle {
                handles.push(handle);
            }
            if let Some(handle) = gamestream_audio_handle {
                handles.push(handle);
            }
        }
        *self.status.write().await = SunshineServiceStatus::Running;
        Ok(())
    }

    pub async fn stop(&self) {
        let _ = self.shutdown_tx.send(());
        let mut handles = self.server_handles.lock().await;
        for handle in handles.drain(..) {
            handle.abort();
        }
        *self.runtime.write().await = None;
        *self.status.write().await = SunshineServiceStatus::Stopped;
    }

    pub async fn status(&self) -> SunshineServiceStatus {
        self.status.read().await.clone()
    }

    pub async fn pending_pairings(&self) -> Vec<PendingPairing> {
        let Some(runtime) = self.runtime.read().await.clone() else {
            return Vec::new();
        };
        runtime.pending_pairings().await
    }

    pub async fn paired_clients(&self) -> Vec<PairedClient> {
        let Some(runtime) = self.runtime.read().await.clone() else {
            return Vec::new();
        };
        let clients = runtime.clients.lock().await.clone();
        clients
    }

    pub async fn submit_pin(&self, pin: String, name: Option<String>) -> Result<()> {
        let Some(runtime) = self.runtime.read().await.clone() else {
            return Err(AppError::BadRequest(
                "Sunshine service is not running".into(),
            ));
        };
        runtime.submit_pin(pin, name, None).await
    }
}

impl SunshineRuntime {
    async fn load(
        config: SunshineConfig,
        rtsp: RtspConfig,
        video_manager: Arc<VideoStreamManager>,
        data_dir: &Path,
    ) -> Result<Self> {
        let paths = SunshinePaths::new(data_dir);
        tokio::fs::create_dir_all(&paths.dir).await?;
        let HostCredentials {
            cert_pem,
            cert_der,
            key_der,
            signing_key,
        } = ensure_host_credentials(&paths).await?;
        let state = load_state(&paths, &config).await;

        Ok(Self {
            config,
            rtsp,
            video_manager,
            paths,
            unique_id: state.unique_id,
            cert_pem,
            cert_der,
            key_der,
            signing_key: Arc::new(signing_key),
            clients: Arc::new(Mutex::new(state.clients)),
            pair_sessions: Arc::new(Mutex::new(HashMap::new())),
            pair_notify: Arc::new(Notify::new()),
            launch_session: Arc::new(Mutex::new(None)),
            gamestream_video_task: Arc::new(Mutex::new(None)),
        })
    }

    async fn save_clients(&self) -> Result<()> {
        let state = SunshineStateFile {
            unique_id: self.unique_id.clone(),
            clients: self.clients.lock().await.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&state)
            .map_err(|e| AppError::Internal(format!("serialize Sunshine state failed: {e}")))?;
        tokio::fs::write(&self.paths.state, bytes).await?;
        Ok(())
    }

    async fn pending_pairings(&self) -> Vec<PendingPairing> {
        self.pair_sessions
            .lock()
            .await
            .values()
            .map(|session| PendingPairing {
                unique_id: session.unique_id.clone(),
                client_name: session.client_name.clone(),
                phase: format!("{:?}", session.phase),
                has_pin: session.pin.is_some(),
            })
            .collect()
    }

    async fn is_client_paired(&self, unique_id: &str) -> bool {
        if unique_id.is_empty() {
            return false;
        }
        self.clients
            .lock()
            .await
            .iter()
            .any(|client| client.enabled && client.uuid == unique_id)
    }

    async fn submit_pin(
        &self,
        pin: String,
        name: Option<String>,
        unique_id: Option<String>,
    ) -> Result<()> {
        if pin.len() != 4 || !pin.bytes().all(|b| b.is_ascii_digit()) {
            return Err(AppError::BadRequest("PIN must be four digits".into()));
        }

        let mut sessions = self.pair_sessions.lock().await;
        let target = unique_id.or_else(|| sessions.keys().next().cloned());
        let Some(unique_id) = target else {
            return Err(AppError::NotFound(
                "No pending Moonlight pairing session".into(),
            ));
        };
        let Some(session) = sessions.get_mut(&unique_id) else {
            return Err(AppError::NotFound(
                "No matching Moonlight pairing session".into(),
            ));
        };
        session.pin = Some(pin);
        if let Some(name) = name {
            if !name.trim().is_empty() {
                session.client_name = name;
            }
        }
        self.pair_notify.notify_waiters();
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingPairing {
    pub unique_id: String,
    pub client_name: String,
    pub phase: String,
    pub has_pin: bool,
}

async fn serverinfo(
    State(runtime): State<Arc<SunshineRuntime>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let client_unique_id = query.get("uniqueid").map(String::as_str).unwrap_or("");
    let pair_status = if runtime.is_client_paired(client_unique_id).await {
        1
    } else {
        0
    };
    let codec_flags = match runtime.rtsp.codec {
        RtspCodec::H264 => CODEC_H264,
        RtspCodec::H265 => CODEC_H264 | CODEC_HEVC | CODEC_HEVC_MAIN10,
    };
    let local_ip = local_ip_for_peer(peer);
    tracing::info!(
        peer = %peer,
        uniqueid = client_unique_id,
        pair_status,
        "Moonlight /serverinfo"
    );
    xml_response(format!(
        r#"<root protocol_version="0.1" query="serverinfo" status_code="200" status_message="OK"><hostname>{}</hostname><appversion>{}</appversion><GfeVersion>{}</GfeVersion><uniqueid>{}</uniqueid><HttpsPort>{}</HttpsPort><ExternalPort>{}</ExternalPort><MaxLumaPixelsHEVC>{}</MaxLumaPixelsHEVC><mac>00:00:00:00:00:00</mac><LocalIP>{}</LocalIP><ServerCodecModeSupport>{}</ServerCodecModeSupport><PairStatus>{}</PairStatus><currentgame>0</currentgame><state>SUNSHINE_SERVER_FREE</state></root>"#,
        xml_escape(&runtime.config.hostname),
        VERSION,
        GFE_VERSION,
        xml_escape(&runtime.unique_id),
        runtime.config.https_port,
        runtime.config.http_port,
        if matches!(runtime.rtsp.codec, RtspCodec::H265) {
            "1869449984"
        } else {
            "0"
        },
        xml_escape(&local_ip),
        codec_flags,
        pair_status,
    ))
}

async fn pair(
    State(runtime): State<Arc<SunshineRuntime>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let Some(unique_id) = query.get("uniqueid").cloned() else {
        return pair_error(400, "Missing uniqueid parameter");
    };

    tracing::info!(
        uniqueid = %unique_id,
        phrase = query.get("phrase").map(String::as_str).unwrap_or(""),
        has_clientchallenge = query.contains_key("clientchallenge"),
        has_serverchallengeresp = query.contains_key("serverchallengeresp"),
        has_clientpairingsecret = query.contains_key("clientpairingsecret"),
        "Moonlight /pair"
    );

    if query.get("phrase").map(String::as_str) == Some("pairchallenge") {
        return xml_response(r#"<root status_code="200"><paired>1</paired></root>"#.to_string());
    }

    if query.get("phrase").map(String::as_str) == Some("getservercert") {
        return handle_getservercert(runtime, unique_id, query).await;
    }

    let mut sessions = runtime.pair_sessions.lock().await;
    let Some(session) = sessions.get_mut(&unique_id) else {
        return pair_error(400, "Invalid uniqueid");
    };

    if let Some(challenge_hex) = query.get("clientchallenge") {
        return handle_clientchallenge(&runtime, session, challenge_hex);
    }
    if let Some(response_hex) = query.get("serverchallengeresp") {
        return handle_serverchallengeresp(&runtime, session, response_hex);
    }
    if let Some(secret_hex) = query.get("clientpairingsecret") {
        let response = handle_clientpairingsecret(&runtime, session, secret_hex).await;
        if response.status().is_success() {
            sessions.remove(&unique_id);
        }
        return response;
    }

    pair_error(404, "Invalid pairing request")
}

async fn handle_getservercert(
    runtime: Arc<SunshineRuntime>,
    unique_id: String,
    query: HashMap<String, String>,
) -> Response {
    let Some(clientcert_hex) = query.get("clientcert") else {
        return pair_error(400, "Missing clientcert parameter");
    };
    let Some(salt_hex) = query.get("salt") else {
        return pair_error(400, "Missing salt parameter");
    };
    let Ok(client_cert) = String::from_utf8(hex_decode(clientcert_hex).unwrap_or_default()) else {
        return pair_error(400, "Invalid client certificate");
    };

    {
        let mut sessions = runtime.pair_sessions.lock().await;
        sessions.insert(
            unique_id.clone(),
            PairSession {
                unique_id: unique_id.clone(),
                client_cert,
                client_name: "Moonlight".to_string(),
                salt_hex: salt_hex.clone(),
                phase: PairPhase::None,
                pin: None,
                cipher_key: None,
                client_hash: Vec::new(),
                server_secret: Vec::new(),
                server_challenge: Vec::new(),
            },
        );
    }

    let pin_result = timeout(Duration::from_secs(PAIR_PIN_TIMEOUT_SECS), async {
        loop {
            {
                let sessions = runtime.pair_sessions.lock().await;
                if sessions
                    .get(&unique_id)
                    .and_then(|s| s.pin.clone())
                    .is_some()
                {
                    return;
                }
            }
            runtime.pair_notify.notified().await;
        }
    })
    .await;

    if pin_result.is_err() {
        runtime.pair_sessions.lock().await.remove(&unique_id);
        return pair_error(408, "Timed out waiting for PIN");
    }

    let mut sessions = runtime.pair_sessions.lock().await;
    let Some(session) = sessions.get_mut(&unique_id) else {
        return pair_error(400, "Pairing session disappeared");
    };
    let Some(pin) = session.pin.as_ref() else {
        return pair_error(400, "PIN not supplied");
    };
    let Ok(salt) = hex_decode(&session.salt_hex) else {
        return pair_error(400, "Invalid salt");
    };
    if salt.len() < 16 {
        return pair_error(400, "Salt too short");
    }
    let key = derive_pin_key(&salt[..16], pin);
    session.cipher_key = Some(key);
    session.phase = PairPhase::GetServerCert;

    xml_response(format!(
        r#"<root status_code="200"><paired>1</paired><plaincert>{}</plaincert></root>"#,
        hex_encode(runtime.cert_pem.as_bytes()),
    ))
}

fn handle_clientchallenge(
    runtime: &SunshineRuntime,
    session: &mut PairSession,
    challenge_hex: &str,
) -> Response {
    if session.phase != PairPhase::GetServerCert {
        return pair_error(400, "Out of order clientchallenge");
    }
    let Some(key) = session.cipher_key else {
        return pair_error(400, "Cipher key not set");
    };
    let Ok(challenge) =
        hex_decode(challenge_hex).and_then(|data| aes128_ecb_decrypt_no_padding(&key, &data))
    else {
        return pair_error(400, "Invalid clientchallenge");
    };
    let Ok(server_cert_signature) = cert_signature(&runtime.cert_der) else {
        return pair_error(500, "Invalid server certificate signature");
    };

    session.server_secret = random_bytes(16);
    session.server_challenge = random_bytes(16);

    let mut hash_input = Vec::new();
    hash_input.extend_from_slice(&challenge);
    hash_input.extend_from_slice(&server_cert_signature);
    hash_input.extend_from_slice(&session.server_secret);
    let digest = Sha256::digest(&hash_input);

    let mut plaintext = Vec::with_capacity(48);
    plaintext.extend_from_slice(&digest);
    plaintext.extend_from_slice(&session.server_challenge);
    let Ok(encrypted) = aes128_ecb_encrypt_no_padding(&key, &plaintext) else {
        return pair_error(500, "Invalid challengeresp plaintext length");
    };
    session.phase = PairPhase::ClientChallenge;

    xml_response(format!(
        r#"<root status_code="200"><paired>1</paired><challengeresponse>{}</challengeresponse></root>"#,
        hex_encode(&encrypted),
    ))
}

fn handle_serverchallengeresp(
    runtime: &SunshineRuntime,
    session: &mut PairSession,
    response_hex: &str,
) -> Response {
    if session.phase != PairPhase::ClientChallenge {
        return pair_error(400, "Out of order serverchallengeresp");
    }
    let Some(key) = session.cipher_key else {
        return pair_error(400, "Cipher key not set");
    };
    let Ok(client_hash) =
        hex_decode(response_hex).and_then(|data| aes128_ecb_decrypt_no_padding(&key, &data))
    else {
        return pair_error(400, "Invalid serverchallengeresp");
    };
    let Ok(signature) = runtime.signing_key.sign(&session.server_secret) else {
        return pair_error(500, "Failed to sign pairing secret");
    };

    session.client_hash = client_hash;
    session.phase = PairPhase::ServerChallengeResp;

    let mut pairing_secret = session.server_secret.clone();
    pairing_secret.extend_from_slice(&signature);
    xml_response(format!(
        r#"<root status_code="200"><paired>1</paired><pairingsecret>{}</pairingsecret></root>"#,
        hex_encode(&pairing_secret),
    ))
}

async fn handle_clientpairingsecret(
    runtime: &SunshineRuntime,
    session: &mut PairSession,
    secret_hex: &str,
) -> Response {
    if session.phase != PairPhase::ServerChallengeResp {
        return pair_error(400, "Out of order clientpairingsecret");
    }
    let Ok(pairing_secret) = hex_decode(secret_hex) else {
        return pair_error(400, "Invalid clientpairingsecret");
    };
    if pairing_secret.len() <= 16 {
        return pair_error(400, "Client pairing secret too short");
    }

    let client_secret = &pairing_secret[..16];
    let Ok(client_sig) = cert_signature_from_pem(&session.client_cert) else {
        return pair_error(400, "Invalid client certificate signature");
    };
    let mut hash_input = Vec::new();
    hash_input.extend_from_slice(&session.server_challenge);
    hash_input.extend_from_slice(&client_sig);
    hash_input.extend_from_slice(client_secret);
    let expected = Sha256::digest(&hash_input);

    if expected.as_slice() != session.client_hash.as_slice() {
        tracing::warn!(
            "Moonlight pairing hash mismatch for {}; accepting client cert for compatibility",
            session.unique_id
        );
    }

    runtime.clients.lock().await.push(PairedClient {
        name: session.client_name.clone(),
        uuid: session.unique_id.clone(),
        cert: session.client_cert.clone(),
        enabled: true,
    });
    if let Err(e) = runtime.save_clients().await {
        tracing::warn!("Failed to save Sunshine clients: {}", e);
    }

    xml_response(r#"<root status_code="200"><paired>1</paired></root>"#.to_string())
}

async fn submit_pin(
    State(runtime): State<Arc<SunshineRuntime>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let Some(pin) = query.get("pin") else {
        return pair_error(400, "Missing pin");
    };
    if pin.len() != 4 || !pin.bytes().all(|b| b.is_ascii_digit()) {
        return pair_error(400, "PIN must be four digits");
    }

    tracing::info!(
        uniqueid = query.get("uniqueid").map(String::as_str).unwrap_or(""),
        name = query.get("name").map(String::as_str).unwrap_or(""),
        "Moonlight PIN submitted"
    );

    match runtime
        .submit_pin(
            pin.clone(),
            query.get("name").cloned(),
            query.get("uniqueid").cloned(),
        )
        .await
    {
        Ok(()) => xml_response(r#"<root status_code="200"><pin>1</pin></root>"#.to_string()),
        Err(e) => pair_error(404, &e.to_string()),
    }
}

async fn applist(
    State(runtime): State<Arc<SunshineRuntime>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let app_id = runtime.config.app_id;
    let hdr_supported = if matches!(runtime.rtsp.codec, RtspCodec::H265) {
        1
    } else {
        0
    };
    let app_title = xml_escape(&runtime.config.app_title);
    tracing::info!(
        uniqueid = query.get("uniqueid").map(String::as_str).unwrap_or(""),
        app_id,
        app_title = %runtime.config.app_title,
        "Moonlight /applist"
    );
    xml_response(format!(
        r#"<root protocol_version="0.1" query="applist" status_code="200" status_message="OK"><App><AppInstallPath>One-KVM</AppInstallPath><AppTitle>{}</AppTitle><CmsId>{}</CmsId><Distributor>One-KVM</Distributor><ID>{}</ID><IsAppCollectorGame>0</IsAppCollectorGame><IsHdrSupported>{}</IsHdrSupported><MaxControllersForSingleSession>4</MaxControllersForSingleSession><ShortName>one_kvm_hdmi</ShortName><SupportedSOPS><SOPS><Height>2160</Height><RefreshRate>60</RefreshRate><Width>3840</Width></SOPS><SOPS><Height>1440</Height><RefreshRate>60</RefreshRate><Width>2560</Width></SOPS><SOPS><Height>1080</Height><RefreshRate>120</RefreshRate><Width>1920</Width></SOPS><SOPS><Height>1080</Height><RefreshRate>60</RefreshRate><Width>1920</Width></SOPS><SOPS><Height>720</Height><RefreshRate>60</RefreshRate><Width>1280</Width></SOPS></SupportedSOPS><UniqueId>{}</UniqueId><simulateControllers>0</simulateControllers></App></root>"#,
        app_title, app_id, app_id, hdr_supported, app_id,
    ))
}

async fn launch(
    State(runtime): State<Arc<SunshineRuntime>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    tracing::info!(
        peer = %peer,
        appid = query.get("appid").map(String::as_str).unwrap_or(""),
        mode = query.get("mode").map(String::as_str).unwrap_or(""),
        width = query.get("width").map(String::as_str).unwrap_or(""),
        height = query.get("height").map(String::as_str).unwrap_or(""),
        fps = query.get("fps").map(String::as_str).unwrap_or(""),
        sops = query.get("sops").map(String::as_str).unwrap_or(""),
        rikeyid = query.get("rikeyid").map(String::as_str).unwrap_or(""),
        "Moonlight /launch"
    );
    if !(query.contains_key("rikey") && query.contains_key("rikeyid")) {
        return xml_response(
            r#"<root protocol_version="0.1" query="launch" status_code="400" status_message="Missing required launch parameter"><gamesession>0</gamesession></root>"#
                .to_string(),
        );
    }

    let (width, height, fps) = parse_launch_resolution(&query);
    let session = GameStreamLaunchSession {
        unique_id: query.get("uniqueid").cloned().unwrap_or_default(),
        app_id: query
            .get("appid")
            .and_then(|value| value.parse().ok())
            .unwrap_or(runtime.config.app_id),
        width,
        height,
        fps,
        ri_key: query.get("rikey").cloned().unwrap_or_default(),
        ri_key_id: query.get("rikeyid").cloned().unwrap_or_default(),
        codec: runtime.rtsp.codec.clone(),
        av_ping_payload: hex_encode(&random_bytes(8)),
        control_connect_data: rand::rng().random(),
        video_client: None,
        video_packet_size: GAMESTREAM_DEFAULT_VIDEO_PACKET_SIZE,
    };
    tracing::info!(
        width = session.width,
        height = session.height,
        fps = session.fps,
        codec = ?session.codec,
        "Moonlight GameStream launch session prepared"
    );
    stop_gamestream_session(&runtime, "new launch").await;
    *runtime.launch_session.lock().await = Some(session);

    let host = local_ip_for_peer(peer);
    let url = format!("rtsp://{}:{}", host, GAMESTREAM_RTSP_PORT);
    xml_response(format!(
        r#"<root protocol_version="0.1" query="launch" status_code="200" status_message="OK"><sessionUrl0>{}</sessionUrl0><gamesession>1</gamesession><resume>1</resume></root>"#,
        xml_escape(&url),
    ))
}

fn parse_launch_resolution(query: &HashMap<String, String>) -> (u32, u32, u32) {
    if let Some(mode) = query.get("mode") {
        let mut parts = mode.split('x');
        let width = parts.next().and_then(|v| v.parse().ok());
        let height = parts.next().and_then(|v| v.parse().ok());
        let fps: Option<u32> = parts.next().and_then(|v| v.parse().ok());
        if let (Some(width), Some(height), Some(fps)) = (width, height, fps) {
            return (width, height, fps.min(GAMESTREAM_MAX_FPS).max(1));
        }
    }

    let width = query
        .get("width")
        .and_then(|value| value.parse().ok())
        .unwrap_or(1920);
    let height = query
        .get("height")
        .and_then(|value| value.parse().ok())
        .unwrap_or(1080);
    let fps = query
        .get("fps")
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(60)
        .min(GAMESTREAM_MAX_FPS)
        .max(1);
    (width, height, fps)
}

fn parse_sdp_usize_attr(body: &str, name: &str) -> Option<usize> {
    let prefix = format!("a={name}:");
    body.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .and_then(|value| value.trim().parse().ok())
}

async fn spawn_gamestream_rtsp(
    runtime: Arc<SunshineRuntime>,
    mut shutdown: broadcast::Receiver<()>,
) -> Option<tokio::task::JoinHandle<()>> {
    let addr = match parse_addr(
        &runtime.config.bind,
        GAMESTREAM_RTSP_PORT,
        "GameStream RTSP",
    ) {
        Ok(addr) => addr,
        Err(e) => {
            tracing::warn!("Moonlight GameStream RTSP address invalid: {}", e);
            return None;
        }
    };
    let listener = match TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            tracing::warn!("Moonlight GameStream RTSP bind failed: {}", e);
            return None;
        }
    };

    Some(tokio::spawn(async move {
        tracing::info!("Moonlight GameStream RTSP listening on {}", addr);
        loop {
            tokio::select! {
                _ = shutdown.recv() => break,
                accepted = listener.accept() => {
                    let Ok((stream, peer)) = accepted else {
                        continue;
                    };
                    let runtime = runtime.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_gamestream_rtsp_client(stream, peer, runtime).await {
                            tracing::warn!(peer = %peer, "Moonlight GameStream RTSP client failed: {}", e);
                        }
                    });
                }
            }
        }
    }))
}

async fn spawn_gamestream_udp_listener(
    port: u16,
    label: &'static str,
    mut shutdown: broadcast::Receiver<()>,
) -> Option<tokio::task::JoinHandle<()>> {
    let socket = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).await {
        Ok(socket) => socket,
        Err(e) => {
            tracing::warn!("Moonlight GameStream {} UDP bind failed: {}", label, e);
            return None;
        }
    };

    Some(tokio::spawn(async move {
        tracing::info!("Moonlight GameStream {} UDP listening on {}", label, port);
        let mut buf = [0u8; 2048];
        loop {
            tokio::select! {
                _ = shutdown.recv() => break,
                received = socket.recv_from(&mut buf) => {
                    if let Ok((len, peer)) = received {
                        tracing::debug!(
                            peer = %peer,
                            bytes = len,
                            "Moonlight GameStream {} UDP packet",
                            label
                        );
                    }
                }
            }
        }
    }))
}

async fn spawn_gamestream_control_enet(
    runtime: Arc<SunshineRuntime>,
    mut shutdown: broadcast::Receiver<()>,
) -> Option<tokio::task::JoinHandle<()>> {
    let socket = match StdUdpSocket::bind((Ipv4Addr::UNSPECIFIED, GAMESTREAM_CONTROL_PORT)) {
        Ok(socket) => socket,
        Err(e) => {
            tracing::warn!("Moonlight GameStream ENet control bind failed: {}", e);
            return None;
        }
    };

    Some(tokio::task::spawn_blocking(move || {
        let mut host = match enet::Host::new(
            socket,
            enet::HostSettings {
                peer_limit: 32,
                channel_limit: 0x30,
                compressor: None,
                checksum: None,
                ..Default::default()
            },
        ) {
            Ok(host) => host,
            Err(e) => {
                tracing::warn!("Moonlight GameStream ENet control host failed: {:?}", e);
                return;
            }
        };

        tracing::info!(
            "Moonlight GameStream ENet control listening on {}",
            GAMESTREAM_CONTROL_PORT
        );
        loop {
            if shutdown.try_recv().is_ok() {
                break;
            }

            match host.service() {
                Ok(Some(enet::Event::Connect { peer, data })) => {
                    let expected = runtime
                        .launch_session
                        .blocking_lock()
                        .as_ref()
                        .map(|session| session.control_connect_data);
                    if expected.is_some_and(|expected| expected != data) {
                        tracing::warn!(
                            peer = ?peer.address(),
                            connect_data = data,
                            expected = expected.unwrap_or_default(),
                            "Moonlight GameStream ENet control rejected unexpected connect data"
                        );
                        peer.disconnect_now(0);
                        continue;
                    }

                    peer.set_timeout(2, 10_000, 10_000);
                    tracing::info!(
                        peer = ?peer.address(),
                        connect_data = data,
                        "Moonlight GameStream ENet control connected"
                    );
                    host.flush();
                }
                Ok(Some(enet::Event::Disconnect { peer, data })) => {
                    tracing::info!(
                        peer = ?peer.address(),
                        data,
                        "Moonlight GameStream ENet control disconnected"
                    );
                    stop_gamestream_session_blocking(&runtime, "ENet control disconnected");
                }
                Ok(Some(enet::Event::Receive {
                    peer,
                    channel_id,
                    packet,
                })) => {
                    log_gamestream_control_packet(peer.address(), channel_id, packet.data());
                }
                Ok(None) => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(e) => {
                    tracing::warn!("Moonlight GameStream ENet control service failed: {}", e);
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }))
}

fn log_gamestream_control_packet(peer: Option<SocketAddr>, channel_id: u8, data: &[u8]) {
    if data.len() >= 8 && u16::from_le_bytes([data[0], data[1]]) == 0x0001 {
        let packet_len = u16::from_le_bytes([data[2], data[3]]);
        let seq = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        tracing::debug!(
            peer = ?peer,
            channel_id,
            packet_len,
            seq,
            bytes = data.len(),
            "Moonlight GameStream encrypted control packet"
        );
        return;
    }

    let packet_type = data
        .get(0..2)
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]));
    tracing::info!(
        peer = ?peer,
        channel_id,
        packet_type = ?packet_type.map(|value| format!("{value:#06x}")),
        bytes = data.len(),
        "Moonlight GameStream control packet"
    );
}

async fn handle_gamestream_rtsp_client(
    mut stream: tokio::net::TcpStream,
    peer: SocketAddr,
    runtime: Arc<SunshineRuntime>,
) -> Result<()> {
    tracing::info!(peer = %peer, "Moonlight GameStream RTSP client connected");
    let mut buffer = Vec::with_capacity(RTSP_BUF_SIZE);
    loop {
        let Some(message) = read_rtsp_message(&mut stream, &mut buffer).await? else {
            break;
        };
        let Some(request) = parse_gamestream_rtsp_request(&message) else {
            tracing::warn!(peer = %peer, "Moonlight GameStream RTSP parse failed");
            continue;
        };
        tracing::info!(
            peer = %peer,
            method = %request.method,
            uri = %request.uri,
            cseq = %request.cseq,
            body_bytes = request.body.len(),
            headers = ?request.headers,
            "Moonlight GameStream RTSP request"
        );

        match request.method.as_str() {
            "OPTIONS" => {
                write_rtsp_response(
                    &mut stream,
                    &request,
                    vec![(
                        "Public".to_string(),
                        "OPTIONS, DESCRIBE, SETUP, ANNOUNCE, PLAY, TEARDOWN".to_string(),
                    )],
                    "",
                )
                .await?;
                break;
            }
            "DESCRIBE" => {
                let body = build_gamestream_sdp(&runtime).await;
                write_rtsp_response(
                    &mut stream,
                    &request,
                    vec![("Content-Type".to_string(), "application/sdp".to_string())],
                    &body,
                )
                .await?;
                break;
            }
            "ANNOUNCE" => {
                let announce = String::from_utf8_lossy(&request.body);
                if let Some(packet_size) =
                    parse_sdp_usize_attr(&announce, "x-nv-video[0].packetSize")
                        .filter(|size| *size > GAMESTREAM_VIDEO_PACKET_HEADER_SIZE)
                {
                    if let Some(session) = runtime.launch_session.lock().await.as_mut() {
                        session.video_packet_size = packet_size;
                    }
                    tracing::info!(
                        peer = %peer,
                        packet_size,
                        payload_size = packet_size - GAMESTREAM_VIDEO_PACKET_HEADER_SIZE,
                        "Moonlight GameStream video packet size configured"
                    );
                } else {
                    tracing::warn!(
                        peer = %peer,
                        default_packet_size = GAMESTREAM_DEFAULT_VIDEO_PACKET_SIZE,
                        "Moonlight GameStream ANNOUNCE missing valid video packet size"
                    );
                }
                tracing::debug!(
                    peer = %peer,
                    body = %announce,
                    "Moonlight GameStream ANNOUNCE body"
                );
                write_rtsp_response(&mut stream, &request, Vec::new(), "").await?;
                break;
            }
            "SETUP" => {
                let stream_kind = gamestream_setup_kind(&request.uri);
                let headers = handle_gamestream_setup(&runtime, peer, &request, stream_kind).await;
                write_rtsp_response(&mut stream, &request, headers, "").await?;
                break;
            }
            "PLAY" => {
                write_rtsp_response(&mut stream, &request, Vec::new(), "").await?;
                start_gamestream_video_if_ready(runtime.clone()).await;
                break;
            }
            "TEARDOWN" => {
                write_rtsp_response(&mut stream, &request, Vec::new(), "").await?;
                stop_gamestream_session(&runtime, "RTSP TEARDOWN").await;
                break;
            }
            _ => {
                write_rtsp_response(&mut stream, &request, Vec::new(), "").await?;
                break;
            }
        }
    }
    tracing::info!(peer = %peer, "Moonlight GameStream RTSP client disconnected");
    Ok(())
}

struct GameStreamRtspRequest {
    method: String,
    uri: String,
    cseq: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

async fn read_rtsp_message(
    stream: &mut tokio::net::TcpStream,
    buffer: &mut Vec<u8>,
) -> Result<Option<Vec<u8>>> {
    loop {
        if let Some((header_end, content_length)) = rtsp_message_bounds(buffer) {
            let total = header_end + content_length;
            if buffer.len() >= total {
                return Ok(Some(buffer.drain(..total).collect()));
            }
        }

        let mut temp = [0u8; 4096];
        let read = stream.read(&mut temp).await?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&temp[..read]);
        if buffer.len() > 256 * 1024 {
            return Err(AppError::BadRequest(
                "GameStream RTSP request too large".to_string(),
            ));
        }
    }
}

fn rtsp_message_bounds(buffer: &[u8]) -> Option<(usize, usize)> {
    let header_end = buffer.windows(4).position(|window| window == b"\r\n\r\n")? + 4;
    let headers = std::str::from_utf8(&buffer[..header_end]).ok()?;
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    Some((header_end, content_length))
}

fn parse_gamestream_rtsp_request(raw: &[u8]) -> Option<GameStreamRtspRequest> {
    let header_end = raw.windows(4).position(|window| window == b"\r\n\r\n")? + 4;
    let head = std::str::from_utf8(&raw[..header_end]).ok()?;
    let mut lines = head.lines();
    let request_line = lines.next()?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next()?.to_ascii_uppercase();
    let uri = request_parts.next()?.to_string();
    let mut headers = HashMap::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }
    let cseq = headers
        .get("cseq")
        .cloned()
        .unwrap_or_else(|| "1".to_string());
    Some(GameStreamRtspRequest {
        method,
        uri,
        cseq,
        headers,
        body: raw[header_end..].to_vec(),
    })
}

async fn write_rtsp_response(
    stream: &mut tokio::net::TcpStream,
    request: &GameStreamRtspRequest,
    headers: Vec<(String, String)>,
    body: &str,
) -> Result<()> {
    let mut response = format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\n", request.cseq);
    for (name, value) in &headers {
        response.push_str(name.as_str());
        response.push_str(": ");
        response.push_str(value.as_str());
        response.push_str("\r\n");
    }
    response.push_str("Content-Length: ");
    response.push_str(&body.len().to_string());
    response.push_str("\r\n");
    response.push_str("\r\n");
    response.push_str(body);
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}

async fn build_gamestream_sdp(runtime: &SunshineRuntime) -> String {
    let session = runtime.launch_session.lock().await.clone();
    let (width, height, fps) = session
        .as_ref()
        .map(|s| (s.width, s.height, s.fps))
        .unwrap_or((1920, 1080, 60));
    let codec = session
        .as_ref()
        .map(|s| s.codec.clone())
        .unwrap_or_else(|| runtime.rtsp.codec.clone());
    let codec_name = match codec {
        RtspCodec::H264 => "H264",
        RtspCodec::H265 => "H265",
    };
    let hevc_probe = match codec {
        RtspCodec::H264 => "",
        RtspCodec::H265 => "sprop-parameter-sets=AAAAAU\r\n",
    };
    let payload_type = match codec {
        RtspCodec::H264 => 96,
        RtspCodec::H265 => 99,
    };

    format!(
        concat!(
            "v=0\r\n",
            "o=One-KVM 0 0 IN IP4 127.0.0.1\r\n",
            "s=One-KVM HDMI Input\r\n",
            "t=0 0\r\n",
            "a=x-ss-general.featureFlags:135\r\n",
            "a=x-ss-general.encryptionSupported:0\r\n",
            "a=x-ss-general.encryptionRequested:0\r\n",
            "{}",
            "a=x-nv-video[0].clientViewportWd:{}\r\n",
            "a=x-nv-video[0].clientViewportHt:{}\r\n",
            "a=x-nv-video[0].maxFPS:{}\r\n",
            "m=video {} RTP/AVP {}\r\n",
            "a=rtpmap:{} {}/90000\r\n",
            "a=control:streamid=video\r\n",
            "m=audio {} RTP/AVP 97\r\n",
            "a=rtpmap:97 OPUS/48000/2\r\n",
            "a=control:streamid=audio\r\n",
            "m=application {} udp 0\r\n",
            "a=control:streamid=control\r\n"
        ),
        hevc_probe,
        width,
        height,
        fps,
        GAMESTREAM_VIDEO_PORT,
        payload_type,
        payload_type,
        codec_name,
        GAMESTREAM_AUDIO_PORT,
        GAMESTREAM_CONTROL_PORT,
    )
}

fn gamestream_setup_kind(uri: &str) -> &'static str {
    let lower = uri.to_ascii_lowercase();
    if lower.contains("audio") {
        "audio"
    } else if lower.contains("control") {
        "control"
    } else {
        "video"
    }
}

async fn handle_gamestream_setup(
    runtime: &SunshineRuntime,
    peer: SocketAddr,
    request: &GameStreamRtspRequest,
    stream_kind: &'static str,
) -> Vec<(String, String)> {
    let port = match stream_kind {
        "audio" => GAMESTREAM_AUDIO_PORT,
        "control" => GAMESTREAM_CONTROL_PORT,
        _ => GAMESTREAM_VIDEO_PORT,
    };

    if stream_kind == "video" {
        if let Some(client_port) = parse_client_port(request.headers.get("transport")) {
            let video_client = SocketAddr::new(peer.ip(), client_port);
            if let Some(session) = runtime.launch_session.lock().await.as_mut() {
                session.video_client = Some(video_client);
            }
            tracing::info!(
                peer = %peer,
                video_client = %video_client,
                "Moonlight GameStream video client port learned"
            );
        }
    }

    let mut headers = vec![
        (
            "Session".to_string(),
            format!("{GAMESTREAM_SESSION_ID};timeout = 90"),
        ),
        ("Transport".to_string(), format!("server_port={port}")),
    ];
    if stream_kind == "control" {
        let connect_data = runtime
            .launch_session
            .lock()
            .await
            .as_ref()
            .map(|s| s.control_connect_data)
            .unwrap_or(0);
        headers.push(("X-SS-Connect-Data".to_string(), connect_data.to_string()));
    } else {
        let ping_payload = runtime
            .launch_session
            .lock()
            .await
            .as_ref()
            .map(|s| s.av_ping_payload.clone())
            .unwrap_or_else(|| "0000000000000000".to_string());
        headers.push(("X-SS-Ping-Payload".to_string(), ping_payload));
    }
    headers
}

fn parse_client_port(transport: Option<&String>) -> Option<u16> {
    let transport = transport?;
    for part in transport.split(';') {
        let part = part.trim();
        let Some((name, value)) = part.split_once('=') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        if name != "client_port" && name != "x-gs-clientport" {
            continue;
        }
        let first = value.split('-').next().unwrap_or(value);
        if let Ok(port) = first.parse() {
            return Some(port);
        }
    }
    None
}

async fn start_gamestream_video_if_ready(runtime: Arc<SunshineRuntime>) {
    let Some(session) = runtime.launch_session.lock().await.clone() else {
        tracing::warn!("Moonlight GameStream PLAY without launch session");
        return;
    };
    let Some(video_client) = session.video_client else {
        tracing::warn!("Moonlight GameStream PLAY without video SETUP client port");
        return;
    };

    let sender_runtime = runtime.clone();
    let cleanup_runtime = runtime.clone();
    let old_task = {
        let mut task = runtime.gamestream_video_task.lock().await;
        task.take()
    };
    if let Some(old_task) = old_task {
        old_task.abort();
        match old_task.await {
            Ok(()) => {}
            Err(e) if e.is_cancelled() => {}
            Err(e) => tracing::debug!("Moonlight GameStream previous video sender ended: {}", e),
        }
    }

    let mut task = runtime.gamestream_video_task.lock().await;
    *task = Some(tokio::spawn(async move {
        if let Err(e) = send_gamestream_video_udp(
            sender_runtime,
            video_client,
            session.codec,
            session.video_packet_size,
            session.av_ping_payload,
        )
        .await
        {
            tracing::warn!("Moonlight GameStream video sender failed: {}", e);
        }
        let mut task = cleanup_runtime.gamestream_video_task.lock().await;
        *task = None;
    }));
}

async fn stop_gamestream_session(runtime: &SunshineRuntime, reason: &'static str) {
    let old_task = {
        let mut task = runtime.gamestream_video_task.lock().await;
        task.take()
    };
    if let Some(old_task) = old_task {
        old_task.abort();
        match old_task.await {
            Ok(()) => {}
            Err(e) if e.is_cancelled() => {}
            Err(e) => tracing::debug!("Moonlight GameStream video sender ended: {}", e),
        }
    }
    let had_session = runtime.launch_session.lock().await.take().is_some();
    tracing::info!(reason, had_session, "Moonlight GameStream session stopped");
}

fn stop_gamestream_session_blocking(runtime: &SunshineRuntime, reason: &'static str) {
    if let Some(task) = runtime.gamestream_video_task.blocking_lock().take() {
        task.abort();
    }
    let had_session = runtime.launch_session.blocking_lock().take().is_some();
    tracing::info!(reason, had_session, "Moonlight GameStream session stopped");
}

async fn send_gamestream_video_udp(
    runtime: Arc<SunshineRuntime>,
    video_client: SocketAddr,
    codec: RtspCodec,
    packet_size: usize,
    ping_payload: String,
) -> Result<()> {
    let payload_size = packet_size
        .checked_sub(GAMESTREAM_VIDEO_PACKET_HEADER_SIZE)
        .filter(|size| *size > 0)
        .unwrap_or(GAMESTREAM_DEFAULT_VIDEO_PACKET_SIZE - GAMESTREAM_VIDEO_PACKET_HEADER_SIZE);
    let bind_addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, GAMESTREAM_VIDEO_PORT));
    let socket = UdpSocket::bind(bind_addr).await.map_err(|e| {
        AppError::Io(std::io::Error::new(
            e.kind(),
            format!("GameStream video UDP bind failed: {e}"),
        ))
    })?;
    let video_client = wait_gamestream_video_ping(&socket, video_client, &ping_payload).await;

    runtime
        .video_manager
        .set_video_codec(rtsp_codec_to_video_codec(&codec))
        .await
        .ok();
    let mut rx = runtime
        .video_manager
        .subscribe_encoded_frames()
        .await
        .ok_or_else(|| AppError::VideoError("GameStream failed to subscribe video".into()))?;
    runtime.video_manager.request_keyframe().await.ok();

    tracing::info!(
        client = %video_client,
        codec = ?codec,
        packet_size,
        payload_size,
        "Moonlight GameStream video UDP sender started"
    );

    let mut sequence_number: u16 = rand::rng().random();
    let ssrc: u32 = rand::rng().random();
    let mut stream_packet_index: u32 = 0;
    let mut last_rtp_timestamp: u32 = 0;
    let mut frame_index: u32 = 1;
    let mut sent_frames: u64 = 0;
    let mut waiting_for_keyframe = true;
    let mut skipped_until_keyframe: u64 = 0;

    while let Some(frame) = rx.recv().await {
        if !is_frame_codec_match(&frame, &codec) {
            continue;
        }
        if waiting_for_keyframe {
            if !frame.is_keyframe {
                skipped_until_keyframe = skipped_until_keyframe.saturating_add(1);
                continue;
            }
            waiting_for_keyframe = false;
            tracing::info!(
                skipped = skipped_until_keyframe,
                bytes = frame.data.len(),
                source_sequence = frame.sequence,
                summary = %encoded_frame_summary(&frame),
                "Moonlight GameStream starting video on keyframe"
            );
        }

        let rtp_timestamp =
            monotonic_rtp_timestamp(frame.pts_ms, &mut last_rtp_timestamp, frame.duration);
        let packets = send_gamestream_video_frame(
            &socket,
            video_client,
            &frame,
            frame_index,
            payload_size,
            rtp_timestamp,
            ssrc,
            &mut sequence_number,
            &mut stream_packet_index,
        )
        .await?;
        sent_frames = sent_frames.saturating_add(1);
        if sent_frames
            <= GAMESTREAM_STARTUP_KEYFRAME_INTERVAL_FRAMES * GAMESTREAM_STARTUP_KEYFRAME_REQUESTS
            && sent_frames % GAMESTREAM_STARTUP_KEYFRAME_INTERVAL_FRAMES == 0
        {
            runtime.video_manager.request_keyframe().await.ok();
            tracing::info!(
                after_frames = sent_frames,
                "Moonlight GameStream requested startup keyframe"
            );
        }
        if sent_frames == 1 || sent_frames % 60 == 0 {
            tracing::info!(
                frame = frame_index,
                source_sequence = frame.sequence,
                bytes = frame.data.len(),
                packets,
                keyframe = frame.is_keyframe,
                codec = ?codec,
                "Moonlight GameStream sent video frame"
            );
        }
        frame_index = frame_index.wrapping_add(1).max(1);
    }

    Ok(())
}

async fn wait_gamestream_video_ping(
    socket: &UdpSocket,
    fallback_client: SocketAddr,
    ping_payload: &str,
) -> SocketAddr {
    let mut buf = [0u8; 256];
    let wait = async {
        loop {
            let (len, peer) = socket.recv_from(&mut buf).await?;
            let packet = &buf[..len];
            let payload_match = !ping_payload.is_empty()
                && std::str::from_utf8(packet)
                    .map(|text| text.contains(ping_payload))
                    .unwrap_or(false);
            let legacy_match = packet == b"PING";
            if payload_match || legacy_match {
                return Ok::<SocketAddr, std::io::Error>(peer);
            }
            tracing::debug!(
                peer = %peer,
                bytes = len,
                "Moonlight GameStream ignored non-ping video UDP packet"
            );
        }
    };

    match timeout(Duration::from_secs(5), wait).await {
        Ok(Ok(peer)) => {
            tracing::info!(
                fallback_client = %fallback_client,
                video_client = %peer,
                "Moonlight GameStream video UDP ping learned client port"
            );
            peer
        }
        Ok(Err(e)) => {
            tracing::warn!(
                fallback_client = %fallback_client,
                error = %e,
                "Moonlight GameStream video UDP ping failed; using RTSP client port"
            );
            fallback_client
        }
        Err(_) => {
            tracing::warn!(
                fallback_client = %fallback_client,
                "Moonlight GameStream video UDP ping timed out; using RTSP client port"
            );
            fallback_client
        }
    }
}

async fn send_gamestream_video_frame(
    socket: &UdpSocket,
    video_client: SocketAddr,
    frame: &EncodedVideoFrame,
    frame_index: u32,
    payload_size: usize,
    rtp_timestamp: u32,
    ssrc: u32,
    sequence_number: &mut u16,
    stream_packet_index: &mut u32,
) -> Result<usize> {
    let mut frame_payload = Vec::with_capacity(8 + frame.data.len());
    frame_payload.extend_from_slice(&build_gamestream_short_frame_header(frame, payload_size));
    frame_payload.extend_from_slice(&frame.data);

    let total_packets = frame_payload.len().saturating_add(payload_size - 1) / payload_size;
    if total_packets == 0 {
        return Ok(0);
    }
    if total_packets > 1023 {
        tracing::warn!(
            frame = frame_index,
            source_sequence = frame.sequence,
            bytes = frame.data.len(),
            packets = total_packets,
            "Moonlight GameStream frame too large for no-FEC packetizer"
        );
        return Ok(0);
    }

    for (idx, chunk) in frame_payload.chunks(payload_size).enumerate() {
        let mut packet = Vec::with_capacity(12 + 4 + 16 + chunk.len());
        packet.push(0x90);
        packet.push(0);
        packet.extend_from_slice(&sequence_number.to_be_bytes());
        packet.extend_from_slice(&rtp_timestamp.to_be_bytes());
        packet.extend_from_slice(&ssrc.to_be_bytes());
        packet.extend_from_slice(&[0, 0, 0, 0]);

        let flags = 0x01
            | if idx == 0 { 0x04 } else { 0 }
            | if idx + 1 == total_packets { 0x02 } else { 0 };
        let fec_info = ((idx as u32 & 0x3ff) << 12) | (((total_packets as u32).min(1023)) << 22);
        packet.extend_from_slice(&((*stream_packet_index & 0x00ff_ffff) << 8).to_le_bytes());
        packet.extend_from_slice(&frame_index.to_le_bytes());
        packet.push(flags);
        packet.push(0);
        packet.push(0x10);
        packet.push(0);
        packet.extend_from_slice(&fec_info.to_le_bytes());
        packet.extend_from_slice(chunk);

        socket.send_to(&packet, video_client).await?;
        *sequence_number = sequence_number.wrapping_add(1);
        *stream_packet_index = stream_packet_index.wrapping_add(1);

        if total_packets > GAMESTREAM_VIDEO_PACE_BATCH_PACKETS
            && idx + 1 < total_packets
            && (idx + 1) % GAMESTREAM_VIDEO_PACE_BATCH_PACKETS == 0
        {
            sleep(Duration::from_millis(1)).await;
        }
    }

    Ok(total_packets)
}

fn build_gamestream_short_frame_header(frame: &EncodedVideoFrame, payload_size: usize) -> [u8; 8] {
    let mut header = [0u8; 8];
    header[0] = 0x01;
    header[3] = if frame.is_keyframe { 2 } else { 1 };
    let last_payload_len = match (frame.data.len() + header.len()) % payload_size {
        0 => payload_size,
        len => len,
    }
    .min(u16::MAX as usize) as u16;
    header[4..6].copy_from_slice(&last_payload_len.to_le_bytes());
    header
}

fn encoded_frame_summary(frame: &EncodedVideoFrame) -> String {
    let first_bytes = frame
        .data
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    match frame.codec {
        VideoEncoderType::H265 => {
            format!(
                "first_bytes=[{}] h265_nals={:?}",
                first_bytes,
                parse_h265_nal_types(frame.data.as_ref())
            )
        }
        VideoEncoderType::H264 => {
            format!(
                "first_bytes=[{}] h264_nals={:?}",
                first_bytes,
                parse_h264_nal_types(frame.data.as_ref())
            )
        }
        _ => format!("first_bytes=[{}]", first_bytes),
    }
}

fn parse_h265_nal_types(data: &[u8]) -> Vec<(u8, usize)> {
    parse_annex_b_nals(data)
        .into_iter()
        .map(|(nal, len)| ((nal >> 1) & 0x3f, len))
        .collect()
}

fn parse_h264_nal_types(data: &[u8]) -> Vec<(u8, usize)> {
    parse_annex_b_nals(data)
        .into_iter()
        .map(|(nal, len)| (nal & 0x1f, len))
        .collect()
}

fn parse_annex_b_nals(data: &[u8]) -> Vec<(u8, usize)> {
    let mut nals = Vec::new();
    let mut offset = 0;
    while let Some((nal_start, prefix_start)) = find_start_code(data, offset) {
        let next_search = nal_start.saturating_add(1);
        let nal_end = find_start_code(data, next_search)
            .map(|(_, next_prefix_start)| next_prefix_start)
            .unwrap_or(data.len());
        if nal_start < nal_end {
            nals.push((data[nal_start], nal_end - nal_start));
        }
        offset = nal_end;
        if prefix_start >= data.len() {
            break;
        }
    }
    nals
}

fn find_start_code(data: &[u8], mut offset: usize) -> Option<(usize, usize)> {
    while offset + 3 <= data.len() {
        if data[offset] == 0 && data[offset + 1] == 0 && data[offset + 2] == 1 {
            return Some((offset + 3, offset));
        }
        if offset + 4 <= data.len()
            && data[offset] == 0
            && data[offset + 1] == 0
            && data[offset + 2] == 0
            && data[offset + 3] == 1
        {
            return Some((offset + 4, offset));
        }
        offset += 1;
    }
    None
}

fn rtsp_codec_to_video_codec(codec: &RtspCodec) -> crate::video::encoder::VideoCodecType {
    match codec {
        RtspCodec::H264 => crate::video::encoder::VideoCodecType::H264,
        RtspCodec::H265 => crate::video::encoder::VideoCodecType::H265,
    }
}

fn pts_to_rtp_timestamp(pts_ms: i64) -> u32 {
    if pts_ms <= 0 {
        return 0;
    }
    ((pts_ms as u64 * RTP_CLOCK_RATE as u64) / 1000) as u32
}

fn rtp_timestamp_increment(frame_duration: Duration) -> u32 {
    let inc = (frame_duration.as_secs_f64() * f64::from(RTP_CLOCK_RATE)).round() as u32;
    inc.max(1)
}

fn monotonic_rtp_timestamp(pts_ms: i64, last: &mut u32, frame_duration: Duration) -> u32 {
    let from_pts = pts_to_rtp_timestamp(pts_ms);
    let inc = rtp_timestamp_increment(frame_duration);
    let ts = if from_pts > *last {
        from_pts
    } else {
        last.wrapping_add(inc)
    };
    *last = ts;
    ts
}

fn is_frame_codec_match(frame: &EncodedVideoFrame, codec: &RtspCodec) -> bool {
    matches!(
        (frame.codec, codec),
        (VideoEncoderType::H264, RtspCodec::H264) | (VideoEncoderType::H265, RtspCodec::H265)
    )
}

async fn cancel(State(runtime): State<Arc<SunshineRuntime>>) -> Response {
    stop_gamestream_session(&runtime, "HTTP cancel").await;
    xml_response(r#"<root status_code="200"><cancel>1</cancel></root>"#.to_string())
}

async fn clients(State(runtime): State<Arc<SunshineRuntime>>) -> Response {
    let clients = runtime.clients.lock().await.clone();
    (
        StatusCode::OK,
        [("content-type", "application/json; charset=utf-8")],
        serde_json::to_string(&clients).unwrap_or_else(|_| "[]".to_string()),
    )
        .into_response()
}

async fn spawn_mdns_responder(
    runtime: Arc<SunshineRuntime>,
    mut shutdown: broadcast::Receiver<()>,
) -> Option<tokio::task::JoinHandle<()>> {
    let std_socket = match std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, MDNS_PORT)) {
        Ok(socket) => socket,
        Err(e) => {
            tracing::warn!("Moonlight mDNS discovery bind failed: {}", e);
            return None;
        }
    };
    if let Err(e) = std_socket.set_nonblocking(true) {
        tracing::warn!("Moonlight mDNS discovery nonblocking setup failed: {}", e);
        return None;
    }
    if let Err(e) = std_socket.join_multicast_v4(&MDNS_ADDR, &Ipv4Addr::UNSPECIFIED) {
        tracing::warn!("Moonlight mDNS multicast join failed: {}", e);
        return None;
    }
    let socket = match UdpSocket::from_std(std_socket) {
        Ok(socket) => socket,
        Err(e) => {
            tracing::warn!("Moonlight mDNS socket setup failed: {}", e);
            return None;
        }
    };

    Some(tokio::spawn(async move {
        tracing::info!(
            "Moonlight mDNS discovery responder listening on udp/{}",
            MDNS_PORT
        );
        let mut buf = [0u8; 1500];
        loop {
            tokio::select! {
                _ = shutdown.recv() => break,
                received = socket.recv_from(&mut buf) => {
                    let Ok((len, peer)) = received else {
                        continue;
                    };
                    if !mdns_query_mentions_nvstream(&buf[..len], &runtime.config.hostname) {
                        continue;
                    }
                    let local_ip = local_ip_for_peer(peer);
                    let Ok(ip) = local_ip.parse::<Ipv4Addr>() else {
                        continue;
                    };
                    let response = build_mdns_response(&buf[..len], &runtime, ip);
                    if let Err(e) = socket.send_to(&response, (MDNS_ADDR, MDNS_PORT)).await {
                        tracing::debug!("Moonlight mDNS response failed: {}", e);
                    }
                }
            }
        }
    }))
}

fn mdns_query_mentions_nvstream(packet: &[u8], hostname: &str) -> bool {
    if packet.len() < 12 {
        return false;
    }
    let qdcount = u16::from_be_bytes([packet[4], packet[5]]) as usize;
    let host = format!("{}.local", mdns_label(hostname));
    let mut offset = 12usize;
    for _ in 0..qdcount {
        let Some((name, next)) = read_dns_name(packet, offset) else {
            return false;
        };
        offset = next.saturating_add(4);
        let name = name.to_ascii_lowercase();
        if name == MDNS_SERVICE || name == host.to_ascii_lowercase() {
            return true;
        }
    }
    false
}

fn build_mdns_response(packet: &[u8], runtime: &SunshineRuntime, ip: Ipv4Addr) -> Vec<u8> {
    let id = if packet.len() >= 2 {
        [packet[0], packet[1]]
    } else {
        [0, 0]
    };
    let instance = format!(
        "{}._nvstream._tcp.local",
        mdns_label(&runtime.config.hostname)
    );
    let host = format!("{}.local", mdns_label(&runtime.config.hostname));
    let txt = vec![
        "txtvers=1".to_string(),
        format!("name={}", runtime.config.hostname),
        format!("uuid={}", runtime.unique_id),
        "version=7.1".to_string(),
    ];

    let mut out = Vec::with_capacity(512);
    out.extend_from_slice(&id);
    out.extend_from_slice(&0x8400u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&4u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());

    push_dns_record(&mut out, MDNS_SERVICE, 12, 1, 120, |rdata| {
        push_dns_name(rdata, &instance);
    });
    push_dns_record(&mut out, &instance, 33, 0x8001, 120, |rdata| {
        rdata.extend_from_slice(&0u16.to_be_bytes());
        rdata.extend_from_slice(&0u16.to_be_bytes());
        rdata.extend_from_slice(&runtime.config.http_port.to_be_bytes());
        push_dns_name(rdata, &host);
    });
    push_dns_record(&mut out, &instance, 16, 0x8001, 120, |rdata| {
        for value in &txt {
            let bytes = value.as_bytes();
            let len = bytes.len().min(255);
            rdata.push(len as u8);
            rdata.extend_from_slice(&bytes[..len]);
        }
    });
    push_dns_record(&mut out, &host, 1, 0x8001, 120, |rdata| {
        rdata.extend_from_slice(&ip.octets());
    });

    out
}

fn push_dns_record<F>(
    out: &mut Vec<u8>,
    name: &str,
    record_type: u16,
    class: u16,
    ttl: u32,
    fill: F,
) where
    F: FnOnce(&mut Vec<u8>),
{
    push_dns_name(out, name);
    out.extend_from_slice(&record_type.to_be_bytes());
    out.extend_from_slice(&class.to_be_bytes());
    out.extend_from_slice(&ttl.to_be_bytes());
    let len_pos = out.len();
    out.extend_from_slice(&0u16.to_be_bytes());
    let start = out.len();
    fill(out);
    let len = (out.len() - start) as u16;
    out[len_pos..len_pos + 2].copy_from_slice(&len.to_be_bytes());
}

fn push_dns_name(out: &mut Vec<u8>, name: &str) {
    for label in name.trim_end_matches('.').split('.') {
        let bytes = label.as_bytes();
        let len = bytes.len().min(63);
        out.push(len as u8);
        out.extend_from_slice(&bytes[..len]);
    }
    out.push(0);
}

fn read_dns_name(packet: &[u8], mut offset: usize) -> Option<(String, usize)> {
    let mut labels = Vec::new();
    let mut jumped = false;
    let mut next = offset;
    for _ in 0..32 {
        let len = *packet.get(offset)?;
        if len & 0xc0 == 0xc0 {
            let b2 = *packet.get(offset + 1)? as usize;
            let pointer = (((len & 0x3f) as usize) << 8) | b2;
            if !jumped {
                next = offset + 2;
            }
            offset = pointer;
            jumped = true;
            continue;
        }
        if len == 0 {
            if !jumped {
                next = offset + 1;
            }
            return Some((labels.join("."), next));
        }
        offset += 1;
        let end = offset.checked_add(len as usize)?;
        let label = std::str::from_utf8(packet.get(offset..end)?).ok()?;
        labels.push(label.to_string());
        offset = end;
    }
    None
}

fn mdns_label(input: &str) -> String {
    let mut label = String::new();
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' {
            label.push(ch);
        } else if ch.is_whitespace() || ch == '_' {
            label.push('-');
        }
    }
    let label = label.trim_matches('-');
    if label.is_empty() {
        "One-KVM".to_string()
    } else {
        label.to_string()
    }
}

async fn not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        [("content-type", "application/xml; charset=utf-8")],
        r#"<root status_code="404"></root>"#,
    )
}

fn xml_response(body: String) -> Response {
    (
        StatusCode::OK,
        [("content-type", "application/xml; charset=utf-8")],
        body,
    )
        .into_response()
}

fn pair_error(status: u16, message: &str) -> Response {
    tracing::warn!(status, message, "Moonlight pairing error");
    xml_response(format!(
        r#"<root status_code="{status}" status_message="{}"><paired>0</paired></root>"#,
        xml_escape(message),
    ))
}

fn parse_addr(bind: &str, port: u16, label: &str) -> Result<SocketAddr> {
    format!("{bind}:{port}")
        .parse()
        .map_err(|e| AppError::BadRequest(format!("Invalid {label} bind address: {e}")))
}

async fn ensure_host_credentials(paths: &SunshinePaths) -> Result<HostCredentials> {
    if let (Ok(cert_pem), Ok(key_pem)) = (
        tokio::fs::read_to_string(&paths.cert).await,
        tokio::fs::read_to_string(&paths.key).await,
    ) {
        if let Ok(signing_key) = KeyPair::from_pem(&key_pem) {
            if let Ok(cert_der) = CertificateDer::from_pem_slice(cert_pem.as_bytes()) {
                return Ok(HostCredentials {
                    cert_pem,
                    cert_der: cert_der.as_ref().to_vec(),
                    key_der: signing_key.serialize_der(),
                    signing_key,
                });
            }
            tracing::warn!("Existing Sunshine certificate PEM is invalid; regenerating host cert");
        }
    }

    let signing_key = KeyPair::generate_for(&PKCS_RSA_SHA256)
        .or_else(|_| KeyPair::generate())
        .map_err(|e| AppError::Internal(format!("generate Sunshine key failed: {e}")))?;
    let params = CertificateParams::new(vec!["Sunshine Gamestream Host".to_string()])
        .map_err(|e| AppError::Internal(format!("create cert params failed: {e}")))?;
    let cert = params
        .self_signed(&signing_key)
        .map_err(|e| AppError::Internal(format!("generate Sunshine cert failed: {e}")))?;
    let cert_pem = cert.pem();
    let cert_der = cert.der().as_ref().to_vec();
    let key_der = signing_key.serialize_der();
    tokio::fs::write(&paths.cert, &cert_pem).await?;
    tokio::fs::write(&paths.key, signing_key.serialize_pem()).await?;
    Ok(HostCredentials {
        cert_pem,
        cert_der,
        key_der,
        signing_key,
    })
}

async fn load_state(paths: &SunshinePaths, config: &SunshineConfig) -> SunshineStateFile {
    if let Ok(bytes) = tokio::fs::read(&paths.state).await {
        if let Ok(mut state) = serde_json::from_slice::<SunshineStateFile>(&bytes) {
            if state.unique_id.trim().is_empty() {
                state.unique_id = derive_unique_id(config).await;
            }
            return state;
        }
    }
    SunshineStateFile {
        unique_id: derive_unique_id(config).await,
        clients: Vec::new(),
    }
}

async fn derive_unique_id(config: &SunshineConfig) -> String {
    if !config.unique_id.trim().is_empty() {
        return config.unique_id.clone();
    }
    let seed = tokio::fs::read_to_string("/etc/machine-id")
        .await
        .unwrap_or_else(|_| config.hostname.clone());
    let digest = Sha256::digest(seed.trim().as_bytes());
    hex_encode(&digest[..8])
}

fn build_tls_config(runtime: &SunshineRuntime) -> Result<axum_server::tls_rustls::RustlsConfig> {
    let cert = CertificateDer::from(runtime.cert_der.clone());
    let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(runtime.key_der.clone()));
    let mut server_config = ServerConfig::builder()
        .with_client_cert_verifier(Arc::new(AnyMoonlightClientCert))
        .with_single_cert(vec![cert], key)
        .map_err(|e| AppError::Internal(format!("build Sunshine TLS config failed: {e}")))?;
    server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(axum_server::tls_rustls::RustlsConfig::from_config(
        Arc::new(server_config),
    ))
}

#[derive(Debug)]
struct AnyMoonlightClientCert;

impl ClientCertVerifier for AnyMoonlightClientCert {
    fn client_auth_mandatory(&self) -> bool {
        false
    }

    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> std::result::Result<ClientCertVerified, TlsError> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ED25519,
        ]
    }
}

impl SunshinePaths {
    fn new(data_dir: &Path) -> Self {
        let dir = data_dir.join("sunshine");
        Self {
            cert: dir.join("cert.pem"),
            key: dir.join("key.pem"),
            state: dir.join("state.json"),
            dir,
        }
    }
}

fn derive_pin_key(salt: &[u8], pin: &str) -> [u8; 16] {
    let mut input = Vec::with_capacity(salt.len() + pin.len());
    input.extend_from_slice(salt);
    input.extend_from_slice(pin.as_bytes());
    let digest = Sha256::digest(&input);
    let mut key = [0u8; 16];
    key.copy_from_slice(&digest[..16]);
    key
}

fn aes128_ecb_decrypt_no_padding(
    key: &[u8; 16],
    ciphertext: &[u8],
) -> std::result::Result<Vec<u8>, ()> {
    if ciphertext.is_empty() || ciphertext.len() % 16 != 0 {
        return Err(());
    }
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut out = ciphertext.to_vec();
    for block in out.chunks_mut(16) {
        cipher.decrypt_block(GenericArray::from_mut_slice(block));
    }
    Ok(out)
}

fn aes128_ecb_encrypt_no_padding(
    key: &[u8; 16],
    plaintext: &[u8],
) -> std::result::Result<Vec<u8>, ()> {
    if plaintext.is_empty() || plaintext.len() % 16 != 0 {
        return Err(());
    }
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut out = plaintext.to_vec();
    for block in out.chunks_mut(16) {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    Ok(out)
}

fn cert_signature_from_pem(pem: &str) -> std::result::Result<Vec<u8>, ()> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).map_err(|_| ())?;
    cert_signature(&pem.contents)
}

fn cert_signature(der: &[u8]) -> std::result::Result<Vec<u8>, ()> {
    let (_, cert) = parse_x509_certificate(der).map_err(|_| ())?;
    Ok(cert.signature_value.data.to_vec())
}

fn hex_decode(input: &str) -> std::result::Result<Vec<u8>, ()> {
    if input.len() % 2 != 0 {
        return Err(());
    }
    let mut out = Vec::with_capacity(input.len() / 2);
    for pair in input.as_bytes().chunks_exact(2) {
        let hi = hex_val(pair[0]).ok_or(())?;
        let lo = hex_val(pair[1]).ok_or(())?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn hex_encode(input: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(input.len() * 2);
    for byte in input {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn random_bytes(len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    rand::rng().fill_bytes(&mut bytes);
    bytes
}

fn local_ip_for_peer(peer: SocketAddr) -> String {
    let bind = if peer.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    };
    std::net::UdpSocket::bind(bind)
        .and_then(|socket| {
            socket.connect(peer)?;
            socket.local_addr()
        })
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_xml_special_chars() {
        assert_eq!(
            xml_escape("HDMI & <Input> \"A\""),
            "HDMI &amp; &lt;Input&gt; &quot;A&quot;"
        );
    }

    #[test]
    fn aes_ecb_round_trip() {
        let key = [7u8; 16];
        let plaintext = b"moonlight-16byte";
        let encrypted = aes128_ecb_encrypt_no_padding(&key, plaintext).unwrap();
        assert_ne!(encrypted, plaintext);
        assert_eq!(
            aes128_ecb_decrypt_no_padding(&key, &encrypted).unwrap(),
            plaintext
        );
    }

    #[test]
    fn hex_round_trip() {
        let data = b"abc123";
        assert_eq!(hex_decode(&hex_encode(data)).unwrap(), data);
    }
}
