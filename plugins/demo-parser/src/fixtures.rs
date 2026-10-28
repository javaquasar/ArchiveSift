//! Deliberately hostile behaviors compiled only by the integration test build.
use archive_sift_scraper_api::*;

pub fn execute(input: &Request) -> Result<Option<Action>, String> {
    let Some(mode) = input.url.split("/__fixture__/").nth(1) else {
        return Ok(None);
    };
    let action = match mode {
        "loop" => loop {
            std::hint::black_box(1_u64.wrapping_add(1));
        },
        "trap" => panic!("intentional fixture trap"),
        "memory" => {
            let bytes = vec![1_u8; 200 * 1024 * 1024];
            std::hint::black_box(bytes);
            return Err("memory fixture unexpectedly completed".into());
        }
        "fetch-denied" => Action::Fetch(FetchRequest::get("https://ungranted.invalid/secret")),
        "fetch-repeat" => Action::Fetch(FetchRequest::get(input.url.clone())),
        "image-denied" => Action::Content(Content {
            title: "Denied image".into(),
            images: vec![Image {
                url: "https://ungranted.invalid/image.png".into(),
                preview_url: None,
                alt_text: None,
                width: None,
                height: None,
            }],
            attributes: vec![],
        }),
        "wrong-kind" => Action::Listing(Listing {
            urls: vec![],
            next_page_url: None,
        }),
        "large-output" => Action::Content(Content {
            title: "x".repeat(64 * 1024),
            images: vec![],
            attributes: vec![],
        }),
        "error" => return Err("intentional extension error".into()),
        "post" => {
            if let Some(response) = &input.response {
                if input.state.as_deref() != Some("post-state") || response.body != "posted" {
                    return Err("lost continuation or POST response".into());
                }
                Action::Content(Content {
                    title: "POST complete".into(),
                    images: vec![],
                    attributes: vec![],
                })
            } else {
                Action::Fetch(FetchRequest {
                    url: input
                        .url
                        .replace("/galleries/__fixture__/post", "/api/post"),
                    method: HttpMethod::Post,
                    headers: vec![Header {
                        name: "content-type".into(),
                        value: "application/json".into(),
                    }],
                    body: Some("{\"query\":\"demo\"}".into()),
                    state: Some("post-state".into()),
                })
            }
        }
        _ => return Err("unknown fixture behavior".into()),
    };
    Ok(Some(action))
}
