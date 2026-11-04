//! Connection handling for the fake Miniserver: HTTP `GET`s, the WebSocket
//! upgrade, and one authenticated session per upgraded connection.

use super::CommandRecord;
use super::ws::{self, ClientFrame};
use super::{
    ApiValueStyle, Ctrl, Entry, FakeConfig, FakeState, HashAlgName, PublicKeyStyle, TEST_PASSWORD,
    TEST_USER, TEST_VISU_PASSWORD, early_table_records, lox_header, value_payload,
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, KeyInit, Mac};
use rand::RngCore;
use rsa::pkcs8::DecodePrivateKey;
use rsa::{Pkcs1v15Encrypt, RsaPrivateKey, RsaPublicKey};
use sha1::Sha1;
use sha2::Sha256;
use std::io;
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

const WS_PATH: &str = "/ws/rfc6455";

/// Parsed once per test binary; PKCS#8 parsing is cheap, key generation is not.
static PRIVATE_KEY: LazyLock<RsaPrivateKey> = LazyLock::new(|| {
    RsaPrivateKey::from_pkcs8_pem(super::TEST_RSA_PRIVATE_KEY_PEM).expect("parse test RSA key")
});

/// Correct multi-line SPKI PEM of [`PRIVATE_KEY`].
static PUBLIC_KEY_PEM: LazyLock<String> = LazyLock::new(|| {
    use rsa::pkcs8::EncodePublicKey;
    RsaPublicKey::from(&*PRIVATE_KEY)
        .to_public_key_pem(rsa::pkcs8::LineEnding::LF)
        .expect("encode test SPKI")
});

/// The same SPKI the way a Miniserver sends it: mislabeled `CERTIFICATE`, on one
/// line, no wrapping.
static PUBLIC_KEY_ONE_LINE: LazyLock<String> = LazyLock::new(|| {
    let body: String = PUBLIC_KEY_PEM
        .lines()
        .filter(|line| !line.starts_with('-'))
        .collect();
    format!("-----BEGIN CERTIFICATE-----{body}-----END CERTIFICATE-----")
});

pub(crate) async fn handle_connection(mut stream: TcpStream, state: Arc<FakeState>) {
    let Ok(Some((head, leftover))) = read_head(&mut stream).await else {
        return;
    };
    let head = String::from_utf8_lossy(&head).into_owned();
    let Some(target) = request_target(&head) else {
        return;
    };

    if target == WS_PATH {
        let Some(key) = header_value(&head, "sec-websocket-key") else {
            let _ = write_http(&mut stream, 400, "missing websocket key").await;
            return;
        };
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {}\r\n\
             Sec-WebSocket-Protocol: remotecontrol\r\n\
             \r\n",
            ws::accept_key(key)
        );
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }
        let _ = stream.flush().await;
        run_session(stream, state, leftover).await;
        return;
    }

    let path = target.trim_start_matches('/').to_string();
    state.push(Entry::HttpGet { path: path.clone() });
    if let Some(body) = state.cfg.files.get(&path) {
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes()).await;
        let _ = stream.write_all(body).await;
        let _ = stream.shutdown().await;
        return;
    }
    let (status, body) = http_response(&state.cfg, &path);
    let _ = write_http(&mut stream, status, &body).await;
    let _ = stream.shutdown().await;
}

// ---------------------------------------------------------------- HTTP layer

/// Read the request head, returning it plus whatever arrived behind it.
async fn read_head(stream: &mut TcpStream) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
    let mut buf = Vec::with_capacity(1024);
    loop {
        if let Some(end) = find_head_end(&buf) {
            let leftover = buf.split_off(end);
            return Ok(Some((buf, leftover)));
        }
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..read]);
        if buf.len() > 64 * 1024 {
            return Ok(None);
        }
    }
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|at| at + 4)
}

fn request_target(head: &str) -> Option<&str> {
    head.lines().next()?.split_whitespace().nth(1)
}

fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

async fn write_http(stream: &mut TcpStream, status: u16, body: &str) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await
}

fn http_response(cfg: &FakeConfig, path: &str) -> (u16, String) {
    match path {
        "jdev/sys/getPublicKey" => (200, public_key_body(cfg)),
        "jdev/cfg/apiKey" if cfg.serve_api_key => (200, api_body(cfg, "dev/cfg/apiKey")),
        "jdev/cfg/api" if cfg.serve_api => (200, api_body(cfg, "dev/cfg/api")),
        _ => (404, r#"{"LL":{"Code":"404"}}"#.to_string()),
    }
}

fn public_key_body(cfg: &FakeConfig) -> String {
    let pem = match cfg.public_key_style {
        PublicKeyStyle::Spki => PUBLIC_KEY_PEM.as_str(),
        PublicKeyStyle::CertificateOneLine => PUBLIC_KEY_ONE_LINE.as_str(),
    };
    format!(
        r#"{{"LL":{{"control":"dev/sys/getPublicKey","value":"{}","Code":"200"}}}}"#,
        json_escape(pem)
    )
}

/// A field of the reachability response, rendered per firmware style.
enum Field {
    Str(String),
    Num(u64),
    Bool(bool),
}

fn api_body(cfg: &FakeConfig, control: &str) -> String {
    let mut fields: Vec<(&str, Field)> = Vec::new();
    if let Some(snr) = &cfg.snr {
        fields.push(("snr", Field::Str(snr.clone())));
    }
    if let Some(version) = &cfg.version {
        fields.push(("version", Field::Str(version.clone())));
    }
    if control.ends_with("apiKey") {
        if let Some(status) = cfg.https_status {
            fields.push(("httpsStatus", Field::Num(u64::from(status))));
        }
        if let Some(local) = cfg.local {
            fields.push(("local", Field::Bool(local)));
        }
    } else if let Some(slots) = cfg.has_event_slots {
        // The document documents `hasEventSlots` on `jdev/cfg/api` only.
        fields.push(("hasEventSlots", Field::Bool(slots)));
    }

    let value = match cfg.api_value_style {
        ApiValueStyle::Object => render_object(&fields, '"'),
        ApiValueStyle::SingleQuotedString => {
            format!("\"{}\"", render_object(&fields, '\''))
        }
    };
    format!(r#"{{"LL":{{"control":"{control}","value":{value},"Code":"200"}}}}"#)
}

fn render_object(fields: &[(&str, Field)], quote: char) -> String {
    let mut out = String::from("{");
    for (index, (key, field)) in fields.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push(quote);
        out.push_str(key);
        out.push(quote);
        out.push(':');
        match field {
            Field::Str(value) => {
                out.push(quote);
                out.push_str(value);
                out.push(quote);
            }
            Field::Num(value) => out.push_str(&value.to_string()),
            Field::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        }
    }
    out.push('}');
    out
}

fn json_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 16);
    for ch in raw.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

// ----------------------------------------------------------- session handling

/// Per-connection protocol state.
struct Session {
    index: usize,
    enabling: bool,
    state: Arc<FakeState>,
    /// AES session installed by `jdev/sys/keyexchange`.
    aes: Option<([u8; 32], [u8; 16])>,
    /// Key returned by `jdev/sys/getkey`, reused for every token hash.
    getkey_hex: String,
    /// Key + salt returned by `jdev/sys/getkey2`.
    getkey2_hex: String,
    user_salt: String,
    /// Key + salt returned by `jdev/sys/getvisusalt`.
    visu_key_hex: String,
    visu_salt: String,
}

async fn run_session(mut stream: TcpStream, state: Arc<FakeState>, leftover: Vec<u8>) {
    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel();
    let index = state.register_session(ctrl_tx);
    state.push(Entry::SessionOpened { session: index });

    let mut session = Session {
        index,
        enabling: false,
        state: Arc::clone(&state),
        aes: None,
        getkey_hex: random_hex(32),
        getkey2_hex: random_hex(32),
        user_salt: random_hex(8),
        visu_key_hex: random_hex(32),
        visu_salt: random_hex(8),
    };

    let mut buf = leftover;
    enum Step {
        Frame(Option<ClientFrame>),
        Ctrl(Option<Ctrl>),
    }

    loop {
        // Every arm only decides what to do; the writes happen below, outside
        // the `select!`, and the frame reader keeps its buffer in `buf`.
        let step = tokio::select! {
            biased;
            ctrl = ctrl_rx.recv() => Step::Ctrl(ctrl),
            frame = ws::read_client_frame(&mut stream, &mut buf) => {
                Step::Frame(frame.unwrap_or(None))
            }
        };

        match step {
            Step::Ctrl(None) | Step::Frame(None) => break,
            Step::Ctrl(Some(Ctrl::Abort)) => break,
            Step::Ctrl(Some(Ctrl::Sync(done))) => {
                let _ = done.send(());
            }
            Step::Ctrl(Some(Ctrl::Write { blobs, gap })) => {
                if ws::write_blobs(&mut stream, &blobs, gap).await.is_err() {
                    break;
                }
            }
            Step::Ctrl(Some(Ctrl::Sleep(duration))) => tokio::time::sleep(duration).await,
            Step::Frame(Some(frame)) => match frame.opcode {
                ws::OP_TEXT => {
                    let blobs = session.on_text(&frame.text());
                    if session.enabling {
                        let _permit = state.enable_reply_gate.acquire().await.unwrap();
                    }
                    if ws::write_blobs(&mut stream, &blobs, Duration::ZERO)
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                ws::OP_CLOSE => {
                    state.push(Entry::ClientClose {
                        session: index,
                        code: frame.close_code(),
                    });
                    let _ = ws::write_blobs(&mut stream, &[ws::close_frame(1000)], Duration::ZERO)
                        .await;
                    break;
                }
                ws::OP_PING => {
                    let pong = ws::frame(ws::OP_PONG, &frame.payload);
                    if ws::write_blobs(&mut stream, &[pong], Duration::ZERO)
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                _ => {}
            },
        }
    }

    state.push(Entry::SessionClosed { session: index });
}

impl Session {
    /// Turn one incoming text frame into the frames that answer it.
    fn on_text(&mut self, wire: &str) -> Vec<Vec<u8>> {
        if wire == "keepalive" {
            self.state.push(Entry::Keepalive {
                session: self.index,
            });
            if !self.state.answers_keepalive() {
                return Vec::new();
            }
            return vec![ws::frame(
                ws::OP_BINARY,
                &lox_header(loxwebsocket::MessageType::Keepalive as u8, 0, 0),
            )];
        }

        if let Some(encoded) = wire.strip_prefix("jdev/sys/keyexchange/") {
            return self.on_key_exchange(wire, encoded);
        }

        let (salted, control) = match wire.strip_prefix("jdev/sys/enc/") {
            Some(blob) => match self.decrypt_command(blob) {
                Some(plain) => (plain, wire.to_string()),
                None => {
                    return ll_message(wire, "\"\"", "400");
                }
            },
            None => (wire.to_string(), wire.to_string()),
        };

        let cmd = strip_salt(salted.trim_end_matches('\0')).to_string();
        self.enabling = cmd == "jdev/sps/enablebinstatusupdate";
        let (value, code) = self.dispatch(&cmd);
        self.state.push(Entry::Command(CommandRecord {
            session: self.index,
            label: cmd_label(&cmd),
            salted: salted.trim_end_matches('\0').to_string(),
            cmd: cmd.clone(),
            code: code.clone(),
        }));

        if cmd == "jdev/sps/enablebinstatusupdate" && self.state.cfg.bare_enable_updates_reply {
            // Some firmwares answer with the bare update counter instead of an
            // LL envelope; the client has to accept that too.
            return text_message(b"1");
        }
        if cmd == "jdev/sps/enablebinstatusupdate" && self.state.cfg.tables_before_enable_ack {
            let mut frames = value_message(&early_table_records());
            frames.extend(ll_message(&control, &value, &code));
            return frames;
        }
        if cmd.starts_with("jdev/sps/io")
            && cmd.ends_with("/all")
            && let Some(uuid) = cmd.rsplit('/').nth(1)
            && let Some(raw) = self.state.poll_payloads.lock().unwrap().get(uuid)
        {
            return text_message(raw);
        }
        if cmd.starts_with("jdev/sps/io")
            && cmd.ends_with("/all")
            && self.state.poll_mismatch.load(Ordering::Relaxed)
        {
            return ll_message("jdev/sps/io/unrelated-control/all", "999.25", "200");
        }
        ll_message(&control, &value, &code)
    }

    fn on_key_exchange(&mut self, wire: &str, encoded: &str) -> Vec<Vec<u8>> {
        // A Miniserver on its way back up accepts the socket before it hands out
        // session keys, so the refusal lands here — before any token is in play.
        if let Some(code) = self.state.cfg.keyexchange_refusal {
            return ll_message(wire, "\"\"", &code.to_string());
        }
        // The Base64 here is deliberately *not* URI-encoded, so a client that
        // percent-encodes it must fail loudly rather than silently.
        let Ok(ciphertext) = B64.decode(encoded) else {
            return ll_message(wire, "\"\"", "400");
        };
        let Ok(plain) = PRIVATE_KEY.decrypt(Pkcs1v15Encrypt, &ciphertext) else {
            return ll_message(wire, "\"\"", "400");
        };
        let Ok(payload) = String::from_utf8(plain) else {
            return ll_message(wire, "\"\"", "400");
        };
        let Some((key_hex, iv_hex)) = payload.split_once(':') else {
            return ll_message(wire, "\"\"", "400");
        };
        let (Ok(key), Ok(iv)) = (hex::decode(key_hex), hex::decode(iv_hex)) else {
            return ll_message(wire, "\"\"", "400");
        };
        if key.len() != 32 || iv.len() != 16 {
            return ll_message(wire, "\"\"", "400");
        }
        let mut key_bytes = [0u8; 32];
        key_bytes.copy_from_slice(&key);
        let mut iv_bytes = [0u8; 16];
        iv_bytes.copy_from_slice(&iv);
        self.aes = Some((key_bytes, iv_bytes));
        self.state.push(Entry::KeyExchange {
            session: self.index,
        });
        ll_message("jdev/sys/keyexchange", "\"\"", "200")
    }

    fn decrypt_command(&self, blob: &str) -> Option<String> {
        let (key, iv) = self.aes?;
        let decoded = percent_encoding::percent_decode_str(blob)
            .decode_utf8()
            .ok()?;
        let ciphertext = B64.decode(decoded.as_ref()).ok()?;
        let plain = aes_cbc_decrypt(&key, &iv, &ciphertext)?;
        String::from_utf8(plain).ok()
    }

    /// Execute one plaintext command, returning `(LL.value JSON, LL.Code)`.
    fn dispatch(&mut self, cmd: &str) -> (String, String) {
        let cfg = &self.state.cfg;
        let alg = cfg.hash_alg;
        let valid_until = cfg.token_valid_until;

        if cmd == "jdev/sys/getkey" {
            return (json_string(&self.getkey_hex), "200".into());
        }
        if let Some(user) = cmd.strip_prefix("jdev/sys/getkey2/") {
            if user != TEST_USER {
                return (json_string(""), "401".into());
            }
            return (
                format!(
                    r#"{{"key":"{}","salt":"{}","hashAlg":"{}"}}"#,
                    self.getkey2_hex,
                    self.user_salt,
                    alg.as_str()
                ),
                "200".into(),
            );
        }
        if let Some(rest) = cmd.strip_prefix("jdev/sys/getjwt/") {
            let parts: Vec<&str> = rest.split('/').collect();
            if parts.len() < 5 {
                return (json_string(""), "400".into());
            }
            let expected = self.expected_credential_hash();
            if parts[0] != expected || parts[1] != TEST_USER {
                return (json_string(""), "401".into());
            }
            let token = self.state.issue_token();
            return (
                format!(
                    r#"{{"token":"{token}","key":"{}","validUntil":{valid_until},"tokenRights":{},"unsecurePass":false}}"#,
                    self.getkey_hex, parts[2]
                ),
                "200".into(),
            );
        }
        if let Some(rest) = cmd.strip_prefix("authwithtoken/") {
            // Refused here but not on `checktoken`: the token can die in the
            // window between the two commands.
            if let Some(code) = self.state.authwithtoken_refusal() {
                return (json_string(""), code);
            }
            return match self.check_token_hash(rest) {
                Ok(()) => (
                    format!(
                        r#"{{"validUntil":{valid_until},"tokenRights":4,"unsecurePass":false}}"#
                    ),
                    "200".into(),
                ),
                Err(code) => (json_string(""), code),
            };
        }
        if let Some(rest) = cmd.strip_prefix("jdev/sys/checktoken/") {
            // Real firmware answers `400 Bad request` here on a connection that
            // has not authenticated yet, whatever the token is worth.
            if let Some(code) = self.state.checktoken_refusal() {
                return (json_string("Bad request"), code);
            }
            return match self.check_token_hash(rest) {
                Ok(()) => (
                    format!(r#"{{"validUntil":{valid_until},"unsecurePass":false}}"#),
                    "200".into(),
                ),
                Err(code) => (json_string(""), code),
            };
        }
        if let Some(rest) = cmd.strip_prefix("jdev/sys/refreshjwt/") {
            // A Miniserver that will not extend the token, while the socket
            // itself stays perfectly healthy.
            if let Some(code) = self.state.refreshjwt_refusal() {
                return (json_string(""), code);
            }
            return match self.check_token_hash(rest) {
                Ok(()) => {
                    let token = self.state.issue_token();
                    (
                        format!(
                            r#"{{"token":"{token}","validUntil":{},"unsecurePass":false}}"#,
                            valid_until + 3600
                        ),
                        "200".into(),
                    )
                }
                Err(code) => (json_string(""), code),
            };
        }
        if let Some(rest) = cmd.strip_prefix("jdev/sys/killtoken/") {
            return match self.check_token_hash(rest) {
                Ok(()) => {
                    if let Some(token) = self.state.current_token() {
                        self.state.kill_token(&token);
                    }
                    (json_string(""), "200".into())
                }
                Err(code) => (json_string(""), code),
            };
        }
        if let Some(user) = cmd.strip_prefix("jdev/sys/getvisusalt/") {
            if user != TEST_USER {
                return (json_string(""), "401".into());
            }
            return (
                format!(
                    r#"{{"key":"{}","salt":"{}","hashAlg":"{}"}}"#,
                    self.visu_key_hex,
                    self.visu_salt,
                    alg.as_str()
                ),
                "200".into(),
            );
        }
        if let Some(rest) = cmd.strip_prefix("jdev/sps/ios/") {
            let Some((hash, _)) = rest.split_once('/') else {
                return (json_string(""), "400".into());
            };
            if hash != self.expected_visu_hash() {
                // The document: a wrong visualization password answers 500.
                return (json_string(""), "500".into());
            }
            if cmd.ends_with("/all")
                && let Some(value) = &self.state.cfg.poll_value
            {
                let refusal = self.state.poll_refusal.load(Ordering::Relaxed);
                return (
                    json_string(value),
                    if refusal == 0 {
                        "200".into()
                    } else {
                        refusal.to_string()
                    },
                );
            }
            return (json_string(cmd), "200".into());
        }
        if cmd == "jdev/sps/enablebinstatusupdate" {
            return (json_string("1"), "200".into());
        }

        // Everything else echoes the command back as its value, which is what
        // the correlation tests key on to tell answers apart.
        if cmd.starts_with("jdev/sps/io/")
            && cmd.ends_with("/all")
            && let Some(value) = &self.state.cfg.poll_value
        {
            let refusal = self.state.poll_refusal.load(Ordering::Relaxed);
            return (
                json_string(value),
                if refusal == 0 {
                    "200".into()
                } else {
                    refusal.to_string()
                },
            );
        }
        (json_string(cmd), "200".into())
    }

    /// `HMAC(getkey2Key, "{user}:{pwHash}")`, computed independently of the crate.
    fn expected_credential_hash(&self) -> String {
        let alg = self.state.cfg.hash_alg;
        let pw_hash = digest_upper_hex(
            alg,
            format!("{TEST_PASSWORD}:{}", self.user_salt).as_bytes(),
        );
        let key = hex::decode(&self.getkey2_hex).expect("hex key");
        hmac_hex(alg, &key, format!("{TEST_USER}:{pw_hash}").as_bytes())
    }

    /// `HMAC(visuKey, upper(hash("{visuPw}:{visuSalt}")))`, without a user prefix.
    fn expected_visu_hash(&self) -> String {
        let alg = self.state.cfg.hash_alg;
        let pw_hash = digest_upper_hex(
            alg,
            format!("{TEST_VISU_PASSWORD}:{}", self.visu_salt).as_bytes(),
        );
        let key = hex::decode(&self.visu_key_hex).expect("hex key");
        hmac_hex(alg, &key, pw_hash.as_bytes())
    }

    /// Validate `{tokenHash}/{user}` against the token the fake handed out.
    fn check_token_hash(&self, rest: &str) -> Result<(), String> {
        if let Some(code) = self.state.token_refusal() {
            return Err(code);
        }
        let Some((hash, user)) = rest.split_once('/') else {
            return Err("400".into());
        };
        if user != TEST_USER {
            return Err("401".into());
        }
        let Some(token) = self.state.current_token() else {
            return Err("401".into());
        };
        let alg = self.state.cfg.hash_alg;
        let key = hex::decode(&self.getkey_hex).expect("hex key");
        if hmac_hex(alg, &key, token.as_bytes()) != hash {
            return Err("401".into());
        }
        Ok(())
    }
}

// ------------------------------------------------------------------- helpers

/// Loxone message with a JSON LL envelope: header frame plus text frame.
fn ll_message(control: &str, value_json: &str, code: &str) -> Vec<Vec<u8>> {
    let body =
        format!(r#"{{"LL":{{"control":"{control}","value":{value_json},"Code":"{code}"}}}}"#);
    text_message(body.as_bytes())
}

/// Type-2 event table: the 8-byte header frame, then the records.
fn value_message(records: &[(loxwebsocket::LoxoneUuid, f64)]) -> Vec<Vec<u8>> {
    let payload = value_payload(records);
    vec![
        ws::frame(
            ws::OP_BINARY,
            &lox_header(
                loxwebsocket::MessageType::ValueStates as u8,
                0,
                payload.len() as u32,
            ),
        ),
        ws::frame(ws::OP_BINARY, &payload),
    ]
}

/// Type-0 message: the 8-byte header frame, then the payload as a text frame.
fn text_message(payload: &[u8]) -> Vec<Vec<u8>> {
    vec![
        ws::frame(
            ws::OP_BINARY,
            &lox_header(
                loxwebsocket::MessageType::Text as u8,
                0,
                payload.len() as u32,
            ),
        ),
        ws::frame(ws::OP_TEXT, payload),
    ]
}

fn json_string(value: &str) -> String {
    format!("\"{}\"", json_escape(value))
}

fn cmd_label(cmd: &str) -> String {
    let segments = if cmd.starts_with("jdev/") || cmd.starts_with("dev/") {
        3
    } else {
        1
    };
    cmd.split('/').take(segments).collect::<Vec<_>>().join("/")
}

fn random_hex(bytes: usize) -> String {
    let mut raw = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut raw);
    hex::encode(raw)
}

/// Strip `salt/{salt}/` or `nextSalt/{prev}/{next}/` from a decrypted command.
///
/// Also written independently of the crate: a client that got the prefix wrong
/// has to end up with a command the fake does not recognize.
fn strip_salt(plain: &str) -> &str {
    if let Some(rest) = plain.strip_prefix("nextSalt/") {
        let mut parts = rest.splitn(3, '/');
        parts.next();
        parts.next();
        return parts.next().unwrap_or("");
    }
    if let Some(rest) = plain.strip_prefix("salt/") {
        return rest.split_once('/').map_or("", |(_, cmd)| cmd);
    }
    plain
}

/// AES-256-CBC with Loxone's ZeroBytePadding, implemented here rather than
/// borrowed from the crate: the fake has to be able to disagree with the code
/// under test about what a command decrypts to.
fn aes_cbc_decrypt(key: &[u8; 32], iv: &[u8; 16], ciphertext: &[u8]) -> Option<Vec<u8>> {
    use aes::cipher::KeyIvInit;
    use aes::cipher::block::BlockModeDecrypt;
    use aes::cipher::block_padding::NoPadding;

    if ciphertext.is_empty() || !ciphertext.len().is_multiple_of(16) {
        return None;
    }
    let mut buf = ciphertext.to_vec();
    cbc::Decryptor::<aes::Aes256>::new(key.into(), iv.into())
        .decrypt_padded::<NoPadding>(&mut buf)
        .ok()?;
    // The padding is whatever it takes to fill the last block with zeros; the
    // plaintext is a command string, so trailing NULs are never significant.
    while buf.last() == Some(&0) {
        buf.pop();
    }
    Some(buf)
}

fn digest_upper_hex(alg: HashAlgName, data: &[u8]) -> String {
    match alg {
        HashAlgName::Sha1 => {
            use sha1::Digest;
            hex::encode_upper(Sha1::digest(data))
        }
        HashAlgName::Sha256 => {
            use sha2::Digest;
            hex::encode_upper(Sha256::digest(data))
        }
    }
}

fn hmac_hex(alg: HashAlgName, key: &[u8], data: &[u8]) -> String {
    match alg {
        HashAlgName::Sha1 => {
            let mut mac = Hmac::<Sha1>::new_from_slice(key).expect("hmac key");
            mac.update(data);
            hex::encode(mac.finalize().into_bytes())
        }
        HashAlgName::Sha256 => {
            let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac key");
            mac.update(data);
            hex::encode(mac.finalize().into_bytes())
        }
    }
}
