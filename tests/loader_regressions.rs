//! HTTP regressions from loxInFlux #1/#3/#8 and commits 5bbf642/613de19/2e1c1ff.
use axum::{
    Router,
    body::Body,
    http::{Request, Response, StatusCode},
};
use loxinflux::{archive, config::Config, miniserver::Loader};
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
async fn malformed_utf8_xml_survives_loading() {
    let mut xml = XML.as_bytes().to_vec();
    let at = xml.windows(4).position(|s| s == b"Test").unwrap();
    xml[at] = 0xe6; // Stray UTF-8 byte, as reported in loxMqttRelay issue #4.
    let server = serve(routes_with_xml(&xml)).await;
    let controls = Loader::new(&config(&server))
        .unwrap()
        .load()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(controls.grabber.len(), 1);
    for route in controls.websocket.values() {
        assert!(route.prefix.starts_with("\u{fffd}est,"));
    }
}
type Requests = Arc<Mutex<Vec<(String, Option<String>)>>>;
#[tokio::test]
async fn every_start_downloads_the_configuration_again() {
    let server = serve(routes()).await;
    for _ in 0..2 {
        let controls = Loader::new(&config(&server))
            .unwrap()
            .load()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(controls.grabber.len(), 1);
    }
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
fn config(server: &Server) -> Config {
    let mut config = Config::default();
    config.miniserver.host = "127.0.0.1".into();
    config.miniserver.port = server.port;
    config.miniserver.user = "test".into();
    config.miniserver.password = "secret".into();
    config
}

#[tokio::test]
async fn redirects_preserve_same_origin_auth_and_load_firmware17_once_per_version() {
    let mut files = routes();
    for path in [VERSION, LIST, FILE, APP] {
        let body = files.remove(path).unwrap();
        files.insert(path.into(), Reply::Redirect(format!("/redirect{path}")));
        files.insert(format!("/redirect{path}"), body);
    }
    let server = serve(files).await;
    let mut loader = Loader::new(&config(&server)).unwrap();
    let controls = loader.load().await.unwrap().unwrap();
    assert_eq!(controls.grabber.len(), 1);
    assert!(loader.load().await.unwrap().is_none());
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.iter().filter(|(path, _)| path == LIST).count(), 1);
    assert!(
        requests
            .iter()
            .all(|(_, auth)| auth.as_deref() == Some("Basic dGVzdDpzZWNyZXQ="))
    );
}

#[tokio::test]
async fn known_version_skips_loxapp3_for_a_bare_loxcc() {
    let server = serve(routes()).await;
    let mut loader = Loader::new(&config(&server)).unwrap();
    assert_eq!(loader.load().await.unwrap().unwrap().grabber.len(), 1);
    let checked = Some("2026-07-27 22:37:21".to_owned());
    assert!(loader.load_version(checked).await.unwrap().is_none());
    let requests = server.requests.lock().unwrap();
    assert!(requests.iter().all(|(path, _)| path != APP));
    assert_eq!(requests.len(), 3);
}

#[tokio::test]
async fn failed_version_check_takes_the_version_from_loxapp3() {
    let mut files = routes();
    files.remove(VERSION);
    let server = serve(files).await;
    let mut loader = Loader::new(&config(&server)).unwrap();
    assert_eq!(loader.load().await.unwrap().unwrap().grabber.len(), 1);
    let checked = Some("2026-07-27 22:37:21".to_owned());
    assert!(loader.load_version(checked).await.unwrap().is_none());
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.iter().filter(|(path, _)| path == APP).count(), 1);
    assert_eq!(requests.iter().filter(|(path, _)| path == LIST).count(), 1);
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
            if endpoint == APP {
                // LoxAPP3.json is only fetched when the version is unknown.
                files.remove(VERSION);
            }
            let server = serve(files).await;
            let mut loader = Loader::new(&config(&server)).unwrap();
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
    let error = format!(
        "{:#}",
        Loader::new(&config(&server))
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
    Loader::new(&config(&source)).unwrap().load().await.unwrap();
    let requests = destination.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].1.is_none());
}
