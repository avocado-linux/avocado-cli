//! `avocado vm stop` — graceful shutdown, falls back to SIGKILL.

use anyhow::Result;

use crate::utils::output::{print_success, OutputLevel};
use crate::utils::vm::lifecycle;

pub struct StopCommand {
    pub force: bool,
}

impl StopCommand {
    pub async fn execute(self) -> Result<()> {
        lifecycle::stop(self.force).await?;
        print_success("avocado-vm stopped.", OutputLevel::Normal);
        Ok(())
    }
}
