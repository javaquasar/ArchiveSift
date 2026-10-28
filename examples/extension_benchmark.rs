//! Local, repeatable extension measurements; prepare the production demo WASM first.
use std::path::Path;
use std::time::Instant;

use archive_sift::scraper::extensions::manifest::PluginManifest;
use archive_sift::scraper::ScraperRegistry;
use archive_sift::AppResult;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() -> AppResult<()> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-plugins/demo-wasm");
    let mut manifest: PluginManifest =
        serde_json::from_slice(&std::fs::read(source.join("plugin.json"))?)?;
    let module = std::fs::read(source.join(&manifest.entry))?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("http://{}", listener.local_addr()?);
    let description = "x".repeat(32 * 1024);
    let html = format!("<h1 data-demo-title=\"Benchmark\">Benchmark</h1><p data-demo-description=\"{description}\"></p><img data-demo-full=\"/image.png\" src=\"/preview.png\">");
    let body_bytes = html.len();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut headers = Vec::new();
            let mut chunk = [0_u8; 2048];
            while !headers.windows(4).any(|part| part == b"\r\n\r\n") {
                let n = stream.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                headers.extend_from_slice(&chunk[..n]);
                assert!(headers.len() <= 8192);
            }
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}", html.len());
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    let root = tempfile::tempdir()?;
    let package = root.path().join("demo");
    std::fs::create_dir(&package)?;
    std::fs::write(package.join(&manifest.entry), &module)?;
    manifest.enabled = true;
    manifest.allow_http = true;
    manifest.network_origins = vec![origin.clone()];
    for route in &mut manifest.routes {
        route.host = "127.0.0.1".into();
    }
    std::fs::write(package.join("plugin.json"), serde_json::to_vec(&manifest)?)?;
    let mut load_ms = Vec::new();
    let mut registries = Vec::new();
    for _ in 0..3 {
        let started = Instant::now();
        let registry = ScraperRegistry::with_extensions_from(root.path());
        assert!(
            registry.extension_statuses()[0].loaded,
            "{:?}",
            registry.extension_statuses()
        );
        load_ms.push(started.elapsed().as_secs_f64() * 1000.0);
        registries.push(registry);
    }
    let registry = &registries[0];
    let clone_runs = 20_000;
    let started = Instant::now();
    for _ in 0..clone_runs {
        std::hint::black_box(registry.clone());
    }
    let clone_ms = started.elapsed().as_secs_f64() * 1000.0;
    let url = format!("{origin}/galleries/benchmark/");
    let scraper = registry.scraper_for_url(&url).unwrap();
    assert_eq!(scraper.site_key(), "demo-wasm");
    let parse_runs = 12;
    let started = Instant::now();
    for _ in 0..parse_runs {
        let page = scraper.scrape_content(&url).await?;
        assert_eq!(page.images.len(), 1);
        assert_eq!(page.attributes[0].value.len(), 32 * 1024);
    }
    let parse_ms = started.elapsed().as_secs_f64() * 1000.0;
    server.abort();
    println!(
        "{}",
        serde_json::json!({
            "profile": if cfg!(debug_assertions) { "dev" } else { "release" },
            "wasm_bytes": module.len(), "response_bytes": body_bytes,
            "load_ms": load_ms, "clone_runs": clone_runs, "clone_ms": clone_ms,
            "parse_runs": parse_runs, "parse_total_ms": parse_ms,
            "parse_mean_ms": parse_ms / f64::from(parse_runs)
        })
    );
    Ok(())
}
