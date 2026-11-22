//! Offline discovery/parity helper; never connects to a Miniserver.
use anyhow::{Context, Result};
fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let config = loxinflux::config::Config::load(std::path::Path::new(
        &args.next().context("config.toml path")?,
    ))?;
    let xml = std::fs::read(args.next().context("configuration.xml path")?)?;
    let start = std::time::Instant::now();
    let c = loxinflux::controls::parse(&xml, &config)?;
    eprintln!(
        "XML bytes: {}; routes: {}; grabber: {}; parse: {:?}",
        xml.len(),
        c.websocket.len(),
        c.grabber.len(),
        start.elapsed()
    );
    if args.next().as_deref() == Some("--json") {
        let routes: std::collections::BTreeMap<_, _> = c
            .websocket
            .iter()
            .map(|(id, r)| (id.to_string(), format!("{}{}=", r.prefix, r.numeric_key)))
            .collect();
        let grabber: std::collections::BTreeMap<_, _> = c
            .grabber
            .iter()
            .map(|(id, r)| {
                (
                    id.to_string(),
                    serde_json::json!({"prefix":r.prefix.as_ref(),"secured":r.secured}),
                )
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({"websocket":routes,"grabber":grabber})
        );
    }
    Ok(())
}
