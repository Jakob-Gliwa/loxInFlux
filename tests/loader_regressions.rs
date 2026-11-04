//! HTTP regressions from loxInFlux #1/#3/#8 and commits 5bbf642/613de19/2e1c1ff.
use axum::{
    Router,
    body::Body,
    http::{Request, Response, StatusCode},
};
use loxtotelegraf::{archive, config::Config, miniserver::Loader};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

const VERSION: &str = "/jdev/sps/LoxAPPversion3";
const LIST: &str = "/dev/fslist/prog/";
const FILE: &str = "/dev/fsget/prog/sps_0272_20260727223721.LoxCC";
const APP: &str = "/data/LoxAPP3.json";
const XML: &str =
    r#"<Root><C Type="Sensor" U="17a05a15-0243-d451-ffff68510c67e724" Title="Test"/></Root>"#;

#[derive(Clone)]
enum Reply {
    Body(Vec<u8>),
    Redirect(String),
}
fn routes() -> HashMap<String, Reply> {
    routes_with_xml(XML.as_bytes())
}
fn routes_with_xml(xml: &[u8]) -> HashMap<String, Reply> {
    let body = lz4_flex::block::compress(xml);
    let mut container = Vec::new();
    for n in [
        0xaabbcceeu32,
        body.len() as u32,
        xml.len() as u32,
        crc32fast::hash(xml),
    ] {
        container.extend(n.to_le_bytes());
    }
    container.extend(body);
    assert_eq!(archive::decode(&container).unwrap().0, xml);
    [
        (
            VERSION,
            br#"{"LL":{"Code":"200","value":"2026-07-27 22:37:21"}}"#.to_vec(),
        ),
        (
            LIST,
            b"sps_9999_20260727223718.zip\nsps_0272_20260727223721.LoxCC".to_vec(),
        ),
        (FILE, container),
        (
            APP,
            br#"{"lastModified":"2026-07-27 22:37:21","controls":{}}"#.to_vec(),
        ),
    ]
    .into_iter()
    .map(|(path, body)| (path.into(), Reply::Body(body)))
    .collect()
}

#[tokio::test]
async fn malformed_utf8_xml_survives_loading_persistence_and_cache_reuse() {
    let mut xml = XML.as_bytes().to_vec();
    let at = xml.windows(4).position(|s| s == b"Test").unwrap();
    xml[at] = 0xe6; // Stray UTF-8 byte, as reported in loxMqttRelay issue #4.
    let server = serve(routes_with_xml(&xml)).await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&server, &dir);
    let first = Loader::new(&cfg).unwrap().load().await.unwrap().unwrap();
    let cached = Loader::new(&cfg).unwrap().load().await.unwrap().unwrap();
    assert_eq!(first.grabber.len(), 1);
    for (id, route) in first.websocket {
        assert!(route.prefix.starts_with("\u{fffd}est,"));
        assert_eq!(route.prefix, cached.websocket[&id].prefix);
    }
    assert_eq!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == LIST)
            .count(),
        1
    );
}
type Requests = Arc<Mutex<Vec<(String, Option<String>)>>>;
#[tokio::test]
async fn corrupt_xml_in_a_version_matching_cache_is_downloaded_again() {
    let server = serve(routes()).await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&server, &dir);
    let expected = Loader::new(&cfg).unwrap().load().await.unwrap().unwrap();
    let path = std::path::Path::new(&cfg.paths.data_dir).join("configuration-cache.json");
    let mut cache: serde_json::Value =
        serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
    cache["xml"] = "not an XML document".into();
    tokio::fs::write(&path, serde_json::to_vec(&cache).unwrap())
        .await
        .unwrap();
    let recovered = Loader::new(&cfg).unwrap().load().await.unwrap().unwrap();
    assert_eq!(recovered.websocket.len(), expected.websocket.len());
    assert_eq!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == LIST)
            .count(),
        2
    );
    let cached = Loader::new(&cfg).unwrap().load().await.unwrap().unwrap();
    assert_eq!(cached.grabber.len(), expected.grabber.len());
    assert_eq!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == LIST)
            .count(),
        2
    );
}
struct Server {
    port: u16,
    requests: Requests,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn serve(routes: HashMap<String, Reply>) -> Server {
    let requests = Requests::default();
    let seen = requests.clone();
    let router = Router::new().fallback(move |request: Request<Body>| {
        let path = request.uri().path().to_owned();
        seen.lock().unwrap().push((
            path.clone(),
            request
                .headers()
                .get("authorization")
                .map(|v| v.to_str().unwrap().to_owned()),
        ));
        let reply = routes.get(&path).cloned();
        async move {
            match reply {
                Some(Reply::Body(body)) => Response::new(Body::from(body)),
                Some(Reply::Redirect(location)) => Response::builder()
                    .status(StatusCode::FOUND)
                    .header("location", location)
                    .body(Body::empty())
                    .unwrap(),
                None => Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Body::empty())
                    .unwrap(),
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Server {
        port,
        requests,
        task,
    }
}
fn config(server: &Server, dir: &tempfile::TempDir) -> Config {
    let mut config = Config::default();
    config.miniserver.host = "127.0.0.1".into();
    config.miniserver.port = server.port;
    config.miniserver.user = "test".into();
    config.miniserver.password = "secret".into();
    config.paths.data_dir = dir
        .path()
        .join("not-yet-created/data")
        .to_string_lossy()
        .into_owned();
    config
}

#[tokio::test]
async fn redirects_preserve_same_origin_auth_and_load_firmware17_then_reuse_cache() {
    let mut files = routes();
    for path in [VERSION, LIST, FILE, APP] {
        let body = files.remove(path).unwrap();
        files.insert(path.into(), Reply::Redirect(format!("/redirect{path}")));
        files.insert(format!("/redirect{path}"), body);
    }
    let server = serve(files).await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&server, &dir);
    let mut loader = Loader::new(&cfg).unwrap();
    let controls = loader.load().await.unwrap().unwrap();
    assert_eq!(controls.grabber.len(), 1);
    assert!(loader.load().await.unwrap().is_none());
    assert!(
        std::path::Path::new(&cfg.paths.data_dir)
            .join("configuration-cache.json")
            .exists()
    );
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.iter().filter(|(path, _)| path == LIST).count(), 1);
    assert!(
        requests
            .iter()
            .all(|(_, auth)| auth.as_deref() == Some("Basic dGVzdDpzZWNyZXQ="))
    );
}

#[tokio::test]
async fn filesystem_and_app_http200_ll_errors_report_status_and_endpoint() {
    for endpoint in [LIST, FILE, APP] {
        for body in [
            br#"{"LL":{"Code":"403","value":"denied"}}"#.as_slice(),
            br#"{"LL":{"code":403,"value":"denied"}}"#,
            br#"<?xml version="1.0"?><LL Code="403" value="denied"/>"#,
        ] {
            let mut files = routes();
            files.insert(endpoint.into(), Reply::Body(body.to_vec()));
            let server = serve(files).await;
            let dir = tempfile::tempdir().unwrap();
            let mut loader = Loader::new(&config(&server, &dir)).unwrap();
            let error = format!("{:#}", loader.load().await.unwrap_err());
            assert!(
                error.contains("403"),
                "lost LL status for {endpoint}: {error}"
            );
            assert!(error.contains(endpoint), "lost endpoint: {error}");
            assert!(!error.contains("secret"));
        }
    }
}

#[tokio::test]
async fn redirect_loop_stops_with_http_error_instead_of_parsing_empty_config() {
    let mut files = routes();
    files.insert(LIST.into(), Reply::Redirect(LIST.into()));
    let server = serve(files).await;
    let dir = tempfile::tempdir().unwrap();
    let error = format!(
        "{:#}",
        Loader::new(&config(&server, &dir))
            .unwrap()
            .load()
            .await
            .unwrap_err()
    );
    assert!(error.contains("redirect"), "{error}");
    assert!(server.requests.lock().unwrap().len() <= 12);
}

#[tokio::test]
async fn redirects_to_another_origin_do_not_forward_credentials() {
    let destination = serve(routes()).await;
    let mut files = routes();
    files.insert(
        LIST.into(),
        Reply::Redirect(format!("http://127.0.0.1:{}{LIST}", destination.port)),
    );
    let source = serve(files).await;
    let dir = tempfile::tempdir().unwrap();
    Loader::new(&config(&source, &dir))
        .unwrap()
        .load()
        .await
        .unwrap();
    let requests = destination.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].1.is_none());
}
