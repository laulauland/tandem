//! What must be true after every step, whatever the schedule did.
//!
//! These are the invariants of the design doc, written as one function so that
//! a generated schedule cannot pass by doing something nobody thought to
//! assert. Each check names the property it stands for, because the failure
//! message is the only documentation a reader gets at three in the morning.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Context, Result};
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::OperationId;
use jj_lib::repo::{RepoLoader, StoreFactories};
use jj_tandem_protocol::hex::{from_hex, to_hex};
use jj_tandem_wal as wal;
use pollster::FutureExt as _;

use super::agent::{Agent, Snapshot};
use super::cluster::Cluster;

/// What the API says the head set is.
#[derive(Debug, Clone)]
pub struct ApiHeads {
    pub version: u64,
    pub heads: BTreeSet<String>,
    pub workspace_heads: BTreeMap<String, String>,
}

pub fn api_heads(cluster: &Cluster) -> Result<ApiHeads> {
    let body: jj_tandem_protocol::wire::HeadsBody = http_client()?
        .get(format!("{}/api/heads", cluster.base_url()))
        .bearer_auth(&cluster.admin_token)
        .send()
        .context("GET /api/heads")?
        .error_for_status()
        .context("GET /api/heads")?
        .json()
        .context("decode /api/heads")?;
    Ok(ApiHeads {
        version: body.version,
        heads: body.heads.into_iter().collect(),
        workspace_heads: body.workspace_heads,
    })
}

/// Fetch one stored object straight from the server, bypassing every client.
pub fn api_object(cluster: &Cluster, kind: &str, id_hex: &str) -> Result<Vec<u8>> {
    let response = http_client()?
        .get(format!(
            "{}/api/objects/{kind}/{id_hex}",
            cluster.base_url()
        ))
        .bearer_auth(&cluster.admin_token)
        .send()
        .context("GET /api/objects")?
        .error_for_status()
        .with_context(|| format!("GET /api/objects/{kind}/{id_hex}"))?;
    Ok(response.bytes().context("read object body")?.to_vec())
}

fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("build the oracle's http client")
}

/// The tandem metadata sidecar the server keeps next to its repo.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Sidecar {
    version: u64,
    #[serde(default)]
    workspace_heads: BTreeMap<String, String>,
}

fn sidecar(cluster: &Cluster) -> Result<Option<Sidecar>> {
    let path = cluster.repo.join(".jj/repo/tandem/heads.json");
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).context("decode the tandem metadata sidecar")?,
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).context("read the tandem metadata sidecar"),
    }
}

fn index(cluster: &Cluster) -> Result<Option<wal::IndexObject>> {
    let path = cluster.bucket.join("index/heads.json");
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).context("decode the bucket index")?,
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).context("read the bucket index"),
    }
}

/// Assert that some WAL entry carries these bytes.
///
/// The oracle's index/WAL check asks whether every head has an entry. That says
/// nothing about what is *inside* an entry, and one object has no head of its
/// own: an object staged between two attempts of a retried publish belongs to
/// whichever publish comes next, and if that publish does not carry it, it is
/// durable nowhere while the head that reaches it is acknowledged. Nothing but
/// reading the entries finds that.
pub fn assert_bytes_in_some_wal_entry(cluster: &Cluster, bytes: &[u8], what: &str) -> Result<()> {
    let dir = cluster.bucket.join("wal");
    let mut names = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            bail!("{what} is in no WAL entry: the bucket has no WAL directory at all")
        }
        Err(err) => return Err(err).context("list the bucket's WAL directory"),
    };
    for entry in entries {
        let entry = entry.context("read a WAL directory entry")?;
        let raw = std::fs::read(entry.path()).context("read a WAL entry")?;
        if contains(&raw, bytes) {
            return Ok(());
        }
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    bail!(
        "{what} ({} bytes) is in none of the {} WAL entries, so it is durable nowhere: {names:?}",
        bytes.len(),
        names.len()
    )
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && needle.len() <= haystack.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// Assert every invariant. `context` names the step that led here, so a
/// failure says which one, not just that one of them did.
pub fn check(cluster: &Cluster, agents: &[Agent], context: &str) -> Result<()> {
    check_inner(cluster, agents).with_context(|| format!("oracle failed after {context}"))
}

/// Read every agent until they all say the same thing.
///
/// A read is not passive here: loading a repo whose heads have diverged is how
/// jj merges them, so the first read after somebody else published does work
/// before it answers. Convergence therefore means "settles within a bounded
/// number of reads", and the bound is the assertion — not a sleep, and not a
/// retry loop that gives up quietly.
fn converge(agents: &[Agent]) -> Result<Vec<Snapshot>> {
    const ROUNDS: usize = 8;
    if agents.is_empty() {
        return Ok(Vec::new());
    }
    let mut snapshots = Vec::new();
    for _ in 0..ROUNDS {
        snapshots = agents
            .iter()
            .map(Agent::snapshot)
            .collect::<Result<Vec<_>>>()?;
        let agreed = snapshots.windows(2).all(|pair| {
            pair[0].view_heads == pair[1].view_heads
                && pair[0].working_copies == pair[1].working_copies
        });
        if agreed {
            return Ok(snapshots);
        }
    }
    let report: Vec<String> = snapshots
        .iter()
        .map(|snapshot| {
            format!(
                "  {}: heads {:?} working copies {:?}",
                snapshot.name, snapshot.view_heads, snapshot.working_copies
            )
        })
        .collect();
    bail!(
        "agents did not converge after {ROUNDS} reads:\n{}",
        report.join("\n")
    )
}

fn check_inner(cluster: &Cluster, agents: &[Agent]) -> Result<()> {
    // 1. Every agent converges on the same view, and none of them falls out of
    //    it: a workspace with no working-copy commit is a workspace the shared
    //    view has forgotten.
    let snapshots = converge(agents)?;
    if let Some(first) = snapshots.first() {
        for agent in agents {
            if !first.working_copies.contains_key(&agent.name) {
                bail!(
                    "the converged view has no working copy for {}: {:?}",
                    agent.name,
                    first.working_copies
                );
            }
        }
    }

    let api = api_heads(cluster)?;

    // 2. The API is a view of jj's op-heads store, not a parallel record.
    let settings = super::test_settings()?;
    let repo_dir = dunce::canonicalize(cluster.repo.join(".jj/repo"))
        .context("canonicalize the server's .jj/repo")?;
    let loader =
        RepoLoader::init_from_file_system(&settings, &repo_dir, &StoreFactories::default())
            .context("load the server's repo")?;
    let jj_heads: BTreeSet<String> = loader
        .op_heads_store()
        .get_op_heads()
        .block_on()
        .context("read the server's op heads")?
        .into_iter()
        .map(|id| id.hex())
        .collect();
    if jj_heads != api.heads {
        bail!(
            "the API head set and the server's jj op heads disagree:\n  jj:  {jj_heads:?}\n  api: {:?}",
            api.heads
        );
    }

    // 3. No phantom heads: every head the API names resolves to an operation
    //    the server can actually load.
    for head in &api.heads {
        assert_operation_stored(&loader, head, "the API")?;
    }

    // 4. Every head named by the durable index has a WAL entry behind it. A
    //    head with no entry is a head nobody can replay.
    if let Some(index) = index(cluster)? {
        for head in &index.op_heads {
            let entry = cluster.bucket.join(format!("wal/{head}"));
            if !entry.exists() {
                bail!("the index names head {head} but the bucket has no WAL entry for it");
            }
            let bytes = std::fs::read(&entry).context("read the WAL entry")?;
            let decoded = wal::WalEntry::decode(&bytes)
                .with_context(|| format!("the WAL entry for {head} does not decode"))?;
            let decoded_id = to_hex(&decoded.op_id);
            if decoded_id != *head {
                bail!("the WAL entry at wal/{head} carries op id {decoded_id}");
            }
        }

        // 5. The materialized copy never claims to be ahead of the durable
        //    one. The other direction is allowed: that is what a crash between
        //    the index write and the local apply leaves behind, and what the
        //    next start repairs.
        if let Some(sidecar) = sidecar(cluster)? {
            if sidecar.version > index.version {
                bail!(
                    "local version {} is ahead of index version {}",
                    sidecar.version,
                    index.version
                );
            }
            if sidecar.workspace_heads != api.workspace_heads {
                bail!(
                    "the sidecar and the API disagree about workspace heads:\n  sidecar: {:?}\n  api:     {:?}",
                    sidecar.workspace_heads,
                    api.workspace_heads
                );
            }
        }
    }

    // 6. No phantom heads on the client side either: every head an agent acts
    //    on resolves to an operation the server holds. An agent's set is not
    //    the server's global set — it also carries that agent's own workspace
    //    head — but every member of it must be a real, stored operation.
    for snapshot in &snapshots {
        for head in &snapshot.op_heads {
            assert_operation_stored(&loader, head, &snapshot.name)?;
        }
    }

    // 7. No divergent change ids: one change, one commit — from every agent's
    //    point of view, since divergence is something a client would see.
    for snapshot in &snapshots {
        for (change, commits) in snapshot.reachable_commits()? {
            if commits.len() > 1 {
                bail!(
                    "{} sees change {change} on more than one commit: {commits:?}",
                    snapshot.name
                );
            }
        }
    }

    // 8. Every byte every agent wrote reads back, from every agent, over the
    //    wire, out of a store that has not seen it before.
    for writer in agents {
        for (commit, files) in writer.expected_files() {
            for (path, bytes) in files {
                for reader in &snapshots {
                    let read = reader.read_file(&commit, &path).with_context(|| {
                        format!(
                            "{} cannot read {path} from {} (written by {})",
                            reader.name,
                            commit.hex(),
                            writer.name
                        )
                    })?;
                    if read != bytes {
                        bail!(
                            "{} read {} bytes of {path} from {}; {} wrote {} bytes",
                            reader.name,
                            read.len(),
                            commit.hex(),
                            writer.name,
                            bytes.len()
                        );
                    }
                }
            }
        }
    }

    Ok(())
}

/// A head must name an operation the server can load. Anything else is a
/// phantom: a name with nothing behind it, which is what a client following it
/// would discover the hard way. `owner` says who is holding the name.
fn assert_operation_stored(loader: &RepoLoader, head: &str, owner: &str) -> Result<()> {
    let id = OperationId::new(
        from_hex(head).with_context(|| format!("{owner} holds head {head}, which is not hex"))?,
    );
    loader
        .op_store()
        .read_operation(&id)
        .block_on()
        .with_context(|| format!("{owner} holds head {head}, which is not stored"))?;
    Ok(())
}
