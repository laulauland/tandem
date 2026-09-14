//! Test-only recovery process for the VM replacement drill.

use std::io::{BufRead as _, Write as _};
use std::path::PathBuf;

use anyhow::{bail, Context as _, Result};

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--repo")) {
        bail!("usage: recovery_probe --repo PATH");
    }
    let repo = PathBuf::from(args.next().context("--repo needs PATH")?);
    if args.next().is_some() {
        bail!("recovery_probe accepts only --repo PATH");
    }
    let bucket = std::env::var("TANDEM_TEST_RECOVERY_BUCKET")
        .context("TANDEM_TEST_RECOVERY_BUCKET is required")?;
    let faults = jj_tandem_repository::FaultPoints::inert();
    faults.hold_next_replay_apply();
    let signal_faults = faults.clone();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let signal = std::thread::spawn(move || {
        signal_faults.wait_for_held_replay_apply();
        let _ = entered_tx.send(());
        println!(r#"{{"event":"replay_entry_applied"}}"#);
        std::io::stdout().flush().expect("flush held signal");
        let mut release = String::new();
        let read = std::io::stdin().lock().read_line(&mut release);
        if !matches!(read, Ok(bytes) if bytes > 0) {
            // systemd's default stdin is /dev/null. EOF must leave the gate
            // held for the supervisor to kill, rather than accidentally
            // turning the interruption drill into a completed recovery.
            loop {
                std::thread::park();
            }
        }
        signal_faults.release_replay_apply();
    });
    let settings = jj_tandem_test_support::test_settings()?;
    let result =
        jj_tandem_repository::Repository::new_with_faults(&settings, repo, Some(&bucket), faults);
    result?;
    entered_rx
        .try_recv()
        .context("recovery completed without applying a WAL entry")?;
    signal
        .join()
        .map_err(|_| anyhow::anyhow!("signal thread panicked"))?;
    Ok(())
}
