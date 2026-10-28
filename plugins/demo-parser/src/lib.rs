//! Public example shared by the WASM component and the native process runner.
use archive_sift_scraper_api::*;
use scraper::{Html, Selector};
use url::Url;

pub fn execute(input: Request) -> Result<Action, String> {
    if input.api_version != API_VERSION {
        return Err("unsupported scraper API version".into());
    }
    #[cfg(feature = "test-fixtures")]
    if let Some(action) = fixtures::execute(&input)? {
        return Ok(action);
    }
    let Some(response) = input.response else {
        return Ok(Action::Fetch(FetchRequest::get(input.url)));
    };
    if response.status >= 400 {
        return Err(format!("HTTP {}", response.status));
    }
    let base = Url::parse(&response.url).map_err(|error| error.to_string())?;
    if input.operation == Operation::Listing {
        if let Some(state) = input.state {
            return parse_lazy_payload(
                &response.body,
                &base,
                serde_json::from_str(&state).map_err(|e| e.to_string())?,
            );
        }
    }
    let document = Html::parse_document(&response.body);
    match input.operation {
        Operation::Content => parse_content(&document, &base).map(Action::Content),
        Operation::Listing => {
            let listing = parse_listing(&document, &base)?;
            if let Some(source) = document
                .select(&selector("[data-demo-lazy-source]"))
                .find_map(|node| node.value().attr("data-demo-lazy-source"))
            {
                let url = base.join(source).map_err(|e| e.to_string())?.to_string();
                let state = LazyState {
                    urls: listing.urls,
                    next_page_url: listing.next_page_url,
                    seen_payloads: vec![url.clone()],
                };
                let mut request = FetchRequest::get(url);
                request.state = Some(serde_json::to_string(&state).map_err(|e| e.to_string())?);
                Ok(Action::Fetch(request))
            } else {
                Ok(Action::Listing(listing))
            }
        }
    }
}

fn selector(value: &str) -> Selector {
    Selector::parse(value).expect("constant demo selector")
}

fn parse_content(document: &Html, base: &Url) -> Result<Content, String> {
    let title = document
        .select(&selector("[data-demo-title]"))
        .find_map(|node| node.value().attr("data-demo-title"))
        .unwrap_or("Demo content")
        .to_string();
    let mut attributes = Vec::new();
    for (attribute, kind) in [
        ("data-demo-actor", "actor"),
        ("data-demo-tag", "tag"),
        ("data-demo-category", "category"),
        ("data-demo-studio", "studio"),
        ("data-demo-description", "description"),
        ("data-demo-date", "date"),
        ("data-demo-director", "director"),
        ("data-demo-duration", "duration"),
        ("data-demo-studio-code", "studio_code"),
        ("data-demo-resource-link", "resource_link"),
    ] {
        for node in document.select(&selector(&format!("[{attribute}]"))) {
            if let Some(value) = node
                .value()
                .attr(attribute)
                .filter(|value| !value.trim().is_empty())
            {
                let value = if kind == "resource_link" {
                    base.join(value)
                        .map_err(|error| error.to_string())?
                        .to_string()
                } else {
                    value.to_string()
                };
                attributes.push(Attribute {
                    kind: kind.into(),
                    label: kind.into(),
                    value,
                    source_url: None,
                    is_female: None,
                });
            }
        }
    }
    let mut images = Vec::new();
    for node in document.select(&selector("[data-demo-full]")) {
        let Some(value) = node.value().attr("data-demo-full") else {
            continue;
        };
        let url = base
            .join(value)
            .map_err(|error| error.to_string())?
            .to_string();
        if images.iter().any(|image: &Image| image.url == url) {
            continue;
        }
        images.push(Image {
            url,
            preview_url: node
                .value()
                .attr("src")
                .and_then(|value| base.join(value).ok())
                // Inline SVG placeholders are presentation data, not network assets.
                .filter(|url| matches!(url.scheme(), "http" | "https"))
                .map(String::from),
            alt_text: node.value().attr("alt").map(str::to_string),
            width: None,
            height: None,
        });
    }
    Ok(Content {
        title,
        images,
        attributes,
    })
}

fn parse_listing(document: &Html, base: &Url) -> Result<Listing, String> {
    let urls = document
        .select(&selector(
            "[data-demo-content-url], a.demo-gallery-link[href]",
        ))
        .filter_map(|node| {
            node.value()
                .attr("data-demo-content-url")
                .or_else(|| node.value().attr("href"))
        })
        .map(|url| {
            base.join(url)
                .map(|url| DiscoveredUrl {
                    url: url.into(),
                    operation: Operation::Content,
                })
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let next_page_url = document
        .select(&selector("[data-demo-next-page], a.demo-next-page[href]"))
        .find_map(|node| {
            node.value()
                .attr("data-demo-next-page")
                .or_else(|| node.value().attr("href"))
        })
        .map(|url| {
            base.join(url)
                .map(String::from)
                .map_err(|error| error.to_string())
        })
        .transpose()?;
    Ok(Listing {
        urls,
        next_page_url,
    })
}

#[derive(serde::Serialize, serde::Deserialize)]
struct LazyState {
    urls: Vec<DiscoveredUrl>,
    next_page_url: Option<String>,
    seen_payloads: Vec<String>,
}

#[derive(serde::Deserialize)]
struct LazyPayload {
    items: Vec<LazyItem>,
    next_payload: Option<String>,
}

#[derive(serde::Deserialize)]
struct LazyItem {
    href: String,
}

fn parse_lazy_payload(body: &str, base: &Url, mut state: LazyState) -> Result<Action, String> {
    let payload: LazyPayload = serde_json::from_str(body).map_err(|e| e.to_string())?;
    for item in payload.items {
        let url = base
            .join(&item.href)
            .map_err(|e| e.to_string())?
            .to_string();
        if !state.urls.iter().any(|entry| entry.url == url) {
            state.urls.push(DiscoveredUrl {
                url,
                operation: Operation::Content,
            });
        }
    }
    if let Some(next) = payload.next_payload {
        let url = base.join(&next).map_err(|e| e.to_string())?.to_string();
        if !state.seen_payloads.contains(&url) {
            state.seen_payloads.push(url.clone());
            let mut request = FetchRequest::get(url);
            request.state = Some(serde_json::to_string(&state).map_err(|e| e.to_string())?);
            return Ok(Action::Fetch(request));
        }
    }
    Ok(Action::Listing(Listing {
        urls: state.urls,
        next_page_url: state.next_page_url,
    }))
}

#[cfg(feature = "test-fixtures")]
mod fixtures;

#[cfg(target_arch = "wasm32")]
mod component {
    wit_bindgen::generate!({
        path: "../../crates/scraper-api/wit",
        world: "scraper-plugin",
        additional_derives: [serde::Serialize, serde::Deserialize],
    });
    struct DemoPlugin;
    archive_sift_scraper_api::impl_wit_conversions!(super::archivesift::scraper::types);
    impl Guest for DemoPlugin {
        fn execute(input: Request) -> Result<Action, String> {
            super::execute(input.into()).map(Into::into)
        }
    }
    export!(DemoPlugin);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_plugin_requests_document_then_parses_all_metadata() {
        let url = "https://example.test/galleries/demo/";
        let mut request = Request {
            api_version: API_VERSION,
            operation: Operation::Content,
            url: url.into(),
            response: None,
            state: None,
        };
        assert_eq!(
            execute(request.clone()).unwrap(),
            Action::Fetch(FetchRequest::get(url))
        );
        request.response = Some(HttpResponse {
            url: url.into(),
            status: 200,
            content_type: None,
            body: include_str!("../../../demo-site/galleries/random-showcase-004/index.html")
                .into(),
        });
        let Action::Content(content) = execute(request).unwrap() else {
            panic!("expected content")
        };
        assert_eq!(content.images.len(), 6);
        for kind in [
            "actor",
            "tag",
            "category",
            "studio",
            "description",
            "date",
            "director",
            "duration",
            "studio_code",
            "resource_link",
        ] {
            assert!(
                content
                    .attributes
                    .iter()
                    .any(|attribute| attribute.kind == kind),
                "missing {kind}"
            );
        }
        assert!(content
            .images
            .iter()
            .all(|image| image.url.starts_with("https://example.test/")));
    }

    #[test]
    fn demo_plugin_parses_legacy_gallery_without_treating_inline_svg_as_network_preview() {
        let url = "https://example.test/demo-site/galleries/summer-escape-001/";
        let request = Request {
            api_version: API_VERSION,
            operation: Operation::Content,
            url: url.into(),
            response: Some(HttpResponse {
                url: url.into(),
                status: 200,
                content_type: Some("text/html".into()),
                body: include_str!("../../../demo-site/galleries/summer-escape-001/index.html")
                    .into(),
            }),
            state: None,
        };
        let Action::Content(content) = execute(request).unwrap() else {
            panic!("expected content")
        };
        assert_eq!(content.title, "Summer Escape");
        assert_eq!(content.images.len(), 3);
        for (index, image) in content.images.iter().enumerate() {
            assert_eq!(
                image.url,
                format!(
                    "https://demo.example.test/media/summer-escape-001/image-{:03}.jpg",
                    index + 1
                )
            );
            assert_eq!(image.preview_url, None);
        }
    }

    #[test]
    fn demo_plugin_returns_listing_and_next_page() {
        let request = Request { api_version: API_VERSION, operation: Operation::Listing, url: "https://example.test/channels/test/".into(), response: Some(HttpResponse { url: "https://example.test/channels/test/".into(), status: 200, content_type: None, body: r#"<a data-demo-content-url="/galleries/one/"></a><a data-demo-next-page="page-2/"></a>"#.into() }), state: None };
        let Action::Listing(listing) = execute(request).unwrap() else {
            panic!("expected listing")
        };
        assert_eq!(listing.urls[0].url, "https://example.test/galleries/one/");
        assert_eq!(
            listing.next_page_url.as_deref(),
            Some("https://example.test/channels/test/page-2/")
        );
    }
}
