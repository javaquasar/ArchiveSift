//! Native process settings checks; kept outside the WASM-only test target.
use archive_sift::scraper::{extensions::manifest::PluginManifest, ScraperRegistry};
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct DemoSite {
    origin: String,
    task: tokio::task::JoinHandle<()>,
}
impl DemoSite {
    async fn start(_: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                let _ = socket.read(&mut request).await.unwrap();
                let body=br#"<div data-demo-title="Configured" data-demo-description="Demo description"><img data-demo-full="/media/one.png"><img data-demo-full="/media/two.png"></div>"#;
                let head=format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len());
                socket.write_all(head.as_bytes()).await.unwrap();
                socket.write_all(body).await.unwrap();
            }
        });
        Self { origin, task }
    }
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.origin)
    }
}
impl Drop for DemoSite {
    fn drop(&mut self) {
        self.task.abort();
    }
}
const CONTENT_PATH: &str = "/galleries/configured/";

#[tokio::test]
async fn declared_settings_reach_native_processes_over_the_same_protocol() {
    use serde_json::json;
    let site = DemoSite::start("").await;
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-plugins/process");
    let root = tempfile::tempdir().unwrap();
    let package = root.path().join("demo.process");
    std::fs::create_dir(&package).unwrap();
    let mut manifest: PluginManifest =
        serde_json::from_slice(&std::fs::read(source.join("plugin.json")).unwrap()).unwrap();
    std::fs::copy(source.join(&manifest.entry), package.join(&manifest.entry)).unwrap();
    std::fs::copy(
        source.join("settings.schema.json"),
        package.join("settings.schema.json"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            package.join(&manifest.entry),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    manifest.network_origins = vec![site.origin.clone()];
    manifest.allow_http = true;
    manifest.enabled = true;
    manifest.trusted = true;
    for route in &mut manifest.routes {
        route.host = "127.0.0.1".into();
    }
    std::fs::write(
        package.join("plugin.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let registry = ScraperRegistry::with_extensions_from(root.path());
    assert!(registry.extension_statuses()[0].loaded);
    let url = site.url(CONTENT_PATH);
    let parser = registry.scraper_for_url(&url).unwrap();
    let options = json!({"max_images":1,"include_description":false});
    let content = parser
        .scrape_content_configured(&url, Some(&options))
        .await
        .unwrap();
    assert_eq!(content.images.len(), 1);
    assert!(!content.attributes.iter().any(|a| a.kind == "description"));
}
