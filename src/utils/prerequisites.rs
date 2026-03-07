//! Task prerequisite checking for avocado CLI commands.
//!
//! This module provides a [`TaskPrerequisites`] trait and a [`check_prerequisites`]
//! function that allow commands to declare their required stamps and have them
//! validated before execution. This is used for commands that run outside the SDK
//! container (e.g. `avocado connect upload`) but still need to verify that
//! prerequisite tasks (e.g. `avocado build`) have been completed.

use anyhow::{Context, Result};

use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::stamps::{
    generate_batch_read_stamps_script, validate_stamps_batch, StampRequirement,
};

/// A command that has prerequisite stamps that must be satisfied before it can run.
pub trait TaskPrerequisites {
    /// Returns the list of stamps that must be present before this task runs.
    fn required_stamps(&self) -> Vec<StampRequirement>;

    /// A human-readable description used in error messages, e.g. `"Cannot upload runtime 'dev'"`.
    fn task_description(&self) -> String;
}

/// Validate all required stamps for `task` by running a batch stamp read inside
/// the SDK container directly via [`SdkContainer::run_in_container_with_output`].
///
/// Calls [`std::process::exit`] (via [`StampValidationError::print_and_exit`]) if
/// any required stamps are missing or stale, so callers do not need to handle the
/// error case — a clear, user-facing message is printed first.
///
/// Returns `Ok(())` if all prerequisites are satisfied.
pub async fn check_prerequisites<T: TaskPrerequisites>(
    task: &T,
    target: &str,
    container: &SdkContainer,
    container_image: &str,
) -> Result<()> {
    let requirements = task.required_stamps();
    if requirements.is_empty() {
        return Ok(());
    }

    let batch_script = generate_batch_read_stamps_script(&requirements);

    let run_config = RunConfig {
        container_image: container_image.to_string(),
        target: target.to_string(),
        command: batch_script,
        source_environment: true,
        interactive: false,
        ..Default::default()
    };

    let stdout = container
        .run_in_container_with_output(run_config)
        .await
        .context("Failed to run prerequisite stamp check")?
        .unwrap_or_default();

    let validation = validate_stamps_batch(&requirements, &stdout, None);

    if !validation.is_satisfied() {
        validation
            .into_error(&task.task_description())
            .print_and_exit();
    }

    Ok(())
}
