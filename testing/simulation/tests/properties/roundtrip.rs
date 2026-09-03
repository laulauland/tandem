//! Bytes in, bytes out.
//!
//! The oldest promise tandem makes: what a client writes, a client reads back,
//! and the server hands out the same thing. It is easy to hold for one file in
//! one commit, which is what a hand-written test checks. It is harder across a
//! generated set of files cut into a generated set of commits — which is where
//! a tree walk that loses an entry, or a batch that drops its tail, shows up.

use std::collections::BTreeMap;

use proptest::prelude::*;

use crate::support::{agent::Agent, cluster::Cluster, oracle};

/// The paths a case may use. Fixed set, generated assignment: what matters is
/// that later commits overwrite earlier ones at the same path.
const PATHS: [&str; 6] = [
    "a.txt",
    "b.bin",
    "dir/c.txt",
    "dir/nested/d",
    "README.md",
    "dir/nested/deeper/e.dat",
];

/// One commit's worth of files.
fn commit_contents() -> impl Strategy<Value = Vec<(usize, Vec<u8>)>> {
    proptest::collection::vec(
        (
            0..PATHS.len(),
            prop_oneof![
                1 => Just(Vec::new()),
                6 => proptest::collection::vec(any::<u8>(), 1..600),
                2 => proptest::collection::vec(any::<u8>(), 600..9_000),
                1 => proptest::collection::vec(any::<u8>(), 9_000..70_000),
            ],
        ),
        1..4,
    )
}

fn history() -> impl Strategy<Value = Vec<Vec<(usize, Vec<u8>)>>> {
    proptest::collection::vec(commit_contents(), 1..4)
}

proptest! {
    // Each case boots a server and a workspace, so the case count is chosen
    // for what it costs, not for what proptest defaults to.
    #![proptest_config(ProptestConfig { cases: 8, max_shrink_iters: 32, ..ProptestConfig::default() })]

    #[test]
    fn files_read_back_byte_identical_from_client_and_server(history in history()) {
        run_case(history).expect("round trip");
    }
}

fn run_case(history: Vec<Vec<(usize, Vec<u8>)>>) -> anyhow::Result<()> {
    let cluster = Cluster::start()?;
    let mut agent = Agent::join(&cluster, "writer")?;
    let reader = Agent::join(&cluster, "reader")?;

    // What each commit should hold at each path, once the commit is written.
    let mut committed: Vec<(jj_lib::backend::CommitId, BTreeMap<String, Vec<u8>>)> = Vec::new();
    let mut running: BTreeMap<String, Vec<u8>> = BTreeMap::new();

    for (number, contents) in history.into_iter().enumerate() {
        let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for (index, bytes) in contents {
            files.insert(PATHS[index].to_string(), bytes);
        }
        let as_vec: Vec<(String, Vec<u8>)> = files.clone().into_iter().collect();
        let commit = agent.commit_files(&as_vec, &format!("commit {number}"))?;
        // A commit carries everything before it too — that is the boundary
        // worth crossing, and the one a tree walk gets wrong.
        running.extend(files);
        committed.push((commit, running.clone()));
    }

    // Read back through a client that has never seen any of these objects.
    let reader_snapshot = reader.snapshot()?;
    let writer_snapshot = agent.snapshot()?;
    for (commit, expected) in &committed {
        for (path, bytes) in expected {
            let from_writer = writer_snapshot.read_file(commit, path)?;
            anyhow::ensure!(
                from_writer == *bytes,
                "the writer read {} bytes of {path}; it wrote {}",
                from_writer.len(),
                bytes.len()
            );

            let from_reader = reader_snapshot.read_file(commit, path)?;
            anyhow::ensure!(
                from_reader == *bytes,
                "the reader read {} bytes of {path}; the writer wrote {}",
                from_reader.len(),
                bytes.len()
            );

            // And from the server itself, with no client in the way at all.
            let id = writer_snapshot.file_id(commit, path)?;
            let from_server = oracle::api_object(&cluster, "file", &id)?;
            anyhow::ensure!(
                from_server == *bytes,
                "the server served {} bytes for {path}; the writer wrote {}",
                from_server.len(),
                bytes.len()
            );
        }
    }

    // Nothing above should have broken any invariant either.
    let agents = [agent, reader];
    oracle::check(&cluster, &agents, "the round-trip case")?;
    Ok(())
}
