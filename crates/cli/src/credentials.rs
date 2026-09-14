//! Installer-managed owner credentials for hosted repository addresses.

use std::path::PathBuf;

pub fn resolve(explicit: Option<&str>, host: Option<&str>) -> Result<String, String> {
    if let Some(token) = explicit.map(str::trim).filter(|token| !token.is_empty()) {
        return Ok(token.to_string());
    }
    let Some(host) = host else {
        return Err("no token: set TANDEM_TOKEN or pass --token".to_string());
    };
    let path = credentials_path()?;
    let contents = std::fs::read_to_string(&path).map_err(|error| {
        format!(
            "no token for {host:?}: cannot read {}: {error}; run the host installer or set TANDEM_TOKEN",
            path.display()
        )
    })?;
    if let Some(token) = find(&contents, host) {
        return Ok(token.to_string());
    }
    Err(format!(
        "no token for {host:?} in {}; run the host installer or set TANDEM_TOKEN",
        path.display()
    ))
}

fn find<'a>(contents: &'a str, host: &str) -> Option<&'a str> {
    for line in contents.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() == host {
            let token = value.trim();
            if !token.is_empty() {
                return Some(token);
            }
        }
    }
    None
}

fn credentials_path() -> Result<PathBuf, String> {
    if let Some(root) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(root).join("td/credentials"));
    }
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".config/td/credentials"))
        .ok_or_else(|| {
            "no credential location: set XDG_CONFIG_HOME or HOME, or set TANDEM_TOKEN".to_string()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_token_precedes_the_file_without_needing_a_home() {
        assert_eq!(resolve(Some(" owner "), Some("host")), Ok("owner".into()));
    }

    #[test]
    fn credential_parser_matches_the_exact_host() {
        let contents = "example.test = wrong\nexample.test:8443 = right\n";
        assert_eq!(find(contents, "example.test:8443"), Some("right"));
    }
}
