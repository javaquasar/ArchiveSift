use std::net::IpAddr;
use std::sync::Arc;

use reqwest::{dns, Client, Url};

use crate::AppResult;

pub(crate) const ISSUE_KIND: &str = "image_origin_required";
pub(crate) const VIEWER_HEADER: &str = "x-archivesift-image-viewer";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ImageAccessIssue {
    pub origin: String,
    pub resource: String,
    pub reason: &'static str,
}

impl ImageAccessIssue {
    pub fn for_url(mut url: Url, reason: &'static str) -> Option<Self> {
        let origin = public_image_origin(&url)?;
        url.set_query(None);
        url.set_fragment(None);
        if url.as_str().len() > 2048 {
            return None;
        }
        Some(Self {
            origin,
            resource: url.into(),
            reason,
        })
    }

    pub fn from_metadata(attribute: &crate::models::MetadataAttribute) -> Option<Self> {
        let value = match (attribute.kind.as_str(), attribute.label.as_str()) {
            (ISSUE_KIND, _) => attribute.source_url.as_deref()?,
            ("notice", "Image host permission") => {
                attribute.source_url.as_deref().unwrap_or(&attribute.value)
            }
            _ => return None,
        };
        let reason = match attribute.label.as_str() {
            "redirect" => "redirect",
            "download" => "download",
            "viewer" => "viewer",
            _ => "discovered",
        };
        Self::for_reference(value, reason)
    }

    pub fn for_reference(value: &str, reason: &'static str) -> Option<Self> {
        let url = Url::parse(value).ok().or_else(|| {
            (!value.contains(['/', '@', ':', '?', '#']))
                .then(|| Url::parse(&format!("https://{value}")).ok())
                .flatten()
        })?;
        Self::for_url(url, reason)
    }

    pub fn into_metadata(self) -> crate::models::MetadataAttribute {
        crate::models::MetadataAttribute {
            kind: ISSUE_KIND.into(),
            label: self.reason.into(),
            value: self.origin,
            source_url: Some(self.resource),
            is_female: None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ImagePermissionRequired(pub ImageAccessIssue);

impl std::fmt::Display for ImagePermissionRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Image resource needs permission for {}", self.0.origin)
    }
}

impl std::error::Error for ImagePermissionRequired {}

pub(crate) fn public_image_origin(url: &Url) -> Option<String> {
    let Some(url::Host::Domain(domain)) = url.host() else {
        return None;
    };
    if url.scheme() != "https"
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || !domain.contains('.')
        || domain.ends_with('.')
        || ["localhost", "local", "internal", "home.arpa"]
            .iter()
            .any(|suffix| {
                domain == *suffix
                    || domain
                        .strip_suffix(suffix)
                        .is_some_and(|prefix| prefix.ends_with('.'))
            })
    {
        return None;
    }
    Some(url.origin().ascii_serialization())
}

pub(crate) fn is_granted(url: &Url, grants: &[String]) -> bool {
    public_image_origin(url).is_some_and(|origin| grants.contains(&origin))
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_documentation()
                && !matches!(
                    (a, b, c),
                    (0, _, _)
                        | (100, 64..=127, _)
                        | (192, 0, 0)
                        | (192, 88, 99)
                        | (198, 18..=19, _)
                        | (224..=255, _, _)
                )
        }
        IpAddr::V6(ip) => {
            let [a, b, ..] = ip.segments();
            // Global unicast, excluding special allocations and transition mechanisms.
            (0x2000..=0x3fff).contains(&a)
                && !matches!(a, 0x2002 | 0x3fff)
                && !(a == 0x2001 && (b < 0x0200 || b == 0x0db8))
        }
    }
}

struct PublicDns;

impl dns::Resolve for PublicDns {
    fn resolve(&self, name: dns::Name) -> dns::Resolving {
        Box::pin(async move {
            let addresses: Vec<_> = tokio::net::lookup_host((name.as_str(), 0)).await?.collect();
            if addresses.is_empty() || addresses.iter().any(|addr| !public_ip(addr.ip())) {
                return Err(std::io::Error::other(
                    "image host did not resolve to public addresses",
                )
                .into());
            }
            Ok(Box::new(addresses.into_iter()) as dns::Addrs)
        })
    }
}

pub(crate) fn public_client(builder: reqwest::ClientBuilder) -> AppResult<Client> {
    Ok(builder
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(Arc::new(PublicDns))
        .build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_are_exact_and_issues_omit_queries_and_fragments() {
        let grants = vec!["https://cdn.example.test".into()];
        let url = Url::parse("https://cdn.example.test/a.png?token=private#ref").unwrap();
        assert!(is_granted(&url, &grants));
        assert_eq!(
            ImageAccessIssue::for_url(url, "test").unwrap().resource,
            "https://cdn.example.test/a.png"
        );
        for url in [
            "http://cdn.example.test/a",
            "https://cdn.example.test:8443/a",
            "https://cdn.example.test.evil.test/a",
            "https://user@cdn.example.test/a",
            "https://localhost/a",
            "https://127.0.0.1/a",
            "https://[::1]/a",
            "https://foo.local/a",
        ] {
            assert!(!is_granted(&Url::parse(url).unwrap(), &grants), "{url}");
        }
    }

    #[test]
    fn private_and_special_dns_answers_are_rejected() {
        for ip in [
            "0.1.2.3",
            "10.0.0.1",
            "100.64.2.3",
            "127.0.0.1",
            "169.254.169.254",
            "192.168.1.1",
            "198.19.0.1",
            "224.0.0.1",
            "::ffff:127.0.0.1",
            "fe80::1",
            "fc00::1",
            "2002:7f00:1::",
            "2001:db8::1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "2606:4700::1"] {
            assert!(public_ip(ip.parse().unwrap()));
        }
    }
}
