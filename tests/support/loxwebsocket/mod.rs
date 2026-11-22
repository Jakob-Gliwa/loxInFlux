//! Shared harness for the integration tests: a configurable fake Miniserver
//! plus the observation helpers the tests assert against.
//!
//! # What the fake actually implements
//!
//! Enough of the protocol document (V17.0) that the real [`loxwebsocket::LoxClient`]
//! completes a full handshake against it and cannot tell the difference:
//!
//! * `GET /jdev/sys/getPublicKey`, `GET /jdev/cfg/apiKey`, `GET /jdev/cfg/api`
//!   over plain HTTP/1.1, each in the firmware variants the client has to
//!   tolerate (SPKI mislabeled as `CERTIFICATE` and unwrapped; `LL.value` as a
//!   single-quoted string).
//! * The WebSocket upgrade on `/ws/rfc6455` with `Sec-WebSocket-Protocol:
//!   remotecontrol`.
//! * `jdev/sys/keyexchange/{b64}`: real RSA PKCS#1 v1.5 decryption of the
//!   session key, which is then used for AES-256-CBC command decryption.
//! * The full token lifecycle (`getkey2`, `getjwt`, `getkey`, `checktoken`,
//!   `authwithtoken`, `refreshjwt`, `killtoken`), `getvisusalt`,
//!   `enablebinstatusupdate` and `keepalive`.
//!
//! Credential and token hashes are verified with an implementation written
//! independently of the crate's own (`hmac`/`sha1`/`sha2` used directly here),
//! so a regression in the client's hashing shows up as a `401` rather than
//! passing unnoticed.
//!
//! # Determinism
//!
//! The RSA key is a fixed 2048-bit test key rather than a freshly generated
//! one: generating a 2048-bit key with `rsa` in an unoptimized test build takes
//! upwards of ten seconds and varies by a factor of several, which no amount of
//! per-test timeout budgeting makes pleasant. Everything else — session keys,
//! salts, tokens — is generated at run time.
//!
//! Tests never sleep to wait for the client. They either await a channel from
//! the recording handler or [`FakeState::wait_until`], which parks on a
//! `Notify` and fails with a bounded timeout.

#![allow(dead_code)]

pub mod server;
pub mod ws;

use loxwebsocket::proto::{DaytimerEntry, WeatherEntry};
use loxwebsocket::{
    ClientEvent, ConnectConfig, DaytimerEvent, LoxHandler, LoxoneUuid, WeatherEvent,
};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc, oneshot};
use zerocopy::IntoBytes;

pub const TEST_USER: &str = "admin";
pub const TEST_PASSWORD: &str = "hunter2";
pub const TEST_VISU_PASSWORD: &str = "1234";

/// `validUntil` far enough out that the client always considers the token
/// reusable (seconds since 2009-01-01, i.e. some time in 2049).
pub const FAR_FUTURE_VALID_UNTIL: i64 = 40 * 365 * 24 * 3600;

/// Fixed 2048-bit RSA test key. See the module docs for why it is not generated.
pub const TEST_RSA_PRIVATE_KEY_PEM: &str = "\
-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCR+W6MIZDVv1Px
Y08xZQDV6Z6waYg8T7qXHx2jfwO+1I9/Euq4bW4YDneEmH84Ghoq/5A/cpz14z7A
Vi6Ct2ZjKQIM/HNiGUq2OkoB0rf6anc6tCSWVoYkCH1O+LPB1Yvg6wtB4bye8Zfb
Atr8g1xn2Jv6JyCu2cYzkIWJ42X6g3YCF33EoO5lCCFmwUWv9fXyApV1POX2PqFD
gxlNnDtGdN7DFEcTxpKCWpWAFhOJ4nWljnYOlfLo7Dgc+qZXuN8QZ6Ry0bHFUPKw
1TL7o3hwC4xUowqzo0KpYNclIkTn+V9+Gw70I1XdoxH/6CWwpRP5S3W/36FPpRgk
vdjVsS9tAgMBAAECggEAAsBYDlGapIC68Q+NYFG2SpHg8RPIItTg4DTQrvJ3rFre
yocdf/TmEJODOq9SJIlPaXSQMDX1kefi2Ka3MTUKO7+732lJtnViFF20Y+ToHVLw
5N0c3G2MkTTMwdaLstFW1dowR+FcmAVXNqRO4tgJ/5YUWIpwwgLuSq4EalUsKKTW
5Ia6+jq9wkSWoPAhcuatHykX5KywpTQ75fg+s33/a6kkAW/EU7pwzLj8r++pZ8X2
j5T9y2mgtMcCYduPHwwZR4wI+gtmlbd6tMwZdXf6NGnsRCAbQiC2OeTmINF/nurg
XY7/BwoD2hnaedJdrR/pZOHNYmdiLag8W4IRfNnUcQKBgQDElZks1bod1TYMmwgO
vmU04xZC8s/aBhmWUOAsp7fk21EhuJp8/OjLgCQ2SwYt4a/mncJH8o79aKt+Zfoy
h8oszo+Ajl4s3TG+WROh4mlUck6EBEGgYMftZT+fQ/o+hXBSuhZbCTEK6sAokUFV
fGIODHDw9IHJEj6FjWgmQbmFUQKBgQC+F/asVjhRABNijydw3LYa/B9y8I9Hiwiu
RFAysx1L3jMyERpZm5p39mRLo3OdcgexecWS5wpNV0b2J4xY87A/abnGGOjlnT28
wbLnJKOKcd+AZxmuQGsJSegeFZAtrOKf+av74F49Cg4ojdKyrHIq6NLLoTA70Biy
dNgBCYNxXQKBgQCXjGiAiuenNgYr45xrmVX2VpaD2CJqhsdU/VZEtqtqz7SVFXZr
oqFouImyHVZPKqxrUfVDd/fJ3dZPZBhkuhAfSMKSLa7mUUOW5Z7f7uaahmCHH6zk
EZgvKB3LDyGs7zvvWqv/VG+tZdnrrEc8ut3wzKCI8UXYl6sBVEkVLRfzcQKBgBhY
J04QyKuO7+yaWrm4elXgXgKxThgidR0kQIUNrT3PGg1aZV5+b/zXACczqpXKSbPv
3V6f2hDnkX3quK2Xn8WvO4xkGkd1qLdoswmpBoyvYqkmCwLm2w5YebKInmtLDcbh
CaZ7KHZ2uDN3XjllnkVihcRwQyYV02PfVN5lIoE9AoGAdbHOfcT4Eawr0lrZIqOL
9ONAT6RzW3lg2X2hGJiufqieWDS0tpp3ZrRHDoWiUbVjWDn4nwqXXYW+LHKed/Or
fd1BgH9gBsn48e8zSI6aaXCsmgK1+ZxE4WSiNrqPSiBD3uBSNXzZuoJZo4A8+631
g2eyrdd/R0r7F/6RIjzOI/g=
-----END PRIVATE KEY-----
";

/// Hash algorithm the fake announces in `getkey2` / `getvisusalt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgName {
    Sha1,
    Sha256,
}

impl HashAlgName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sha1 => "SHA1",
            Self::Sha256 => "SHA256",
        }
    }
}

/// How `jdev/sys/getPublicKey` presents the SubjectPublicKeyInfo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicKeyStyle {
    /// Correct multi-line `PUBLIC KEY` PEM.
    Spki,
    /// What Miniservers actually send: an SPKI labeled `CERTIFICATE`, on a
    /// single line with no wrapping.
    CertificateOneLine,
}

/// How `jdev/cfg/apiKey` and `jdev/cfg/api` encode `LL.value`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiValueStyle {
    /// A JSON object.
    Object,
    /// A JSON *string* containing a single-quoted pseudo-object.
    SingleQuotedString,
}

/// Everything about the fake a test may want to vary.
#[derive(Debug, Clone)]
pub struct FakeConfig {
    /// Bridge-test extensions: configuration HTTP files and an /all value.
    pub files: std::collections::HashMap<String, Vec<u8>>,
    pub poll_value: Option<String>,
    pub public_key_style: PublicKeyStyle,
    pub api_value_style: ApiValueStyle,
    pub hash_alg: HashAlgName,
    /// Serial number reported by the reachability endpoints.
    pub snr: Option<String>,
    pub version: Option<String>,
    pub https_status: Option<u8>,
    pub local: Option<bool>,
    /// `hasEventSlots`, reported on `jdev/cfg/api` as the document specifies.
    pub has_event_slots: Option<bool>,
    /// Answer `GET /jdev/cfg/apiKey` at all.
    pub serve_api_key: bool,
    /// Answer `GET /jdev/cfg/api` at all.
    pub serve_api: bool,
    pub token_valid_until: i64,
    /// Answer plaintext `keepalive` with a type-6 message.
    pub answer_keepalive: bool,
    /// Reject `checktoken` / `authwithtoken` with LL `401`.
    pub reject_token: bool,
    /// Refuse `jdev/sys/keyexchange` with this LL code instead of installing
    /// the session key, the way a Miniserver that is still booting does.
    pub keyexchange_refusal: Option<u16>,
    /// Send `enablebinstatusupdate` a bare counter instead of an LL envelope,
    /// which some firmwares do.
    pub bare_enable_updates_reply: bool,
    /// Push the initial value table *before* acknowledging
    /// `enablebinstatusupdate`, which some firmwares do: enabling updates is
    /// what starts the push, and the acknowledgement can lose the race.
    pub tables_before_enable_ack: bool,
}

impl Default for FakeConfig {
    fn default() -> Self {
        Self {
            files: Default::default(),
            poll_value: None,
            public_key_style: PublicKeyStyle::Spki,
            api_value_style: ApiValueStyle::Object,
            hash_alg: HashAlgName::Sha256,
            snr: Some("504F94A0B1C2".into()),
            version: Some("15.2.11.4".into()),
            https_status: None,
            local: Some(true),
            has_event_slots: Some(true),
            serve_api_key: true,
            serve_api: true,
            token_valid_until: FAR_FUTURE_VALID_UNTIL,
            answer_keepalive: true,
            reject_token: false,
            keyexchange_refusal: None,
            bare_enable_updates_reply: false,
            tables_before_enable_ack: false,
        }
    }
}

/// The value table [`FakeConfig::tables_before_enable_ack`] pushes ahead of the
/// acknowledgement, so the test and the fake agree on what must arrive.
pub fn early_table_records() -> Vec<(LoxoneUuid, f64)> {
    vec![(uuid(0xa1), 21.5), (uuid(0xa2), 0.0)]
}

/// One decrypted command the fake processed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRecord {
    pub session: usize,
    /// Command verb, e.g. `jdev/sys/getjwt` — never the hash arguments.
    pub label: String,
    /// Decrypted plaintext *including* the `salt/…` / `nextSalt/…` prefix.
    pub salted: String,
    /// Decrypted plaintext with the salt prefix stripped.
    pub cmd: String,
    /// LL status code the fake answered with.
    pub code: String,
}

/// The salt prefix a command was sent under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaltPrefix {
    /// `salt/{salt}/`
    Same(String),
    /// `nextSalt/{prev}/{next}/`
    Rotated { prev: String, next: String },
    /// No recognizable prefix.
    None,
}

impl CommandRecord {
    pub fn salt_prefix(&self) -> SaltPrefix {
        if let Some(rest) = self.salted.strip_prefix("nextSalt/") {
            let mut parts = rest.splitn(3, '/');
            if let (Some(prev), Some(next)) = (parts.next(), parts.next()) {
                return SaltPrefix::Rotated {
                    prev: prev.to_string(),
                    next: next.to_string(),
                };
            }
        }
        if let Some(rest) = self.salted.strip_prefix("salt/")
            && let Some((salt, _)) = rest.split_once('/')
        {
            return SaltPrefix::Same(salt.to_string());
        }
        SaltPrefix::None
    }
}

/// Ordered record of everything the fake observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    HttpGet {
        path: String,
    },
    /// WebSocket upgrade accepted; `session` counts from zero.
    SessionOpened {
        session: usize,
    },
    /// `jdev/sys/keyexchange` accepted and the AES session installed.
    KeyExchange {
        session: usize,
    },
    Command(CommandRecord),
    Keepalive {
        session: usize,
    },
    /// A close frame arrived from the client.
    ClientClose {
        session: usize,
        code: Option<u16>,
    },
    SessionClosed {
        session: usize,
    },
}

impl Entry {
    pub fn as_command(&self) -> Option<&CommandRecord> {
        match self {
            Self::Command(record) => Some(record),
            _ => None,
        }
    }
}

/// Instruction for a live WebSocket session.
#[derive(Debug)]
pub enum Ctrl {
    /// Write each blob with its own `write_all` + `flush`, pausing `gap`
    /// between them. Splitting one frame across blobs is how a partial frame
    /// gets onto the wire.
    Write { blobs: Vec<Vec<u8>>, gap: Duration },
    /// Pause before the next instruction, without touching the socket.
    Sleep(Duration),
    /// Drop the TCP connection without a close frame.
    Abort,
    /// Answered once every instruction queued before it has been written.
    Sync(oneshot::Sender<()>),
}

/// Handle for pushing frames into one live session.
#[derive(Debug, Clone)]
pub struct SessionHandle {
    pub index: usize,
    tx: mpsc::UnboundedSender<Ctrl>,
}

impl SessionHandle {
    fn send(&self, ctrl: Ctrl) {
        let _ = self.tx.send(ctrl);
    }

    /// Loxone message: the 8-byte header frame, then the payload frame.
    pub fn send_message(&self, msg_type: u8, payload: &[u8]) {
        self.send_message_with_opcode(msg_type, payload, ws::OP_BINARY);
    }

    /// Like [`Self::send_message`] but chooses the payload frame's opcode; the
    /// Miniserver sends type-0 payloads as text frames.
    pub fn send_message_with_opcode(&self, msg_type: u8, payload: &[u8], opcode: u8) {
        self.send(Ctrl::Write {
            blobs: vec![
                ws::frame(
                    ws::OP_BINARY,
                    &lox_header(msg_type, 0, payload.len() as u32),
                ),
                ws::frame(opcode, payload),
            ],
            gap: Duration::ZERO,
        });
    }

    /// An estimated header announcing `announced`, then the exact header and
    /// the payload — the sequence a Gateway Miniserver produces.
    pub fn send_estimated_then_message(&self, msg_type: u8, announced: u32, payload: &[u8]) {
        self.send(Ctrl::Write {
            blobs: vec![
                ws::frame(ws::OP_BINARY, &lox_header(msg_type, 0x01, announced)),
                ws::frame(
                    ws::OP_BINARY,
                    &lox_header(msg_type, 0, payload.len() as u32),
                ),
                ws::frame(ws::OP_BINARY, payload),
            ],
            gap: Duration::ZERO,
        });
    }

    /// A bare header with no payload frame behind it.
    pub fn send_header(&self, msg_type: u8, info: u8, len: u32) {
        self.send(Ctrl::Write {
            blobs: vec![ws::frame(ws::OP_BINARY, &lox_header(msg_type, info, len))],
            gap: Duration::ZERO,
        });
    }

    /// Identifier 5: no payload follows and the Miniserver closes afterwards.
    pub fn send_out_of_service(&self) {
        self.send_header(loxwebsocket::MessageType::OutOfService as u8, 0, 0);
    }

    /// Header frame, then the payload frame spread over `chunk`-sized TCP
    /// writes with `gap` between them.
    ///
    /// The Loxone header and the WebSocket frame header both go out at once, so
    /// the client learns the announced length and the frame length before the
    /// bytes arrive; only the payload trickles.
    pub fn send_message_in_tcp_chunks(
        &self,
        msg_type: u8,
        payload: &[u8],
        chunk: usize,
        gap: Duration,
    ) {
        let split = chunk.min(payload.len());
        let mut opening = ws::frame(
            ws::OP_BINARY,
            &lox_header(msg_type, 0, payload.len() as u32),
        );
        opening.extend_from_slice(&ws::frame_header(ws::OP_BINARY, payload.len()));
        opening.extend_from_slice(&payload[..split]);
        self.send(Ctrl::Write {
            blobs: vec![opening],
            gap: Duration::ZERO,
        });
        for rest in payload[split..].chunks(chunk.max(1)) {
            self.send(Ctrl::Sleep(gap));
            self.send(Ctrl::Write {
                blobs: vec![rest.to_vec()],
                gap: Duration::ZERO,
            });
        }
    }

    /// Raw bytes, one `write_all` per blob.
    pub fn send_raw(&self, blobs: Vec<Vec<u8>>, gap: Duration) {
        self.send(Ctrl::Write { blobs, gap });
    }

    pub fn close(&self, code: u16) {
        self.send(Ctrl::Write {
            blobs: vec![ws::close_frame(code)],
            gap: Duration::ZERO,
        });
    }

    /// Drop the connection without a close frame.
    pub fn abort(&self) {
        self.send(Ctrl::Abort);
    }

    /// Wait until everything queued so far has hit the socket.
    pub async fn flushed(&self) {
        let (tx, rx) = oneshot::channel();
        self.send(Ctrl::Sync(tx));
        let _ = within(5, "session flush", rx).await;
    }
}

/// Shared, observable state of the fake.
#[derive(Debug)]
pub struct FakeState {
    pub cfg: FakeConfig,
    pub(crate) enable_reply_gate: tokio::sync::Semaphore,
    log: Mutex<Vec<Entry>>,
    notify: Notify,
    sessions: Mutex<Vec<SessionHandle>>,
    session_opened_at: Mutex<Vec<Instant>>,
    /// Token the fake has issued and still accepts.
    token: Mutex<Option<String>>,
    killed_tokens: Mutex<Vec<String>>,
    /// LL code `checktoken`/`authwithtoken` refuse with, or 0 to accept.
    token_refusal: AtomicU16,
    /// LL code `authwithtoken` alone refuses with, or 0 to accept.
    authwithtoken_refusal: AtomicU16,
    /// LL code `refreshjwt` refuses with, or 0 to accept.
    refreshjwt_refusal: AtomicU16,
    /// LL code `checktoken` alone refuses with, or 0 to accept.
    checktoken_refusal: AtomicU16,
    pub(crate) poll_refusal: AtomicU16,
    pub(crate) poll_mismatch: AtomicBool,
    pub(crate) poll_payloads: Mutex<std::collections::HashMap<String, Vec<u8>>>,
    answer_keepalive: AtomicBool,
    tokens_issued: AtomicU64,
}

impl FakeState {
    fn new(cfg: FakeConfig) -> Arc<Self> {
        let reject = AtomicU16::new(if cfg.reject_token { 401 } else { 0 });
        let keepalive = AtomicBool::new(cfg.answer_keepalive);
        Arc::new(Self {
            cfg,
            enable_reply_gate: tokio::sync::Semaphore::new(1),
            log: Mutex::new(Vec::new()),
            notify: Notify::new(),
            sessions: Mutex::new(Vec::new()),
            session_opened_at: Mutex::new(Vec::new()),
            token: Mutex::new(None),
            killed_tokens: Mutex::new(Vec::new()),
            token_refusal: reject,
            authwithtoken_refusal: AtomicU16::new(0),
            refreshjwt_refusal: AtomicU16::new(0),
            checktoken_refusal: AtomicU16::new(0),
            poll_refusal: AtomicU16::new(0),
            poll_mismatch: AtomicBool::new(false),
            poll_payloads: Mutex::new(std::collections::HashMap::new()),
            answer_keepalive: keepalive,
            tokens_issued: AtomicU64::new(0),
        })
    }

    pub(crate) fn push(&self, entry: Entry) {
        self.log.lock().expect("log lock").push(entry);
        self.notify.notify_waiters();
    }

    pub fn log(&self) -> Vec<Entry> {
        self.log.lock().expect("log lock").clone()
    }
    pub fn set_poll_payload(&self, uuid: String, payload: Option<Vec<u8>>) {
        let mut map = self.poll_payloads.lock().unwrap();
        if let Some(payload) = payload {
            map.insert(uuid, payload);
        } else {
            map.remove(&uuid);
        }
    }
    pub fn set_poll_refusal(&self, code: u16) {
        self.poll_refusal.store(code, Ordering::Relaxed);
    }
    pub fn set_poll_mismatch(&self, enabled: bool) {
        self.poll_mismatch.store(enabled, Ordering::Relaxed);
    }
    pub fn pause_enable_reply(&self) {
        self.enable_reply_gate.try_acquire().unwrap().forget();
    }
    pub fn resume_enable_reply(&self) {
        self.enable_reply_gate.add_permits(1);
    }

    pub fn commands(&self) -> Vec<CommandRecord> {
        self.log()
            .iter()
            .filter_map(Entry::as_command)
            .cloned()
            .collect()
    }

    /// Commands of one session, in the order they arrived.
    pub fn session_commands(&self, session: usize) -> Vec<CommandRecord> {
        self.commands()
            .into_iter()
            .filter(|record| record.session == session)
            .collect()
    }

    /// How often a command verb was received across all sessions.
    pub fn count(&self, label: &str) -> usize {
        self.commands()
            .iter()
            .filter(|record| record.label == label)
            .count()
    }

    pub fn http_gets(&self) -> Vec<String> {
        self.log()
            .into_iter()
            .filter_map(|entry| match entry {
                Entry::HttpGet { path } => Some(path),
                _ => None,
            })
            .collect()
    }

    pub fn session_count(&self) -> usize {
        self.sessions.lock().expect("sessions lock").len()
    }

    pub fn session_opened_at(&self, index: usize) -> Option<Instant> {
        self.session_opened_at
            .lock()
            .expect("times lock")
            .get(index)
            .copied()
    }

    pub fn set_reject_token(&self, reject: bool) {
        self.set_token_refusal(if reject { 401 } else { 0 });
    }

    /// Refuse token validation with an arbitrary LL code; `0` accepts again.
    pub fn set_token_refusal(&self, code: u16) {
        self.token_refusal.store(code, Ordering::Relaxed);
    }

    /// Refuse `authwithtoken` only, leaving `checktoken` accepting; `0` accepts
    /// again.
    pub fn set_authwithtoken_refusal(&self, code: u16) {
        self.authwithtoken_refusal.store(code, Ordering::Relaxed);
    }

    /// Refuse to extend the token on `refreshjwt`; `0` accepts again.
    pub fn set_refreshjwt_refusal(&self, code: u16) {
        self.refreshjwt_refusal.store(code, Ordering::Relaxed);
    }

    /// Refuse `checktoken` only, leaving `authwithtoken` accepting — what real
    /// firmware does on a connection that has not authenticated yet. `0`
    /// accepts again.
    pub fn set_checktoken_refusal(&self, code: u16) {
        self.checktoken_refusal.store(code, Ordering::Relaxed);
    }

    pub fn set_answer_keepalive(&self, answer: bool) {
        self.answer_keepalive.store(answer, Ordering::Relaxed);
    }

    pub(crate) fn token_refusal(&self) -> Option<String> {
        match self.token_refusal.load(Ordering::Relaxed) {
            0 => None,
            code => Some(code.to_string()),
        }
    }

    pub(crate) fn authwithtoken_refusal(&self) -> Option<String> {
        match self.authwithtoken_refusal.load(Ordering::Relaxed) {
            0 => None,
            code => Some(code.to_string()),
        }
    }

    pub(crate) fn refreshjwt_refusal(&self) -> Option<String> {
        match self.refreshjwt_refusal.load(Ordering::Relaxed) {
            0 => None,
            code => Some(code.to_string()),
        }
    }

    pub(crate) fn checktoken_refusal(&self) -> Option<String> {
        match self.checktoken_refusal.load(Ordering::Relaxed) {
            0 => None,
            code => Some(code.to_string()),
        }
    }

    pub(crate) fn answers_keepalive(&self) -> bool {
        self.answer_keepalive.load(Ordering::Relaxed)
    }

    pub(crate) fn issue_token(&self) -> String {
        let n = self.tokens_issued.fetch_add(1, Ordering::Relaxed) + 1;
        // Shaped like a JWT so nothing downstream trips over the format.
        let token = format!("eyJhbGciOiJIUzI1NiJ9.fake-token-{n}.signature");
        *self.token.lock().expect("token lock") = Some(token.clone());
        token
    }

    pub(crate) fn current_token(&self) -> Option<String> {
        self.token.lock().expect("token lock").clone()
    }

    pub(crate) fn kill_token(&self, token: &str) {
        self.killed_tokens
            .lock()
            .expect("killed lock")
            .push(token.to_string());
    }

    pub fn killed_tokens(&self) -> Vec<String> {
        self.killed_tokens.lock().expect("killed lock").clone()
    }

    pub fn tokens_issued(&self) -> u64 {
        self.tokens_issued.load(Ordering::Relaxed)
    }

    /// Register a live session and return its index.
    pub(crate) fn register_session(&self, tx: mpsc::UnboundedSender<Ctrl>) -> usize {
        let mut sessions = self.sessions.lock().expect("sessions lock");
        let index = sessions.len();
        sessions.push(SessionHandle { index, tx });
        drop(sessions);
        self.session_opened_at
            .lock()
            .expect("times lock")
            .push(Instant::now());
        self.notify.notify_waiters();
        index
    }

    /// Wait until `pred` holds for the log, or fail after `secs`.
    pub async fn wait_until<F>(&self, secs: u64, what: &str, mut pred: F)
    where
        F: FnMut(&[Entry]) -> bool,
    {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            // Registered before the check so a notification that lands in
            // between cannot be lost.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let log = self.log();
            if pred(&log) {
                return;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                panic!("timed out waiting for {what}; log so far: {log:#?}");
            }
            if tokio::time::timeout(remaining, notified).await.is_err() {
                let log = self.log();
                if pred(&log) {
                    return;
                }
                panic!("timed out waiting for {what}; log so far: {log:#?}");
            }
        }
    }

    /// Wait for the `index`-th WebSocket session and return its push handle.
    pub async fn session(&self, index: usize) -> SessionHandle {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if let Some(handle) = self.sessions.lock().expect("sessions lock").get(index) {
                return handle.clone();
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                panic!("session {index} never opened");
            }
            let _ = tokio::time::timeout(remaining, notified).await;
        }
    }
}

/// A fake Miniserver bound to a loopback port.
#[derive(Debug)]
pub struct FakeMiniserver {
    addr: std::net::SocketAddr,
    pub state: Arc<FakeState>,
    accept: tokio::task::JoinHandle<()>,
}

impl FakeMiniserver {
    /// Bind to `127.0.0.1:0` and start accepting.
    pub async fn start(cfg: FakeConfig) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("listener address");
        let state = FakeState::new(cfg);

        let accept_state = Arc::clone(&state);
        let accept = tokio::spawn(async move {
            loop {
                let Ok((stream, _peer)) = listener.accept().await else {
                    return;
                };
                let _ = stream.set_nodelay(true);
                let conn_state = Arc::clone(&accept_state);
                tokio::spawn(server::handle_connection(stream, conn_state));
            }
        });

        Self {
            addr,
            state,
            accept,
        }
    }

    /// Fake with default behaviour.
    pub async fn start_default() -> Self {
        Self::start(FakeConfig::default()).await
    }

    /// Base URL to hand to [`ConnectConfig`].
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }
}

impl Drop for FakeMiniserver {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

/// Client configuration wired to `fake`, with test-scale timings.
///
/// Keepalive is effectively disabled; the liveness tests turn it back on.
pub fn test_config(fake: &FakeMiniserver) -> ConnectConfig {
    let mut cfg = ConnectConfig::new(fake.url(), TEST_USER, TEST_PASSWORD);
    cfg.connect_delay_secs = 0;
    cfg.long_backoff_secs = 2;
    cfg.command_timeout_secs = 5;
    cfg.read_idle_timeout_secs = 30;
    cfg.keepalive_secs = 3600;
    cfg
}

/// 8-byte Loxone message header (`WsBinHdr`).
pub fn lox_header(msg_type: u8, info: u8, len: u32) -> [u8; 8] {
    let mut header = [0u8; 8];
    header[0] = 0x03;
    header[1] = msg_type;
    header[2] = info;
    header[4..].copy_from_slice(&len.to_le_bytes());
    header
}

/// UUID whose every byte is `tag`, so assertions stay readable.
pub fn uuid(tag: u8) -> LoxoneUuid {
    LoxoneUuid::from_bytes([tag; 16])
}

/// Type-2 payload: 24 bytes per record.
pub fn value_payload(records: &[(LoxoneUuid, f64)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(records.len() * 24);
    for (uuid, value) in records {
        out.extend_from_slice(uuid.as_bytes());
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

/// Type-3 payload: `uuid | uuidIcon | textLength | text | pad to 4`.
pub fn text_payload(records: &[(LoxoneUuid, LoxoneUuid, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (uuid, icon, text) in records {
        out.extend_from_slice(uuid.as_bytes());
        out.extend_from_slice(icon.as_bytes());
        out.extend_from_slice(&(text.len() as u32).to_le_bytes());
        out.extend_from_slice(text);
        out.resize((out.len() + 3) & !3, 0);
    }
    out
}

/// Type-4 payload: table header plus the entry array, laid out by zerocopy.
pub fn daytimer_payload(tables: &[(LoxoneUuid, f64, Vec<DaytimerEntry>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (uuid, default_value, entries) in tables {
        out.extend_from_slice(uuid.as_bytes());
        out.extend_from_slice(&default_value.to_le_bytes());
        out.extend_from_slice(&(entries.len() as i32).to_le_bytes());
        out.extend_from_slice(entries.as_slice().as_bytes());
    }
    out
}

/// Type-7 payload: table header plus the entry array, laid out by zerocopy.
pub fn weather_payload(tables: &[(LoxoneUuid, u32, Vec<WeatherEntry>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (uuid, last_update, entries) in tables {
        out.extend_from_slice(uuid.as_bytes());
        out.extend_from_slice(&last_update.to_le_bytes());
        out.extend_from_slice(&(entries.len() as i32).to_le_bytes());
        out.extend_from_slice(entries.as_slice().as_bytes());
    }
    out
}

/// What the handler saw, flattened so tests can compare with `assert_eq!`.
#[derive(Debug, Clone, PartialEq)]
pub enum Rec {
    Value {
        uuid: LoxoneUuid,
        value: f64,
    },
    Text {
        uuid: LoxoneUuid,
        icon: LoxoneUuid,
        text: Vec<u8>,
    },
    Daytimer {
        uuid: LoxoneUuid,
        default_value: f64,
        /// `(mode, from, to, need_activate, value)` per entry.
        entries: Vec<(i32, i32, i32, i32, f64)>,
    },
    Weather {
        uuid: LoxoneUuid,
        last_update: u32,
        /// `(timestamp, weather_type, temperature)` per entry.
        entries: Vec<(i32, i32, f64)>,
    },
    Json(Vec<u8>),
    Binary(Vec<u8>),
    Keepalive,
    Connected,
    Reconnected,
    ConnectionClosed(Option<u16>),
    Closed,
}

/// Handler that forwards everything into an unbounded channel.
///
/// Unbounded on purpose: [`loxwebsocket::ChannelHandler`] drops events when its
/// queue fills, which would turn "did the reader deliver every record of a
/// 5000-entry table" into a flaky question.
#[derive(Debug)]
pub struct RecordingHandler {
    tx: mpsc::UnboundedSender<Rec>,
}

impl RecordingHandler {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<Rec>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }

    fn push(&self, rec: Rec) {
        let _ = self.tx.send(rec);
    }
}

impl LoxHandler for RecordingHandler {
    fn on_value(&mut self, uuid: &LoxoneUuid, value: f64) {
        self.push(Rec::Value { uuid: *uuid, value });
    }

    fn on_text(&mut self, uuid: &LoxoneUuid, icon: &LoxoneUuid, text: &[u8]) {
        self.push(Rec::Text {
            uuid: *uuid,
            icon: *icon,
            text: text.to_vec(),
        });
    }

    fn on_daytimer(&mut self, event: DaytimerEvent<'_>) {
        self.push(Rec::Daytimer {
            uuid: event.uuid,
            default_value: event.default_value,
            entries: event
                .entries
                .iter()
                .map(|entry| {
                    (
                        entry.mode(),
                        entry.from_minutes(),
                        entry.to_minutes(),
                        entry.need_activate(),
                        entry.value(),
                    )
                })
                .collect(),
        });
    }

    fn on_weather(&mut self, event: WeatherEvent<'_>) {
        self.push(Rec::Weather {
            uuid: event.uuid,
            last_update: event.last_update,
            entries: event
                .entries
                .iter()
                .map(|entry| (entry.timestamp(), entry.weather_type(), entry.temperature()))
                .collect(),
        });
    }

    fn on_json(&mut self, payload: &[u8]) {
        self.push(Rec::Json(payload.to_vec()));
    }

    fn on_binary(&mut self, payload: &[u8]) {
        self.push(Rec::Binary(payload.to_vec()));
    }

    fn on_keepalive(&mut self) {
        self.push(Rec::Keepalive);
    }

    fn on_event(&mut self, event: ClientEvent) {
        self.push(match event {
            ClientEvent::Connected => Rec::Connected,
            ClientEvent::Reconnected => Rec::Reconnected,
            ClientEvent::ConnectionClosed { close_code } => Rec::ConnectionClosed(close_code),
            ClientEvent::Closed => Rec::Closed,
        });
    }
}

/// Bound a future by `secs`, failing the test with `what` on expiry.
pub async fn within<F: Future>(secs: u64, what: &str, future: F) -> F::Output {
    match tokio::time::timeout(Duration::from_secs(secs), future).await {
        Ok(value) => value,
        Err(_) => panic!("timed out after {secs}s waiting for {what}"),
    }
}

/// Next handler record, or fail.
pub async fn next_rec(rx: &mut mpsc::UnboundedReceiver<Rec>, secs: u64) -> Rec {
    within(secs, "a handler record", rx.recv())
        .await
        .expect("handler channel closed")
}

/// Drain records until one matches, returning it.
pub async fn wait_rec<F>(rx: &mut mpsc::UnboundedReceiver<Rec>, secs: u64, mut pred: F) -> Rec
where
    F: FnMut(&Rec) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let rec = match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(rec)) => rec,
            Ok(None) => panic!("handler channel closed before a matching record"),
            Err(_) => panic!("no matching handler record within {secs}s"),
        };
        if pred(&rec) {
            return rec;
        }
    }
}

/// Collect exactly `count` records that match `pred`.
pub async fn collect_recs<F>(
    rx: &mut mpsc::UnboundedReceiver<Rec>,
    secs: u64,
    count: usize,
    mut pred: F,
) -> Vec<Rec>
where
    F: FnMut(&Rec) -> bool,
{
    let mut out = Vec::with_capacity(count);
    while out.len() < count {
        let rec = wait_rec(rx, secs, &mut pred).await;
        out.push(rec);
    }
    out
}

/// Opt-in tracing for debugging a failing test (`RUST_LOG=loxwebsocket=debug`).
pub fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}
