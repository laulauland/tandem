//! Deterministic simulation: generated schedules against every invariant.
//!
//! `pinned_seeds` runs the schedules that have failed
//! before, so a fixed bug stays fixed. `fresh_seeds` runs schedules nobody has
//! seen, so new bugs have somewhere to come from. Fixed
//! schedules that walk a named window point by point — the durability order,
//! and the two windows in which the server is holding objects no entry has
//! carried — because the generator reaches those by chance and "by chance" is
//! not a coverage claim.
//!
//! Generated schedules end with one publish to drain
//! whatever is staged, then the server's disk is thrown away and everything is
//! read back out of the bucket alone. See `schedule::close`.
//! Abandoned-publish regressions instead cold-restart immediately, without a
//! draining publish that could repair the missing durability being tested.
//!
//! When any of them fails it prints the seed, and that seed is the whole
//! reproducer: add it to the pinned list and the failure is a regression test.
//!
//! Each test fans its cases out over a few threads. A case is a whole cluster —
//! a server, its bucket and its agents — and cases share nothing, so running
//! them one after another only makes the suite slower.

use jj_tandem_test_support as support;

use support::schedule;

/// How many cases run at once inside one test. Each case is a server with its
/// own runtime, so this is a memory and scheduler budget, not a target.
const LANES: usize = 6;

/// Seeds worth keeping. Add the seed a failure printed, above the line that
/// says what it caught.
///
/// The set is chosen so that the rarer steps are in it rather than near it:
/// 1 and 2 cold-restart, 5 and 8 land an object between two attempts of a
/// retried publish, 6 and 9 make a bucket refuse a WAL entry. Those three steps
/// each come up on a few percent of rolls, so a pinned set that did not name
/// them would exercise them only sometimes — and the deterministic cases below
/// pin the invariants, not the schedules that reach them.
const PINNED_SEEDS: [u64; 14] = [1, 2, 3, 5, 6, 8, 9, 13, 21, 34, 55, 89, 144, 233];

/// An unindexed WAL is not replay ancestry. Another workspace must not depend
/// on its failed publisher ever retrying before its own bytes become durable.
#[test]
fn an_abandoned_publish_does_not_strand_another_workspaces_uploads() -> anyhow::Result<()> {
    abandoned_publish(false)
}

#[test]
fn an_index_write_failure_does_not_strand_another_workspaces_uploads() -> anyhow::Result<()> {
    abandoned_publish(true)
}

/// An operation file is the replay stop sentinel. Corrupt framing must be
/// rejected before writing it, or a second boot may skip unfinished ancestry.
#[test]
fn a_failed_ancestor_replay_cannot_be_skipped_on_the_next_boot() -> anyhow::Result<()> {
    use jj_tandem_wal::{IndexObject, RecordKind, WalEntry, WalRecord, INDEX_KEY};
    use support::{agent::Agent, cluster::Cluster, oracle};

    let mut cluster = Cluster::start()?;
    let mut agent = Agent::join(&cluster, "recovery")?;
    let ancestor_bytes = b"ancestor file survives repair\n".to_vec();
    let ancestor_commit = agent.commit_files(
        &[("ancestor.txt".to_string(), ancestor_bytes.clone())],
        "ancestor",
    )?;
    let ancestor = oracle::api_heads(&cluster)?
        .heads
        .into_iter()
        .next()
        .unwrap();
    let tip_bytes = b"tip file survives repair\n".to_vec();
    let tip_commit = agent.commit_files(&[("tip.txt".to_string(), tip_bytes.clone())], "tip")?;
    let index = IndexObject::decode(&std::fs::read(cluster.bucket.join(INDEX_KEY))?)?;
    assert!(
        !index.op_heads.contains(&ancestor),
        "corrupt an ancestor, not the tip"
    );
    let key = cluster.bucket.join(jj_tandem_wal::wal_key(&ancestor));
    let original = std::fs::read(&key)?;
    let mut corrupt = WalEntry::decode(&original)?;
    let operation = corrupt.records.pop().unwrap();
    assert_eq!(operation.kind, RecordKind::Operation);
    corrupt.records.insert(0, operation);
    corrupt.records.push(WalRecord {
        kind: RecordKind::File,
        id: vec![0; 20],
        data: b"does not hash to the recorded file id".to_vec(),
    });
    std::fs::write(&key, corrupt.encode()?)?;

    assert!(
        cluster.cold_restart().is_err(),
        "corrupt ancestry must fail cold recovery"
    );
    let metadata_path = cluster.repo.join(".jj/repo/tandem/heads.json");
    let metadata: serde_json::Value = serde_json::from_slice(&std::fs::read(&metadata_path)?)?;
    assert!(metadata["version"].as_u64().unwrap() < index.version);
    assert!(
        cluster.restart().is_err(),
        "a second boot must not skip an incompletely replayed ancestor"
    );
    let metadata: serde_json::Value = serde_json::from_slice(&std::fs::read(&metadata_path)?)?;
    assert!(metadata["version"].as_u64().unwrap() < index.version);

    std::fs::write(&key, original)?;
    cluster.restart()?;
    let snapshot = agent.snapshot()?;
    assert_eq!(
        snapshot.read_file(&ancestor_commit, "ancestor.txt")?,
        ancestor_bytes
    );
    assert_eq!(snapshot.read_file(&tip_commit, "tip.txt")?, tip_bytes);
    Ok(())
}

/// A cold materialization creates a disposable jj head before replay. If that
/// process dies after applying one WAL entry, the next process must still know
/// that the local initializer was never part of durable history.
#[test]
fn interrupted_cold_recovery_does_not_publish_its_initializer() -> anyhow::Result<()> {
    use support::{agent::Agent, cluster::Cluster, oracle};

    let mut cluster = Cluster::start()?;
    let mut agent = Agent::join(&cluster, "replacement")?;
    agent.commit_files(
        &[("before.txt".to_string(), b"before replacement\n".to_vec())],
        "before replacement",
    )?;

    cluster.stop();
    std::fs::remove_dir_all(&cluster.repo)?;
    std::fs::create_dir_all(&cluster.repo)?;
    jj_lib::workspace::Workspace::init_colocated_git(&support::test_settings()?, &cluster.repo)?;
    std::fs::write(cluster.repo.join(".tandem-initializing"), b"initializing\n")?;
    cluster.restart()?;
    assert!(
        !cluster.repo.join(".tandem-initializing").exists(),
        "a recognized interrupted initialization must be replaced completely"
    );

    cluster.stop();
    std::fs::remove_dir_all(&cluster.repo)?;
    std::fs::create_dir_all(&cluster.repo)?;
    cluster.faults.fail_after_replay_apply(1);
    assert!(
        cluster.restart().is_err(),
        "the first cold recovery must stop after applying one WAL entry"
    );
    assert!(
        cluster
            .repo
            .join(".jj/repo/tandem/bootstrap-heads.json")
            .is_file(),
        "the disposable initializer identity must survive the interrupted process"
    );

    cluster.restart()?;
    agent.commit_files(
        &[("after.bin".to_string(), b"after replacement\0\xff".to_vec())],
        "after replacement",
    )?;
    cluster.cold_restart()?;
    oracle::check(
        &cluster,
        &[agent],
        "interrupted recovery and replacement publish",
    )?;
    Ok(())
}

#[test]
fn an_existing_head_marker_cannot_hide_a_missing_indexed_operation() -> anyhow::Result<()> {
    use jj_tandem_wal::{IndexObject, INDEX_KEY};
    use support::{agent::Agent, cluster::Cluster};

    let mut cluster = Cluster::start()?;
    let mut agent = Agent::join(&cluster, "missing-head")?;
    let content = b"recover after restoring the indexed operation\n".to_vec();
    let commit = agent.commit_files(&[("file.txt".to_string(), content.clone())], "published")?;
    cluster.stop();
    let index_path = cluster.bucket.join(INDEX_KEY);
    let mut index = IndexObject::decode(&std::fs::read(&index_path)?)?;
    let operation_path = cluster
        .repo
        .join(".jj/repo/op_store/operations")
        .join(&index.op_heads[0]);
    let operation_bytes = std::fs::read(&operation_path)?;
    std::fs::remove_file(&operation_path)?;
    index.version += 1;
    std::fs::write(&index_path, index.encode()?)?;

    assert!(
        cluster.restart().is_err(),
        "head markers must not bypass materialization checks"
    );
    let metadata: serde_json::Value = serde_json::from_slice(&std::fs::read(
        cluster.repo.join(".jj/repo/tandem/heads.json"),
    )?)?;
    assert!(metadata["version"].as_u64().unwrap() < index.version);
    std::fs::write(&operation_path, operation_bytes)?;
    cluster.restart()?;
    assert_eq!(agent.snapshot()?.read_file(&commit, "file.txt")?, content);
    Ok(())
}

#[test]
fn malformed_indexed_heads_cannot_replace_the_repository() -> anyhow::Result<()> {
    use jj_tandem_wal::{IndexObject, INDEX_KEY};
    use support::{agent::Agent, cluster::Cluster};

    let mut cluster = Cluster::start()?;
    let mut agent = Agent::join(&cluster, "root-validation")?;
    let content = b"malformed root aliases must not replace this history\n".to_vec();
    let commit = agent.commit_files(&[("file.txt".to_string(), content.clone())], "published")?;
    cluster.stop();
    let index_path = cluster.bucket.join(INDEX_KEY);
    let original = std::fs::read(&index_path)?;
    let mut index = IndexObject::decode(&original)?;
    index.version += 1;
    for invalid in [vec!["0".to_string()], vec!["0000".to_string()], Vec::new()] {
        index.op_heads = invalid;
        std::fs::write(&index_path, index.encode()?)?;
        assert!(
            cluster.restart().is_err(),
            "invalid heads were adopted: {:?}",
            index.op_heads
        );
        let metadata: serde_json::Value = serde_json::from_slice(&std::fs::read(
            cluster.repo.join(".jj/repo/tandem/heads.json"),
        )?)?;
        assert!(metadata["version"].as_u64().unwrap() < index.version);
    }
    std::fs::write(&index_path, original)?;
    cluster.restart()?;
    assert_eq!(agent.snapshot()?.read_file(&commit, "file.txt")?, content);
    Ok(())
}

#[test]
fn the_full_root_operation_materializes_without_a_wal_entry() -> anyhow::Result<()> {
    use jj_tandem_wal::{IndexObject, INDEX_KEY};
    use support::{agent::Agent, cluster::Cluster, oracle};

    let mut cluster = Cluster::start()?;
    cluster.stop();
    let index_path = cluster.bucket.join(INDEX_KEY);
    let mut index = IndexObject::decode(&std::fs::read(&index_path)?)?;
    let root = "00".repeat(64);
    index.version += 1;
    index.op_heads = vec![root.clone()];
    index.workspace_heads.clear();
    std::fs::write(&index_path, index.encode()?)?;
    assert!(!cluster.bucket.join(jj_tandem_wal::wal_key(&root)).exists());

    cluster.cold_restart()?;
    let heads = oracle::api_heads(&cluster)?;
    assert_eq!(heads.version, index.version);
    assert_eq!(heads.heads, std::collections::BTreeSet::from([root]));
    let mut agent = Agent::join(&cluster, "from-root")?;
    let content = b"new work after recovering the synthetic root\n".to_vec();
    let commit = agent.commit_files(&[("file.txt".to_string(), content.clone())], "published")?;
    cluster.cold_restart()?;
    assert_eq!(agent.snapshot()?.read_file(&commit, "file.txt")?, content);
    Ok(())
}

fn abandoned_publish(index_write_failure: bool) -> anyhow::Result<()> {
    use support::agent::Agent;
    use support::cluster::Cluster;

    let mut cluster = Cluster::start()?;
    let abandoned = Agent::join(&cluster, "abandoned")?;
    let survivor = Agent::join(&cluster, "survivor")?;
    let contents = b"survives an abandoned WAL and an immediate cold restart\n".to_vec();
    let (surviving_op, _, commit) = survivor.prepare_files(
        &[("survives.txt".to_string(), contents.clone())],
        "surviving work",
    )?;
    let (abandoned_op, _, _) = abandoned.prepare_files(&[], "abandoned work")?;

    if index_write_failure {
        cluster.faults.fail_index_writes(1);
        let err = abandoned
            .publish_once(&cluster, &abandoned_op)
            .expect_err("index write must fail");
        assert!(
            err.to_string().contains("injected bucket failure"),
            "{err:#}"
        );
    } else {
        cluster.faults.set_index_cas_conflicts(1);
        assert!(!abandoned.publish_once(&cluster, &abandoned_op)?);
    }
    abandoned_op.leave_unpublished();
    assert!(survivor.publish_once(&cluster, &surviving_op)?);
    surviving_op.leave_unpublished();

    // No final draining publish or reupload: those would hide this failure.
    cluster.cold_restart()?;
    assert_eq!(
        survivor.snapshot()?.read_file(&commit, "survives.txt")?,
        contents
    );
    Ok(())
}

#[test]
fn pinned_seeds() {
    in_parallel(PINNED_SEEDS.to_vec(), run_seed);
}

#[test]
fn fresh_seeds() {
    let count = std::env::var("TANDEM_DST_CASES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(6);
    let base = std::env::var("TANDEM_DST_SEED")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or_else(support::rng::arbitrary_seed);

    eprintln!("dst: {count} fresh schedules from base seed {base}");
    let seeds: Vec<u64> = (0..count as u64)
        .map(|step| base.wrapping_add(step.wrapping_mul(0x9E37_79B9_7F4A_7C15)))
        .collect();
    in_parallel(seeds, run_seed);
}

/// Every crash window, every run.
#[test]
fn crash_windows() {
    use jj_tandem_repository::CrashWindow;

    in_parallel(CrashWindow::ALL.to_vec(), |window| {
        use support::schedule::Step;

        let schedule = schedule::Schedule {
            seed: 0,
            agent_count: 2,
            steps: vec![
                Step::Commit {
                    agent: 0,
                    files: vec![("a.txt".to_string(), b"before the crash\n".to_vec())],
                    description: "before".to_string(),
                },
                Step::ArmCrash(window),
                Step::Commit {
                    agent: 1,
                    files: vec![("b.txt".to_string(), b"during the crash\n".to_vec())],
                    description: "during".to_string(),
                },
                Step::Commit {
                    agent: 0,
                    files: vec![("dir/c.txt".to_string(), b"after the crash\n".to_vec())],
                    description: "after".to_string(),
                },
            ],
        };
        if let Err(err) = schedule::run(&schedule) {
            panic!(
                "the {} crash window did not recover:\n{err:#}",
                window.as_str()
            );
        }
    });
}

/// An object that lands between two attempts of a retried publish, every run.
///
/// The generator reaches this by chance, and "by chance" is not a coverage
/// claim for the one window in which a drained staging buffer can lose an
/// object that nothing else will ever write again. The publish that conflicts
/// cannot carry it — its WAL entry is already in the bucket and immutable — so
/// the next one has to, and `schedule::run`'s closing phase is what asks.
#[test]
fn objects_staged_between_retries_reach_the_wal() {
    use support::rng::Rng;
    use support::schedule::{staged_object, Schedule, Step};

    let mut rng = Rng::new(0xB10B);
    let schedule = Schedule {
        seed: 0,
        agent_count: 2,
        steps: vec![
            Step::Commit {
                agent: 0,
                files: vec![("a.txt".to_string(), b"before the contention\n".to_vec())],
                description: "before".to_string(),
            },
            Step::StageObjectOnConflict {
                bytes: staged_object(&mut rng, 1),
            },
            Step::ArmCasConflicts { count: 1 },
            Step::Commit {
                agent: 1,
                files: vec![("b.txt".to_string(), b"through the contention\n".to_vec())],
                description: "through contention".to_string(),
            },
        ],
    };
    if let Err(err) = schedule::run(&schedule) {
        panic!("an object staged between two attempts of a retried publish was lost:\n{err:#}");
    }
}

/// A publish drains the staging buffer before it writes, so a write that then
/// fails is holding every object the client wrote for it. Restaging is the only
/// thing that keeps them, and nothing else in the suite makes that write fail.
#[test]
fn objects_drained_by_a_failed_wal_write_reach_the_wal() {
    use support::schedule::{Schedule, Step};

    // Distinctive content, so finding it in an entry means finding it.
    let contended = b"tandem-object-drained-by-a-failed-wal-write\n".to_vec();
    let schedule = Schedule {
        seed: 0,
        agent_count: 2,
        steps: vec![
            Step::Commit {
                agent: 0,
                files: vec![("a.txt".to_string(), b"before the outage\n".to_vec())],
                description: "before".to_string(),
            },
            Step::FailWalWrites { count: 1 },
            Step::Commit {
                agent: 1,
                files: vec![("b.txt".to_string(), contended)],
                description: "during the outage".to_string(),
            },
            Step::FailWalWrites { count: 0 },
        ],
    };
    if let Err(err) = schedule::run(&schedule) {
        panic!("an object drained by a publish whose WAL write failed was lost:\n{err:#}");
    }
}

/// Run every case, a few at a time, and fail the test if any of them failed.
///
/// A panic inside a worker thread would otherwise only print; `join` returning
/// an error is what turns it back into a failing test.
fn in_parallel<T, F>(cases: Vec<T>, run: F)
where
    T: Send + 'static,
    F: Fn(T) + Copy + Send + 'static,
{
    let lanes: Vec<Vec<T>> = {
        let mut lanes: Vec<Vec<T>> = (0..LANES.min(cases.len().max(1)))
            .map(|_| Vec::new())
            .collect();
        for (index, case) in cases.into_iter().enumerate() {
            let lane = index % lanes.len();
            lanes[lane].push(case);
        }
        lanes
    };

    let handles: Vec<_> = lanes
        .into_iter()
        .map(|lane| {
            std::thread::spawn(move || {
                for case in lane {
                    run(case);
                }
            })
        })
        .collect();

    let mut failed = false;
    for handle in handles {
        if handle.join().is_err() {
            failed = true;
        }
    }
    assert!(!failed, "a simulation case failed; its panic is above");
}

fn run_seed(seed: u64) {
    let schedule = schedule::generate(seed);
    if let Err(err) = schedule::run(&schedule) {
        let steps: Vec<String> = schedule
            .steps
            .iter()
            .enumerate()
            .map(|(index, step)| format!("    {index}: {}", step.label()))
            .collect();
        panic!(
            "simulation failed.\n\
             Reproduce with: TANDEM_DST_SEED={seed} TANDEM_DST_CASES=1 cargo test --test dst fresh_seeds\n\
             Or pin it: add {seed} to PINNED_SEEDS in tests/dst.rs\n\
             Schedule ({} agents):\n{}\n\n{err:#}",
            schedule.agent_count,
            steps.join("\n")
        );
    }
}
