//! What `td clone <address>` was given, and what it means.
//!
//! Two shapes reach this module. `tandem.land/laurynas/my-project` is a hosted
//! repository: a host, then a name of exactly two segments, spoken to over
//! HTTPS with the name as the path prefix. `server:13013` is a classic
//! one-repository server, which is what the VM host is, and keeps the meaning
//! it always had. An explicit `http://` or `https://` URL settles the scheme
//! itself, which is how a hosted repository is reached over plain HTTP in a
//! test.
//!
//! The name rules are the ones the service enforces (see
//! `docs/hosting-on-cloudflare.md`): each segment is lowercase
//! `[a-z0-9._-]`, 2 to 64 characters, first and last alphanumeric. Checking
//! them here means a typo costs a message, not a round trip.

/// Where a clone points, and the name it asks for when the host has more than
/// one repository.
#[derive(Debug)]
pub struct CloneTarget {
    /// The base URL every request is built on, path prefix included.
    pub base_url: String,
    /// `<namespace>/<repository>`, when the address named one.
    pub name: Option<RepositoryName>,
    /// Host key used by the installer-managed credential file.
    pub credential_host: Option<String>,
}

use jj_tandem_protocol::names::RepositoryName;

impl CloneTarget {
    pub fn parse(address: &str) -> Result<Self, String> {
        let address = address.trim();
        if address.is_empty() {
            return Err("no address: clone takes <host>/<namespace>/<repository>".to_string());
        }

        let (scheme, rest) = match address.split_once("://") {
            Some((scheme, rest)) => {
                let scheme = scheme.to_ascii_lowercase();
                if scheme != "http" && scheme != "https" {
                    return Err(format!(
                        "unsupported scheme {scheme:?} in {address:?}; tandem speaks HTTP, so use \
                         host:port or an http(s):// URL"
                    ));
                }
                (Some(scheme), rest)
            }
            None => (None, address),
        };

        let rest = rest.trim_end_matches('/');
        let (host, path) = match rest.split_once('/') {
            Some((host, path)) => (host, Some(path)),
            None => (rest, None),
        };
        if host.is_empty() {
            return Err(format!("no host in {address:?}"));
        }

        let name = match path {
            None => None,
            Some(path) => Some(Self::parse_name(address, path)?),
        };

        let scheme = scheme.unwrap_or_else(|| {
            // A name means a hosted repository, and the service that hosts
            // repositories is on the public internet. A bare host:port is the
            // VM host, where the operator terminates TLS or does not. A host
            // that wants the other answer says so with an explicit scheme;
            // there is no list of addresses that quietly mean plain HTTP.
            if name.is_some() {
                "https".to_string()
            } else {
                "http".to_string()
            }
        });

        let base_url = match &name {
            Some(name) => format!(
                "{scheme}://{host}/{}/{}",
                name.namespace(),
                name.repository()
            ),
            None => format!("{scheme}://{host}"),
        };

        let credential_host = name.is_some().then(|| host.to_string());
        Ok(Self {
            base_url,
            name,
            credential_host,
        })
    }

    fn parse_name(address: &str, path: &str) -> Result<RepositoryName, String> {
        let segments: Vec<&str> = path.split('/').collect();
        if segments.len() != 2 {
            return Err(format!(
                "{address:?} is not a repository address: a name is exactly \
                 <namespace>/<repository>"
            ));
        }
        RepositoryName::parse(segments[0], segments[1])
            .map_err(|error| format!("invalid repository address {address:?}: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosted_address_is_a_host_and_a_two_segment_name() {
        let target = CloneTarget::parse("tandem.land/laurynas/my-project").expect("parse");
        assert_eq!(target.base_url, "https://tandem.land/laurynas/my-project");
        let name = target.name.as_ref().expect("a name");
        assert_eq!(name.namespace(), "laurynas");
        assert_eq!(name.repository(), "my-project");
    }

    #[test]
    fn a_classic_server_address_keeps_its_meaning() {
        let target = CloneTarget::parse("server:13013").expect("parse");
        assert_eq!(target.base_url, "http://server:13013");
        assert!(target.name.is_none());
    }

    #[test]
    fn only_an_explicit_scheme_makes_a_named_address_plain_http() {
        let implied = CloneTarget::parse("127.0.0.1:13013/ns/repo").expect("parse");
        assert_eq!(implied.base_url, "https://127.0.0.1:13013/ns/repo");
        let explicit = CloneTarget::parse("http://127.0.0.1:13013/ns/repo").expect("parse");
        assert_eq!(explicit.base_url, "http://127.0.0.1:13013/ns/repo");
    }

    #[test]
    fn an_explicit_scheme_is_kept() {
        let target = CloneTarget::parse("https://example.test/ns/repo").expect("parse");
        assert_eq!(target.base_url, "https://example.test/ns/repo");
        let plain = CloneTarget::parse("http://example.test:8080").expect("parse");
        assert_eq!(plain.base_url, "http://example.test:8080");
    }

    #[test]
    fn one_segment_is_not_a_repository_name() {
        let error = CloneTarget::parse("tandem.land/laurynas").expect_err("one segment");
        assert!(
            error.contains("tandem.land/laurynas") && error.contains("<namespace>/<repository>"),
            "{error}"
        );
    }

    #[test]
    fn three_segments_are_not_a_repository_name() {
        let error = CloneTarget::parse("tandem.land/a/b/c").expect_err("three segments");
        assert!(error.contains("tandem.land/a/b/c"), "{error}");
    }

    #[test]
    fn a_name_segment_is_lowercase_and_bounded() {
        let too_long = "x".repeat(65);
        for bad in [
            "tandem.land/Laurynas/my-project".to_string(),
            "tandem.land/a/my-project".to_string(),
            "tandem.land/laurynas/-leading".to_string(),
            "tandem.land/laurynas/trailing-".to_string(),
            "tandem.land/laurynas/has space".to_string(),
            "tandem.land/laurynas/exclaim!".to_string(),
            format!("tandem.land/laurynas/{too_long}"),
        ] {
            assert!(
                CloneTarget::parse(&bad).is_err(),
                "{bad} was accepted as a repository address"
            );
        }
    }

    #[test]
    fn a_name_segment_may_carry_digits_dots_and_hyphens() {
        let target = CloneTarget::parse("tandem.land/ns-1/my.repo_2").expect("parse");
        let name = target.name.as_ref().expect("a name");
        assert_eq!(name.namespace(), "ns-1");
        assert_eq!(name.repository(), "my.repo_2");
    }

    #[test]
    fn an_empty_address_names_what_is_missing() {
        let error = CloneTarget::parse("").expect_err("empty");
        assert!(error.contains("address"), "{error}");
    }
}
