//! `avocado container dev` subcommands.
//!
//! Thin dispatch stubs at this stage. The `up`/`down`/`status` orchestration
//! lands in a later task, and `sync`/`prune` are defined alongside it. Each
//! handler currently returns a not-yet-implemented error so the command tree,
//! `--help`, and completion wiring can be exercised before the host-side
//! registry and engine-driver watcher exist.

use anyhow::{bail, Result};

pub struct DevUpCommand;
pub struct DevSyncCommand;
pub struct DevStatusCommand;
pub struct DevDownCommand;
pub struct DevPruneCommand;

impl DevUpCommand {
    pub async fn execute(self) -> Result<()> {
        bail!("`avocado container dev up` is not implemented yet")
    }
}

impl DevSyncCommand {
    pub async fn execute(self) -> Result<()> {
        bail!("`avocado container dev sync` is not implemented yet")
    }
}

impl DevStatusCommand {
    pub async fn execute(self) -> Result<()> {
        bail!("`avocado container dev status` is not implemented yet")
    }
}

impl DevDownCommand {
    pub async fn execute(self) -> Result<()> {
        bail!("`avocado container dev down` is not implemented yet")
    }
}

impl DevPruneCommand {
    pub async fn execute(self) -> Result<()> {
        bail!("`avocado container dev prune` is not implemented yet")
    }
}
