//! Shared, allocation-conscious adapters for generated host and guest WIT types.

#[doc(hidden)]
#[macro_export]
macro_rules! __wit_record {
    ($sdk:ident, $wit:ident, $name:ident; [$($plain:ident),*]; [$($mapped:ident),*]; [$($optional:ident),*]; [$($list:ident),*]) => {
        impl From<$sdk::$name> for $wit::$name {
            fn from(value: $sdk::$name) -> Self {
                Self {
                    $($plain: value.$plain,)*
                    $($mapped: value.$mapped.into(),)*
                    $($optional: value.$optional.map(Into::into),)*
                    $($list: value.$list.into_iter().map(Into::into).collect(),)*
                }
            }
        }
        impl From<$wit::$name> for $sdk::$name {
            fn from(value: $wit::$name) -> Self {
                Self {
                    $($plain: value.$plain,)*
                    $($mapped: value.$mapped.into(),)*
                    $($optional: value.$optional.map(Into::into),)*
                    $($list: value.$list.into_iter().map(Into::into).collect(),)*
                }
            }
        }
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __wit_enum {
    ($sdk:ident, $wit:ident, $name:ident; $($variant:ident),*) => {
        impl From<$sdk::$name> for $wit::$name {
            fn from(value: $sdk::$name) -> Self {
                match value { $($sdk::$name::$variant => Self::$variant,)* }
            }
        }
        impl From<$wit::$name> for $sdk::$name {
            fn from(value: $wit::$name) -> Self {
                match value { $($wit::$name::$variant => Self::$variant,)* }
            }
        }
    };
}

/// Implements conversions for a generated `archivesift::scraper::types` module.
/// The module path is relative to the conversion submodule created by this macro.
/// Strings and response bodies are moved, never serialized through JSON.
#[macro_export]
macro_rules! impl_wit_conversions {
    ($bindings:path) => {
        mod sdk_wit_conversion {
            use $bindings as wit;
            mod sdk {
                pub use $crate::{Action, Attribute, Content, DiscoveredUrl, FetchRequest, Header,
                    HttpMethod, HttpResponse, Image, Listing, Operation, Request};
            }

            $crate::__wit_enum!(sdk, wit, Operation; Content, Listing);
            $crate::__wit_enum!(sdk, wit, HttpMethod; Get, Post);
            $crate::__wit_record!(sdk, wit, Header; [name, value]; []; []; []);
            $crate::__wit_record!(sdk, wit, HttpResponse; [url, status, content_type, body]; []; []; []);
            $crate::__wit_record!(sdk, wit, Request; [api_version, url, state]; [operation]; [response]; []);
            $crate::__wit_record!(sdk, wit, FetchRequest; [url, body, state]; [method]; []; [headers]);
            $crate::__wit_record!(sdk, wit, Attribute; [kind, label, value, source_url, is_female]; []; []; []);
            $crate::__wit_record!(sdk, wit, Image; [url, preview_url, alt_text, width, height]; []; []; []);
            $crate::__wit_record!(sdk, wit, Content; [title]; []; []; [images, attributes]);
            $crate::__wit_record!(sdk, wit, DiscoveredUrl; [url]; [operation]; []; []);
            $crate::__wit_record!(sdk, wit, Listing; [next_page_url]; []; []; [urls]);

            impl From<sdk::Action> for wit::Action {
                fn from(value: sdk::Action) -> Self {
                    match value {
                        sdk::Action::Fetch(value) => Self::Fetch(value.into()),
                        sdk::Action::Content(value) => Self::Content(value.into()),
                        sdk::Action::Listing(value) => Self::Listing(value.into()),
                    }
                }
            }
            impl From<wit::Action> for sdk::Action {
                fn from(value: wit::Action) -> Self {
                    match value {
                        wit::Action::Fetch(value) => Self::Fetch(value.into()),
                        wit::Action::Content(value) => Self::Content(value.into()),
                        wit::Action::Listing(value) => Self::Listing(value.into()),
                    }
                }
            }
        }
    };
}
