//! Who is asking, and what they are allowed to ask for.
//!
//! There are exactly two authorities. The **admin token** is the one the
//! operator configures when the server starts: it mints the others and is the
//! only thing that may publish outside a single workspace's namespace. A
//! **workspace token** is minted from it, carries one workspace name, and
//! expires.
//!
//! A minted token is not written down anywhere. It carries its own workspace
//! and its own expiry in the clear, followed by a BLAKE2b tag over both keyed
//! by the admin token, so the server can tell a token it issued from a token
//! somebody wrote by hand without keeping a table of them. Three things follow
//! from that, and all three are the point:
//!
//! * A restart does not invalidate the tokens the workspaces on disk are
//!   holding. The server's memory is not where the truth was.
//! * A copy of the server's memory is not a set of usable credentials: what is
//!   in it is the admin token's digest, and the admin token itself only for as
//!   long as the process needs it to sign with.
//! * There is no revocation short of changing the admin token, which is why the
//!   expiry is short by default.
//!
//! The word for a token's authority over a workspace is the **writer role**
//! (`crate::writer`), which is a separate thing entirely: a token says
//! what a client *may* do, the writer role says which client is currently the
//! one doing it.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use blake2::{Blake2b512, Digest as _};
use rand::TryRngCore as _;

use jj_tandem_protocol::hex::{from_hex, to_hex};

/// How long a minted workspace token lives when the caller does not say.
///
/// Long enough that a working session does not stop halfway through, short
/// enough that a token copied out of a process listing is not a permanent
/// credential. Nothing refreshes a client's token yet — the workspace daemon
/// that will is a later stage — so this is also the longest a workspace can go
/// without being handed a new one.
pub const DEFAULT_TOKEN_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// The longest TTL a caller may ask for.
pub const MAX_TOKEN_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The prefix on a workspace token, so a leaked string says what it is.
const WORKSPACE_PREFIX: &str = "tdmw";
/// The prefix on an admin token.
const ADMIN_PREFIX: &str = "tdma";

/// A token is its prefix, an underscore, and its body.
///
/// The separator lives here and nowhere else, so a prefix exists in exactly one
/// spelling and cannot drift by an underscore between the two ends of it.
fn prefixed(prefix: &str, body: &str) -> String {
    format!("{prefix}_{body}")
}

/// The body of `token` behind `prefix`, or `None` when it is not one of those.
fn behind_prefix<'a>(prefix: &str, token: &'a str) -> Option<&'a str> {
    token.strip_prefix(prefix)?.strip_prefix('_')
}

/// What a presented token turns out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Authority {
    /// The operator's token: mints tokens, and publishes without a namespace
    /// restriction because it is the integrator.
    Admin,
    /// One workspace, named. Everything it publishes is checked against this.
    Workspace(String),
}

impl Authority {
    pub fn is_admin(&self) -> bool {
        matches!(self, Authority::Admin)
    }

    /// Whether this authority may act on `workspace`.
    pub fn may_act_for(&self, workspace: &str) -> bool {
        match self {
            Authority::Admin => true,
            Authority::Workspace(own) => own == workspace,
        }
    }
}

/// A freshly minted token, as the client is told about it exactly once.
#[derive(Debug, Clone)]
pub struct MintedToken {
    /// The bearer itself. The server keeps nothing.
    pub token: String,
    pub workspace_id: String,
    pub ttl: Duration,
}

/// The admin token, and the authority to mint and to check workspace tokens.
pub struct TokenStore {
    /// Kept in the clear because it is the signing key. Nothing prints it.
    admin_token: String,
    admin_digest: [u8; 64],
    retained_signing_keys: Vec<String>,
}

impl std::fmt::Debug for TokenStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenStore").finish_non_exhaustive()
    }
}

impl TokenStore {
    /// A store whose admin token is `admin_token`.
    #[cfg(test)]
    pub fn new(admin_token: &str) -> Self {
        Self {
            admin_token: admin_token.to_string(),
            admin_digest: digest(admin_token),
            retained_signing_keys: Vec::new(),
        }
    }

    pub fn with_retained(admin_token: &str, retained_signing_keys: Vec<String>) -> Self {
        Self {
            admin_token: admin_token.to_string(),
            admin_digest: digest(admin_token),
            retained_signing_keys,
        }
    }

    /// Mint a workspace-scoped bearer. `ttl` is clamped to [`MAX_TOKEN_TTL`].
    pub fn mint(&self, workspace_id: &str, ttl: Duration) -> MintedToken {
        let ttl = ttl.min(MAX_TOKEN_TTL);
        let expires_at = unix_now().saturating_add(ttl.as_secs());
        let payload = format!("{workspace_id}:{expires_at}");
        let tag = self.tag(&payload);
        MintedToken {
            token: prefixed(
                WORKSPACE_PREFIX,
                &format!("{}_{}", to_hex(payload.as_bytes()), to_hex(&tag[..32])),
            ),
            workspace_id: workspace_id.to_string(),
            ttl,
        }
    }

    /// What `presented` authorizes, or `None` when it authorizes nothing.
    ///
    /// An expired token is indistinguishable from one that was never minted,
    /// which is the point: expiry is not a state a caller can probe for.
    pub fn authority_for(&self, presented: &str) -> Option<Authority> {
        if constant_time_eq(&digest(presented), &self.admin_digest) {
            return Some(Authority::Admin);
        }

        let body = behind_prefix(WORKSPACE_PREFIX, presented)?;
        let (payload_hex, tag_hex) = body.split_once('_')?;
        let payload = String::from_utf8(from_hex(payload_hex).ok()?).ok()?;
        let presented_tag = from_hex(tag_hex).ok()?;
        if presented_tag.len() != 32 {
            return None;
        }
        if !self
            .signing_keys()
            .any(|key| constant_time_eq(&tag(key, &payload)[..32], &presented_tag))
        {
            return None;
        }

        // The workspace name is everything before the last colon: a name may
        // contain one, an expiry may not.
        let (workspace_id, expires_at) = payload.rsplit_once(':')?;
        if workspace_id.is_empty() {
            return None;
        }
        if expires_at.parse::<u64>().ok()? <= unix_now() {
            return None;
        }
        Some(Authority::Workspace(workspace_id.to_string()))
    }

    /// The tag that makes a payload a token: a keyed BLAKE2b over it.
    ///
    /// The key goes in first and BLAKE2b is not length-extendable, so this is a
    /// MAC rather than a hash somebody can extend. The separator byte keeps the
    /// key and the payload from running together into an ambiguity.
    fn tag(&self, payload: &str) -> [u8; 64] {
        tag(&self.admin_token, payload)
    }

    fn signing_keys(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.admin_token.as_str())
            .chain(self.retained_signing_keys.iter().map(String::as_str))
    }
}

fn tag(key: &str, payload: &str) -> [u8; 64] {
    let mut hasher = Blake2b512::new();
    hasher.update(key.as_bytes());
    hasher.update([0x1f]);
    hasher.update(payload.as_bytes());
    hasher.finalize().into()
}

/// A token nobody can guess: 32 bytes from the system CSPRNG, in hex, behind a
/// prefix that says at a glance what a leaked string is.
pub fn generate_token(prefix: &str) -> String {
    let mut raw = [0u8; 32];
    rand::rngs::OsRng
        .try_fill_bytes(&mut raw)
        .expect("the operating system random source");
    prefixed(prefix, &to_hex(&raw))
}

/// The admin token a server generates for itself when the operator names none.
pub fn generate_admin_token() -> String {
    generate_token(ADMIN_PREFIX)
}

fn digest(token: &str) -> [u8; 64] {
    let mut hasher = Blake2b512::new();
    hasher.update(token.as_bytes());
    hasher.finalize().into()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Compare without letting the time taken say where the first difference was.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_admin_token_is_the_admin_authority_and_nothing_else_is() {
        let store = TokenStore::new("s3cret");

        assert_eq!(store.authority_for("s3cret"), Some(Authority::Admin));
        assert_eq!(store.authority_for("s3cre"), None);
        assert_eq!(store.authority_for(""), None);
        assert_eq!(store.authority_for("s3cret "), None);
    }

    #[test]
    fn a_minted_token_carries_its_workspace_and_only_that_workspace() {
        let store = TokenStore::new("admin");
        let minted = store.mint("agent-a", DEFAULT_TOKEN_TTL);

        let authority = store
            .authority_for(&minted.token)
            .expect("the minted token resolves");
        assert_eq!(authority, Authority::Workspace("agent-a".to_string()));
        assert!(authority.may_act_for("agent-a"));
        assert!(!authority.may_act_for("agent-b"));
        assert!(!authority.is_admin());
    }

    #[test]
    fn two_mints_of_the_same_workspace_are_both_good() {
        let store = TokenStore::new("admin");
        let first = store.mint("agent-a", DEFAULT_TOKEN_TTL);
        let second = store.mint("agent-a", DEFAULT_TOKEN_TTL);
        assert!(store.authority_for(&first.token).is_some());
        assert!(store.authority_for(&second.token).is_some());
    }

    #[test]
    fn a_token_whose_ttl_has_passed_authorizes_nothing() {
        let store = TokenStore::new("admin");
        let minted = store.mint("agent-a", Duration::from_secs(0));
        assert_eq!(store.authority_for(&minted.token), None);
    }

    #[test]
    fn a_ttl_past_the_ceiling_is_clamped_rather_than_refused() {
        let store = TokenStore::new("admin");
        let minted = store.mint("agent-a", MAX_TOKEN_TTL * 4);
        assert_eq!(minted.ttl, MAX_TOKEN_TTL);
    }

    #[test]
    fn a_token_survives_the_server_that_issued_it() {
        let issuer = TokenStore::new("admin");
        let minted = issuer.mint("agent-a", DEFAULT_TOKEN_TTL);

        // Same admin token, new process: what a restart looks like from here.
        let restarted = TokenStore::new("admin");
        assert_eq!(
            restarted.authority_for(&minted.token),
            Some(Authority::Workspace("agent-a".to_string()))
        );

        // A different admin token is a different server, and rejects it.
        let stranger = TokenStore::new("another-admin");
        assert_eq!(stranger.authority_for(&minted.token), None);
    }

    #[test]
    fn rotation_retains_old_scoped_tokens_without_retaining_old_admin_authority() {
        let old = TokenStore::new("key-one");
        let old_scoped = old.mint("agent-a", DEFAULT_TOKEN_TTL);
        let rotated = TokenStore::with_retained("key-two", vec!["key-one".into()]);
        assert_eq!(
            rotated.authority_for(&old_scoped.token),
            Some(Authority::Workspace("agent-a".into()))
        );
        assert_eq!(rotated.authority_for("key-one"), None);
        assert_eq!(rotated.authority_for("key-two"), Some(Authority::Admin));
        let new_scoped = rotated.mint("agent-a", DEFAULT_TOKEN_TTL);
        assert_eq!(old.authority_for(&new_scoped.token), None);
    }

    #[test]
    fn a_token_edited_to_name_another_workspace_is_refused() {
        let store = TokenStore::new("admin");
        let minted = store.mint("agent-a", DEFAULT_TOKEN_TTL);

        // Rewrite the payload to say `agent-b`, keeping the tag: the tag is
        // over the payload, so it no longer matches.
        let body = behind_prefix(WORKSPACE_PREFIX, &minted.token).expect("the prefix");
        let (payload_hex, tag_hex) = body.split_once('_').expect("payload and tag");
        let payload = String::from_utf8(from_hex(payload_hex).expect("hex")).expect("utf8");
        let forged_payload = payload.replace("agent-a", "agent-b");
        let forged = prefixed(
            WORKSPACE_PREFIX,
            &format!("{}_{tag_hex}", to_hex(forged_payload.as_bytes())),
        );

        assert_eq!(store.authority_for(&forged), None);
    }

    #[test]
    fn a_token_edited_to_last_longer_is_refused() {
        let store = TokenStore::new("admin");
        let minted = store.mint("agent-a", Duration::from_secs(0));
        let body = behind_prefix(WORKSPACE_PREFIX, &minted.token).expect("the prefix");
        let (payload_hex, tag_hex) = body.split_once('_').expect("payload and tag");
        let payload = String::from_utf8(from_hex(payload_hex).expect("hex")).expect("utf8");
        let (workspace, _) = payload.rsplit_once(':').expect("an expiry");
        let forged = prefixed(
            WORKSPACE_PREFIX,
            &format!(
                "{}_{tag_hex}",
                to_hex(format!("{workspace}:{}", unix_now() + 9_000).as_bytes())
            ),
        );

        assert_eq!(store.authority_for(&forged), None);
    }

    #[test]
    fn a_token_that_is_not_one_is_refused_rather_than_panicking() {
        let store = TokenStore::new("admin");
        for nonsense in [
            "tdmw_",
            "tdmw_zz_zz",
            "tdmw_6161_6161",
            "tdmw__",
            "Bearer",
            "tdmw_3a3a_00",
        ] {
            assert_eq!(store.authority_for(nonsense), None, "for {nonsense}");
        }
    }

    #[test]
    fn a_generated_token_is_long_enough_to_be_worth_generating() {
        let token = generate_token(WORKSPACE_PREFIX);
        assert_eq!(
            behind_prefix(WORKSPACE_PREFIX, &token).map(str::len),
            Some(64)
        );
        assert_ne!(token, generate_token(WORKSPACE_PREFIX));
    }
}
