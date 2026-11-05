use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use archive_sift_scraper_api::{Action, Request};
use wasmtime::component::Linker;
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};

use super::compilation_cache::CompilationCache;
use super::manifest::{Limits, ModuleBytes};
use crate::AppResult;

mod bindings {
    wasmtime::component::bindgen!({
        path: "crates/scraper-api/wit",
        world: "scraper-plugin",
        additional_derives: [serde::Serialize, serde::Deserialize],
    });
}

archive_sift_scraper_api::impl_wit_conversions!(super::bindings::archivesift::scraper::types);

struct PreparedComponent {
    pre: bindings::ScraperPluginPre<StoreLimits>,
}

/// Compiled components are shared. Each call receives a fresh isolated Store.
pub struct WasmBackend {
    prepared: Arc<PreparedComponent>,
    limits: Limits,
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn shared_engine() -> AppResult<Engine> {
    static ENGINE: OnceLock<Result<Engine, String>> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            let mut config = Config::new();
            config
                .wasm_component_model(true)
                .consume_fuel(true)
                .epoch_interruption(true);
            let engine = Engine::new(&config).map_err(|error| error.to_string())?;
            let ticker = engine.clone();
            std::thread::Builder::new()
                .name("wasm-epoch".into())
                .spawn(move || loop {
                    std::thread::sleep(Duration::from_millis(10));
                    ticker.increment_epoch();
                })
                .map_err(|error| error.to_string())?;
            Ok(engine)
        })
        .as_ref()
        .cloned()
        .map_err(|error| error.clone().into())
}

impl WasmBackend {
    pub fn new(module: &ModuleBytes, limits: Limits) -> AppResult<Self> {
        let engine = shared_engine()?;
        static CACHE: CompilationCache<PreparedComponent> = CompilationCache::new(4);
        let prepared = CACHE.get_or_compile(module.digest(), || {
            let component =
                super::disk_cache::compile(&engine, module).map_err(|error| error.to_string())?;
            // Type-check and resolve imports/exports once, without invoking the guest.
            let linker = Linker::<StoreLimits>::new(&engine);
            let instance = linker
                .instantiate_pre(&component)
                .map_err(|error| error.to_string())?;
            let pre =
                bindings::ScraperPluginPre::new(instance).map_err(|error| error.to_string())?;
            Ok(PreparedComponent { pre })
        })?;
        Ok(Self { prepared, limits })
    }

    pub async fn execute(self: &Arc<Self>, input: Request) -> AppResult<Action> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let _cancel_guard = CancelOnDrop(Arc::clone(&cancelled));
        let backend = Arc::clone(self);
        let deadline = Instant::now() + Duration::from_millis(self.limits.timeout_ms);
        tokio::task::spawn_blocking(move || backend.execute_sync(input, cancelled, deadline))
            .await?
    }

    /// Executes a publisher-supplied response without network or credentials.
    /// Loading a package with a failing check cannot activate it.
    pub(super) fn check_offline_test(&self, directory: &std::path::Path) -> AppResult<bool> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Sample {
            request: Request,
            expected: Action,
        }
        let path = directory.join("self-test.json");
        if !path.exists() {
            return Ok(false);
        }
        let sample: Sample =
            serde_json::from_slice(&super::manager::bounded_read(&path, 1024 * 1024)?)?;
        if sample.request.api_version != 1
            || sample
                .request
                .response
                .as_ref()
                .is_none_or(|r| r.body.len() > self.limits.max_response_bytes)
            || matches!(sample.expected, Action::Fetch(_))
        {
            return Err(
                "self-test must parse a supplied bounded response into content or a listing".into(),
            );
        }
        let result = self.execute_sync(
            sample.request,
            Arc::new(AtomicBool::new(false)),
            Instant::now() + Duration::from_millis(self.limits.timeout_ms.min(3000)),
        )?;
        archive_sift_scraper_api::check_json_size(&result, self.limits.max_output_bytes)?;
        if result != sample.expected {
            return Err("offline parser self-test returned an unexpected result".into());
        }
        Ok(true)
    }

    fn execute_sync(
        &self,
        input: Request,
        cancelled: Arc<AtomicBool>,
        deadline: Instant,
    ) -> AppResult<Action> {
        if cancelled.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return Err("WASM execution cancelled or timed out".into());
        }
        let limits = StoreLimitsBuilder::new()
            .memory_size(self.limits.memory_mb * 1024 * 1024)
            .memories(1)
            .instances(4)
            .tables(4)
            .table_elements(10_000)
            .trap_on_grow_failure(true)
            .build();
        let mut store = Store::new(self.prepared.pre.engine(), limits);
        store.limiter(|limits| limits);
        store.set_fuel(self.limits.fuel)?;
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(move |_| {
            if cancelled.load(Ordering::Relaxed) || Instant::now() >= deadline {
                return Err(wasmtime::Error::msg(
                    "WASM execution cancelled or timed out",
                ));
            }
            Ok(wasmtime::UpdateDeadline::Continue(1))
        });
        let guest = self.prepared.pre.instantiate(&mut store)?;
        let input = input.into();
        let result = guest
            .call_execute(&mut store, &input)
            .map_err(|error| format!("WASM execution failed: {error:#}"))?
            .map_err(|error| format!("WASM plugin: {error}"))?;
        Ok(result.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use archive_sift_scraper_api as sdk;
    use bindings::archivesift::scraper::types as wit;

    #[test]
    fn wit_request_conversion_preserves_wire_format_and_moves_the_response_buffer() {
        for operation in [sdk::Operation::Content, sdk::Operation::Listing] {
            let input = sdk::Request {
                api_version: sdk::API_VERSION,
                operation,
                url: "https://example.test/content".into(),
                response: Some(sdk::HttpResponse {
                    url: "https://example.test/api".into(),
                    status: 200,
                    content_type: Some("application/json".into()),
                    body: "x".repeat(128 * 1024),
                }),
                state: Some("next cursor 🦀".into()),
            };
            let pointer = input.response.as_ref().unwrap().body.as_ptr();
            let expected = serde_json::to_value(&input).unwrap();
            let converted: wit::Request = input.into();
            assert_eq!(converted.response.as_ref().unwrap().body.as_ptr(), pointer);
            assert_eq!(serde_json::to_value(&converted).unwrap(), expected);
            let restored: sdk::Request = converted.into();
            assert_eq!(restored.response.as_ref().unwrap().body.as_ptr(), pointer);
            assert_eq!(serde_json::to_value(restored).unwrap(), expected);
        }
        let input = sdk::Request {
            api_version: 1,
            operation: sdk::Operation::Content,
            url: "https://example.test/".into(),
            response: None,
            state: None,
        };
        let converted: wit::Request = input.clone().into();
        assert_eq!(sdk::Request::from(converted), input);
    }

    #[test]
    fn wit_action_conversions_match_json_for_all_variants_and_optional_metadata() {
        let actions = [
            sdk::Action::Fetch(sdk::FetchRequest::get("https://example.test/")),
            sdk::Action::Fetch(sdk::FetchRequest {
                url: "https://example.test/api".into(),
                method: sdk::HttpMethod::Post,
                headers: vec![sdk::Header {
                    name: "content-type".into(),
                    value: "application/json".into(),
                }],
                body: Some("{}".into()),
                state: Some("cursor".into()),
            }),
            sdk::Action::Content(sdk::Content {
                title: "Example 🦀".into(),
                images: vec![
                    sdk::Image {
                        url: "/one".into(),
                        preview_url: Some("/thumb".into()),
                        alt_text: Some("alt".into()),
                        width: Some(u32::MAX),
                        height: Some(0),
                    },
                    sdk::Image {
                        url: "/two".into(),
                        preview_url: None,
                        alt_text: None,
                        width: None,
                        height: None,
                    },
                ],
                attributes: vec![
                    sdk::Attribute {
                        kind: "actor".into(),
                        label: "Actor".into(),
                        value: "Example".into(),
                        source_url: Some("/actor".into()),
                        is_female: Some(false),
                    },
                    sdk::Attribute {
                        kind: "description".into(),
                        label: "Description".into(),
                        value: "quoted \"text\"\n".into(),
                        source_url: None,
                        is_female: None,
                    },
                ],
            }),
            sdk::Action::Listing(sdk::Listing {
                urls: vec![
                    sdk::DiscoveredUrl {
                        url: "/content".into(),
                        operation: sdk::Operation::Content,
                    },
                    sdk::DiscoveredUrl {
                        url: "/listing".into(),
                        operation: sdk::Operation::Listing,
                    },
                ],
                next_page_url: Some("/next".into()),
            }),
            sdk::Action::Listing(sdk::Listing {
                urls: Vec::new(),
                next_page_url: None,
            }),
        ];
        for input in actions {
            let expected = serde_json::to_value(&input).unwrap();
            let converted: wit::Action = input.clone().into();
            assert_eq!(serde_json::to_value(&converted).unwrap(), expected);
            assert_eq!(sdk::Action::from(converted), input);
        }
    }
}
