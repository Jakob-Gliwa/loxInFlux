use crate::{
    archive,
    config::Config,
    controls::{self, Controls},
};
use anyhow::{Context, Result, bail, ensure};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    io::{BufWriter, Write},
    path::PathBuf,
    sync::LazyLock,
    time::Duration,
};

static CONFIG_FILE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"sps_(\d+)_(\d+)\.(?:zip|LoxCC)").unwrap());
pub fn newest_file(listing: &str) -> Option<&str> {
    CONFIG_FILE
        .captures_iter(listing)
        .filter_map(|c| {
            Some((
                c.get(0)?.as_str(),
                (
                    c.get(2)?.as_str().parse::<u64>().ok()?,
                    c.get(1)?.as_str().parse::<u64>().ok()?,
                ),
            ))
        })
        .max_by_key(|(_, key)| *key)
        .map(|(name, _)| name)
}
#[derive(Serialize, Deserialize)]
struct Cache {
    origin: String,
    username: String,
    xml: String,
    app: serde_json::Value,
}
pub struct Loader {
    client: reqwest::Client,
    config: Config,
    version: Option<String>,
}
impl Loader {
    pub fn new(config: &Config) -> Result<Self> {
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(!config.miniserver.tls_verify)
            .timeout(Duration::from_secs(config.miniserver.request_timeout))
            // Match the legacy HTTP loader; reqwest strips Authorization when
            // a redirect changes host/port. Bound loops instead of treating
            // a redirect's empty body as a missing configuration.
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()?;
        Ok(Self {
            client,
            config: config.clone(),
            version: None,
        })
    }
    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        let ms = &self.config.miniserver;
        let mut response = self
            .client
            .get(format!("{}/{path}", ms.base_url()))
            .basic_auth(&ms.user, Some(&ms.password))
            .send()
            .await
            .with_context(|| format!("Miniserver HTTP /{path}"))?
            .error_for_status()?;
        ensure!(
            response.content_length().unwrap_or(0) <= archive::MAX_CONFIG_BYTES as u64,
            "HTTP configuration exceeds size limit"
        );
        let mut data = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                data.len() + chunk.len() <= archive::MAX_CONFIG_BYTES,
                "HTTP configuration exceeds size limit"
            );
            data.extend_from_slice(&chunk);
        }
        check_response(&data).with_context(|| format!("Miniserver HTTP /{path}"))?;
        Ok(data)
    }
    async fn remote_version(&self) -> Result<String> {
        let data = self.get("jdev/sps/LoxAPPversion3").await?;
        let json: serde_json::Value = serde_json::from_slice(&data)?;
        let ll = json.get("LL").context("Missing LL version response")?;
        check_status(ll)?;
        ll["value"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .context("Missing configuration version")
    }
    fn cache_path(&self) -> PathBuf {
        PathBuf::from(&self.config.paths.data_dir).join("configuration-cache.json")
    }
    pub async fn load(&mut self) -> Result<Option<Controls>> {
        let remote = self.remote_version().await.ok();
        if remote.is_some() && remote == self.version {
            return Ok(None);
        }
        if self.version.is_none()
            && let Some(version) = &remote
            && let Ok(bytes) = tokio::fs::read(self.cache_path()).await
            && let Ok(cache) = serde_json::from_slice::<Cache>(&bytes)
            && cache.origin == self.config.miniserver.base_url()
            && cache.username == self.config.miniserver.user
            && cache.app["lastModified"].as_str() == Some(version.as_str())
        {
            match controls::parse(cache.xml.as_bytes(), &self.config) {
                Ok(controls) => {
                    self.version = remote;
                    return Ok(Some(controls));
                }
                Err(error) => {
                    tracing::warn!(%error, "Ignoring invalid configuration cache; downloading again")
                }
            }
        }
        let listing = String::from_utf8(self.get("dev/fslist/prog/").await?)?;
        let filename = newest_file(&listing)
            .context("No sps configuration in /prog; check full-access permissions")?;
        let raw = self.get(&format!("dev/fsget/prog/{filename}")).await?;
        let (xml, app) = tokio::task::spawn_blocking(move || archive::decode(&raw)).await??;
        let app = match app {
            Some(app) => app,
            None => serde_json::from_slice(&self.get("data/LoxAPP3.json").await?)?,
        };
        let version = app["lastModified"]
            .as_str()
            .context("LoxAPP3.json lacks lastModified; check access rights")?
            .to_owned();
        let config = self.config.clone();
        let (controls, xml) = tokio::task::spawn_blocking(move || -> Result<_> {
            // Preserve the parser's lossy UTF-8 recovery through persistence.
            // Otherwise a stray byte accepted by the scanner still aborts startup.
            let xml = String::from_utf8(xml)
                .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
            let controls = controls::parse(xml.as_bytes(), &config)?;
            Ok((controls, xml))
        })
        .await??;
        let cache = Cache {
            origin: self.config.miniserver.base_url(),
            username: self.config.miniserver.user.clone(),
            xml,
            app,
        };
        tokio::fs::create_dir_all(&self.config.paths.data_dir).await?;
        // A single atomic snapshot prevents pairing an old XML with a new JSON
        // if the process exits during persistence.
        let path = self.cache_path();
        let temp = path.with_extension("json.tmp");
        // Stream on the blocking pool: avoid a second complete escaped XML/JSON
        // buffer and do not stall the WebSocket reader during a config refresh.
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut out = BufWriter::with_capacity(64 * 1024, std::fs::File::create(&temp)?);
            serde_json::to_writer(&mut out, &cache)?;
            out.flush()?;
            drop(out);
            std::fs::rename(temp, path)?;
            Ok(())
        })
        .await??;
        self.version = Some(version);
        Ok(Some(controls))
    }
}

/// Filesystem endpoints can return HTTP 200 with either a JSON or XML LL error.
/// Check before treating the body as a listing, archive or LoxAPP3 document;
/// do not include response values (which may be private) in the diagnostic.
fn check_response(data: &[u8]) -> Result<()> {
    match data.iter().copied().find(|b| !b.is_ascii_whitespace()) {
        Some(b'{') => {
            if let Ok(document) = serde_json::from_slice::<serde_json::Value>(data)
                && let Some(ll) = document.get("LL")
            {
                check_status(ll)?;
            }
        }
        Some(b'<') => {
            let mut reader = quick_xml::Reader::from_reader(data);
            loop {
                match reader.read_event() {
                    Ok(quick_xml::events::Event::Start(e) | quick_xml::events::Event::Empty(e)) => {
                        if e.name().as_ref() == b"LL" {
                            for attr in e.attributes().flatten() {
                                if matches!(attr.key.as_ref(), b"Code" | b"code") {
                                    let code = attr.decoded_and_normalized_value(
                                        quick_xml::XmlVersion::Implicit1_0,
                                        reader.decoder(),
                                    )?;
                                    ensure!(
                                        code == "200",
                                        "Miniserver LL status {code}; check access rights"
                                    );
                                }
                            }
                        }
                        break;
                    }
                    Ok(quick_xml::events::Event::Eof) | Err(_) => break,
                    _ => {}
                }
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn check_status(ll: &serde_json::Value) -> Result<()> {
    check_code(ll.get("Code").or_else(|| ll.get("code")))
}
pub(crate) fn check_code(code: Option<&serde_json::Value>) -> Result<()> {
    match code {
        Some(serde_json::Value::String(s)) if s == "200" => Ok(()),
        Some(serde_json::Value::Number(n)) if n.as_u64() == Some(200) => Ok(()),
        Some(c) => bail!("Miniserver LL status {c}"),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selects_timestamp_before_version_with_numeric_order() {
        assert_eq!(
            newest_file("sps_9_9.zip sps_10_100.zip sps_10_101.LoxCC"),
            Some("sps_10_101.LoxCC")
        );
        assert_eq!(
            newest_file("sps_999_100.zip sps_2_101.zip"),
            Some("sps_2_101.zip")
        );
        assert_eq!(newest_file("LL:403"), None);
    }
    #[test]
    fn http_200_does_not_hide_ll_errors() {
        for code in [serde_json::json!("403"), serde_json::json!(403)] {
            assert!(check_status(&serde_json::json!({"Code":code})).is_err());
        }
    }
}
