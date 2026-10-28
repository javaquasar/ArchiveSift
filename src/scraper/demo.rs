use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

use reqwest::Url;
use scraper::{Html, Selector};
use serde::Deserialize;

use crate::http::fetch_text;
use crate::models::{
    ListingFetchBehavior, ListingPage, ListingPaginationBehavior, ListingUrl, MetadataAttribute,
    PageKind, ScrapedImage, ScrapedItem, ScraperCapabilities, ScraperDiagnostic,
};
use crate::scraper::ContentScraper;
use crate::AppResult;

pub struct DemoSiteScraper;

impl DemoSiteScraper {
    pub fn new() -> Self {
        Self
    }
}

impl ContentScraper for DemoSiteScraper {
    fn site_key(&self) -> &'static str {
        "demo-site"
    }

    fn capabilities(&self) -> ScraperCapabilities {
        ScraperCapabilities {
            supported_page_kinds: vec![PageKind::Content, PageKind::Listing],
            listing_behaviors: vec![
                ListingFetchBehavior::StaticHtml,
                ListingFetchBehavior::LazyLoadApi,
            ],
            discovers_child_listing_urls: false,
            enriches_content_metadata: false,
            emits_listing_diagnostics: true,
        }
    }

    fn supports_url(&self, url: &str) -> bool {
        matches!(
            self.classify_url(url),
            PageKind::Content | PageKind::Listing
        )
    }

    fn classify_url(&self, url: &str) -> PageKind {
        let Ok(parsed) = Url::parse(url) else {
            return PageKind::Unknown;
        };

        let host = parsed.host_str().unwrap_or_default();
        let path = parsed.path().to_ascii_lowercase();
        let looks_like_demo_host = host == "demo.example.test"
            || host == "127.0.0.1"
            || host == "localhost"
            || host.ends_with(".github.io");
        let looks_like_demo_path = path.contains("/demo-site/");

        if !(looks_like_demo_host || looks_like_demo_path) {
            return PageKind::Unknown;
        }

        if path.contains("/galleries/") {
            PageKind::Content
        } else if path.contains("/channels/") || path.contains("/models/") {
            PageKind::Listing
        } else {
            PageKind::Unknown
        }
    }

    fn scrape_content<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = AppResult<ScrapedItem>> + Send + 'a>> {
        Box::pin(async move {
            let raw_html = fetch_text(url).await?;
            parse_demo_gallery_html(self.site_key(), url, raw_html)
        })
    }

    fn scrape_listing<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = AppResult<ListingPage>> + Send + 'a>> {
        Box::pin(async move {
            let raw_html = fetch_text(url).await?;
            let parsed = parse_demo_listing_html(url, &raw_html)?;

            let mut discovered_urls = parsed
                .content_urls
                .into_iter()
                .map(ListingUrl::content)
                .collect::<Vec<_>>();
            let mut diagnostics = Vec::new();
            let mut fetch_behavior = ListingFetchBehavior::StaticHtml;
            if let Some(lazy_payload_url) = parsed.lazy_payload_url {
                let lazy_urls = collect_lazy_content_urls(&lazy_payload_url).await?;
                merge_urls(
                    &mut discovered_urls,
                    lazy_urls.into_iter().map(ListingUrl::content).collect(),
                );
                fetch_behavior = ListingFetchBehavior::LazyLoadApi;
                diagnostics.push(ScraperDiagnostic::info(
                    "demo.lazy-payload-chain",
                    format!(
                        "expanded listing through lazy payload chain starting at {}",
                        lazy_payload_url
                    ),
                ));
            }

            Ok(ListingPage {
                discovered_urls,
                next_page_url: parsed.next_page_url,
                total_pages: None,
                pagination_behavior: ListingPaginationBehavior::FollowNextPage,
                fetch_behavior,
                diagnostics,
            })
        })
    }
}

struct ParsedDemoListing {
    content_urls: Vec<String>,
    next_page_url: Option<String>,
    lazy_payload_url: Option<String>,
}

#[derive(Deserialize)]
struct DemoLazyPayload {
    items: Vec<DemoLazyItem>,
    #[serde(default)]
    next_payload: Option<String>,
}

#[derive(Deserialize)]
struct DemoLazyItem {
    href: String,
}

fn parse_demo_gallery_html(
    site_key: &str,
    page_url: &str,
    raw_html: String,
) -> AppResult<ScrapedItem> {
    let document = Html::parse_document(&raw_html);
    let base_url = Url::parse(page_url)?;

    let title = first_attr_value(&document, "[data-demo-title]", "data-demo-title")
        .or_else(|| extract_title_text(&document))
        .unwrap_or_else(|| "Demo Gallery".to_string());

    let mut attributes = Vec::new();
    collect_attributes(
        &document,
        "[data-demo-actor]",
        "data-demo-actor",
        "actor",
        &mut attributes,
    );
    collect_attributes(
        &document,
        "[data-demo-tag]",
        "data-demo-tag",
        "tag",
        &mut attributes,
    );
    collect_attributes(
        &document,
        "[data-demo-category]",
        "data-demo-category",
        "category",
        &mut attributes,
    );
    collect_attributes(
        &document,
        "[data-demo-studio]",
        "data-demo-studio",
        "studio",
        &mut attributes,
    );
    collect_attributes(
        &document,
        "[data-demo-description]",
        "data-demo-description",
        "description",
        &mut attributes,
    );
    collect_attributes(
        &document,
        "[data-demo-date]",
        "data-demo-date",
        "date",
        &mut attributes,
    );
    collect_attributes(
        &document,
        "[data-demo-director]",
        "data-demo-director",
        "director",
        &mut attributes,
    );
    collect_attributes(
        &document,
        "[data-demo-duration]",
        "data-demo-duration",
        "duration",
        &mut attributes,
    );
    collect_attributes(
        &document,
        "[data-demo-studio-code]",
        "data-demo-studio-code",
        "studio_code",
        &mut attributes,
    );
    collect_attributes(
        &document,
        "[data-demo-resource-link]",
        "data-demo-resource-link",
        "resource_link",
        &mut attributes,
    );

    let image_selector = Selector::parse("img[data-demo-full]").unwrap();
    let mut images = Vec::new();

    for (index, img) in document.select(&image_selector).enumerate() {
        let Some(full) = img.value().attr("data-demo-full") else {
            continue;
        };
        let full = full.trim();
        if full.is_empty() {
            continue;
        }
        let source_url = resolve_url(&base_url, full)?;
        let preview_url = img
            .value()
            .attr("src")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| resolve_url(&base_url, value))
            .transpose()?;

        images.push(ScrapedImage {
            source_url,
            preview_url,
            alt_text: img.value().attr("alt").map(ToString::to_string),
            width: None,
            height: None,
            sort_order: index as i64,
            local_file_path: None,
        });
    }

    Ok(ScrapedItem {
        site_key: site_key.to_string(),
        page_url: page_url.to_string(),
        title,
        raw_html,
        images,
        attributes,
    })
}

fn parse_demo_listing_html(page_url: &str, raw_html: &str) -> AppResult<ParsedDemoListing> {
    let document = Html::parse_document(raw_html);
    let base_url = Url::parse(page_url)?;

    let gallery_selector = Selector::parse("a.demo-gallery-link[href]").unwrap();
    let next_selector = Selector::parse("a.demo-next-page[href]").unwrap();
    let lazy_selector = Selector::parse("[data-demo-lazy-source]").unwrap();

    let content_urls = document
        .select(&gallery_selector)
        .filter_map(|node| node.value().attr("href"))
        .map(|href| resolve_url(&base_url, href))
        .collect::<Result<Vec<_>, _>>()?;

    let next_page_url = document
        .select(&next_selector)
        .next()
        .and_then(|node| node.value().attr("href"))
        .map(|href| resolve_url(&base_url, href))
        .transpose()?;

    let lazy_payload_url = document
        .select(&lazy_selector)
        .next()
        .and_then(|node| node.value().attr("data-demo-lazy-source"))
        .map(|href| resolve_url(&base_url, href))
        .transpose()?;

    Ok(ParsedDemoListing {
        content_urls,
        next_page_url,
        lazy_payload_url,
    })
}

async fn collect_lazy_content_urls(first_payload_url: &str) -> AppResult<Vec<String>> {
    let mut urls = Vec::new();
    let mut seen_content_urls = HashSet::new();
    let mut seen_payload_urls = HashSet::new();
    let mut next_payload = Some(first_payload_url.to_string());

    while let Some(payload_url) = next_payload {
        if !seen_payload_urls.insert(payload_url.clone()) {
            break;
        }

        let base_url = Url::parse(&payload_url)?;
        let raw_payload = fetch_text(&payload_url).await?;
        let payload = parse_lazy_payload(&base_url, &raw_payload)?;

        for url in payload.content_urls {
            if seen_content_urls.insert(url.clone()) {
                urls.push(url);
            }
        }

        next_payload = payload.next_payload_url;
    }

    Ok(urls)
}

struct ParsedLazyPayload {
    content_urls: Vec<String>,
    next_payload_url: Option<String>,
}

fn parse_lazy_payload(base_url: &Url, raw_payload: &str) -> AppResult<ParsedLazyPayload> {
    let payload: DemoLazyPayload = serde_json::from_str(raw_payload)?;

    let content_urls = payload
        .items
        .into_iter()
        .map(|item| resolve_url(base_url, &item.href))
        .collect::<Result<Vec<_>, _>>()?;

    let next_payload_url = payload
        .next_payload
        .as_deref()
        .map(|href| resolve_url(base_url, href))
        .transpose()?;

    Ok(ParsedLazyPayload {
        content_urls,
        next_payload_url,
    })
}

fn resolve_url(base_url: &Url, href: &str) -> AppResult<String> {
    Ok(base_url.join(href)?.to_string())
}

fn merge_urls(target: &mut Vec<ListingUrl>, additional: Vec<ListingUrl>) {
    let mut seen: HashSet<(String, crate::models::ListingUrlKind)> = target
        .iter()
        .map(|entry| (entry.url.clone(), entry.kind))
        .collect();
    for entry in additional {
        let key = (entry.url.clone(), entry.kind);
        if seen.insert(key) {
            target.push(entry);
        }
    }
}

fn first_attr_value(document: &Html, selector: &str, attr_name: &str) -> Option<String> {
    let selector = Selector::parse(selector).ok()?;
    document
        .select(&selector)
        .next()
        .and_then(|node| node.value().attr(attr_name))
        .map(ToString::to_string)
}

fn extract_title_text(document: &Html) -> Option<String> {
    let selector = Selector::parse("title").ok()?;
    document
        .select(&selector)
        .next()
        .map(|node| node.text().collect::<String>().trim().to_string())
        .filter(|title| !title.is_empty())
}

fn collect_attributes(
    document: &Html,
    selector: &str,
    attr_name: &str,
    kind: &str,
    attributes: &mut Vec<MetadataAttribute>,
) {
    let selector = Selector::parse(selector).unwrap();
    for node in document.select(&selector) {
        let Some(value) = node.value().attr(attr_name) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }

        attributes.push(MetadataAttribute {
            kind: kind.to_string(),
            label: kind.to_string(),
            value: value.to_string(),
            source_url: None,
            is_female: if kind == "actor" { Some(true) } else { None },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scraper::ContentScraper;

    #[test]
    fn parses_gallery_page_metadata_and_images() {
        let html = r#"
        <html>
          <head><title>Ignored Title</title></head>
          <body>
            <main data-demo-role="gallery">
              <h1 data-demo-title="Demo Summer Set">Demo Summer Set</h1>
              <span data-demo-actor="Alice Blue">Alice Blue</span>
              <span data-demo-tag="outdoor">outdoor</span>
              <span data-demo-category="editorial">editorial</span>
              <span data-demo-studio="Studio Nova">Studio Nova</span>
              <p data-demo-description="A deterministic demo collection">Description</p>
              <time data-demo-date="2026-10-28">2026-10-28</time>
              <span data-demo-director="Demo Director">Demo Director</span>
              <span data-demo-duration="00:42:15">00:42:15</span>
              <span data-demo-studio-code="DEMO-004">DEMO-004</span>
              <a data-demo-resource-link="https://demo.example.test/source/demo">Source</a>
              <img data-demo-full="../../media/a.jpg" src="../../media/thumb-a.jpg" alt="A">
              <img data-demo-full="https://demo.example.test/media/b.jpg" src="thumb-b.jpg" alt="B">
            </main>
          </body>
        </html>
        "#;

        let page = parse_demo_gallery_html(
            "demo-site",
            "https://demo.example.test/galleries/demo/",
            html.to_string(),
        )
        .expect("gallery parse should succeed");

        assert_eq!(page.title, "Demo Summer Set");
        assert_eq!(page.images.len(), 2);
        assert_eq!(
            page.images[0].source_url,
            "https://demo.example.test/media/a.jpg"
        );
        assert_eq!(
            page.images[0].preview_url.as_deref(),
            Some("https://demo.example.test/media/thumb-a.jpg")
        );
        assert!(page
            .attributes
            .iter()
            .any(|a| a.kind == "actor" && a.value == "Alice Blue"));
        assert!(page
            .attributes
            .iter()
            .any(|a| a.kind == "tag" && a.value == "outdoor"));
        assert!(page
            .attributes
            .iter()
            .any(|a| a.kind == "category" && a.value == "editorial"));
        assert!(page
            .attributes
            .iter()
            .any(|a| a.kind == "studio" && a.value == "Studio Nova"));
        for (kind, value) in [
            ("description", "A deterministic demo collection"),
            ("date", "2026-10-28"),
            ("director", "Demo Director"),
            ("duration", "00:42:15"),
            ("studio_code", "DEMO-004"),
            ("resource_link", "https://demo.example.test/source/demo"),
        ] {
            assert!(page
                .attributes
                .iter()
                .any(|attribute| attribute.kind == kind && attribute.value == value));
        }
    }

    #[test]
    fn parses_lazy_listing_html_and_payload_links() {
        let html = r#"
        <html>
          <body>
            <main data-demo-role="listing">
              <section data-demo-lazy-source="../../../api/lazy/studio-nova-page-1.json"></section>
            </main>
          </body>
        </html>
        "#;
        let listing = parse_demo_listing_html(
            "https://demo.example.test/demo-site/channels/studio-nova/lazy/",
            html,
        )
        .expect("listing parse should succeed");

        assert_eq!(
            listing.lazy_payload_url.as_deref(),
            Some("https://demo.example.test/demo-site/api/lazy/studio-nova-page-1.json")
        );

        let payload_1 = parse_lazy_payload(
            &Url::parse("https://demo.example.test/demo-site/api/lazy/studio-nova-page-1.json")
                .unwrap(),
            r#"{
              "items": [
                { "href": "../../galleries/summer-escape-001/" },
                { "href": "../../galleries/city-lights-002/" }
              ],
              "next_payload": "./studio-nova-page-2.json"
            }"#,
        )
        .expect("payload 1 parse should succeed");

        assert_eq!(payload_1.content_urls.len(), 2);
        assert_eq!(
            payload_1.next_payload_url.as_deref(),
            Some("https://demo.example.test/demo-site/api/lazy/studio-nova-page-2.json")
        );
    }

    #[tokio::test]
    async fn scraper_follows_lazy_payload_chain_for_listing_page() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;

        let html = r#"<!DOCTYPE html>
        <html>
          <body>
            <main data-demo-role="listing" data-demo-lazy-source="../../../api/lazy/studio-nova-page-1.json">
              <h1>Lazy Listing</h1>
            </main>
          </body>
        </html>"#;

        let payload_1 = r#"{
          "items": [
            { "href": "../../galleries/summer-escape-001/" },
            { "href": "../../galleries/city-lights-002/" }
          ],
          "next_payload": "./studio-nova-page-2.json"
        }"#;

        let payload_2 = r#"{
          "items": [
            { "href": "../../galleries/quiet-morning-003/" }
          ]
        }"#;

        let listener = TcpListener::bind("127.0.0.1:0").expect("listener bind should succeed");
        let addr = listener
            .local_addr()
            .expect("listener address should be available");

        let server = thread::spawn(move || {
            let responses = [
                (
                    "/demo-site/channels/studio-nova/lazy/",
                    "text/html; charset=utf-8",
                    html,
                ),
                (
                    "/demo-site/api/lazy/studio-nova-page-1.json",
                    "application/json",
                    payload_1,
                ),
                (
                    "/demo-site/api/lazy/studio-nova-page-2.json",
                    "application/json",
                    payload_2,
                ),
            ];

            for (expected_path, content_type, body) in responses {
                let (mut stream, _) = listener.accept().expect("test server should accept");
                let mut buf = [0_u8; 4096];
                let n = stream.read(&mut buf).expect("request should be readable");
                let request = String::from_utf8_lossy(&buf[..n]);
                let first_line = request.lines().next().unwrap_or_default();
                let path = first_line.split_whitespace().nth(1).unwrap_or_default();
                assert_eq!(path, expected_path);

                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    content_type,
                    body.len(),
                    body
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("response should be written");
            }
        });

        let scraper = DemoSiteScraper::new();
        let url = format!("http://{}/demo-site/channels/studio-nova/lazy/", addr);
        let listing = scraper
            .scrape_listing(&url)
            .await
            .expect("lazy listing scrape should succeed");

        let content_urls = listing.content_urls().collect::<Vec<_>>();

        assert_eq!(content_urls.len(), 3);
        let summer = format!("http://{}/demo-site/galleries/summer-escape-001/", addr);
        let city = format!("http://{}/demo-site/galleries/city-lights-002/", addr);
        let quiet = format!("http://{}/demo-site/galleries/quiet-morning-003/", addr);

        assert!(content_urls.iter().any(|url| *url == summer));
        assert!(content_urls.iter().any(|url| *url == city));
        assert!(content_urls.iter().any(|url| *url == quiet));
        assert_eq!(listing.next_page_url, None);
        assert_eq!(listing.fetch_behavior, ListingFetchBehavior::LazyLoadApi);
        assert_eq!(
            listing.pagination_behavior,
            ListingPaginationBehavior::FollowNextPage
        );
        assert_eq!(listing.diagnostics.len(), 1);

        server.join().expect("test server should complete");
    }
}
