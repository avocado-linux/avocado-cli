//! Task prerequisite checking for avocado CLI commands.
//!
//! This module provides a [`TaskPrerequisites`] trait and a [`check_prerequisites`]
//! function that allow commands to declare their required stamps and have them
//! validated before execution. This is used for commands that run outside the SDK
//! container (e.g. `avocado connect upload`) but still need to verify that
//! prerequisite tasks (e.g. `avocado build`) have been completed.

use anyhow::{Context, Result};

use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::runs_on::RunsOnContext;
use crate::utils::stamps::{
    generate_batch_read_stamps_script, parse_batch_stamps_output, validate_stamps_parsed,
    CurrentInput, Stamp, StampRequirement, StampValidationResult,
};

/// Stamps read from the SDK container in a single invocation, keyed by
/// relative stamp path.
pub struct StampBatch {
    stamps: std::collections::HashMap<String, Option<String>>,
}

impl StampBatch {
    /// The stamp recorded for `req`, or `None` when it is absent or its JSON
    /// can't be parsed. An unparseable stamp is treated as missing on
    /// purpose: it was written by a different (or corrupted) CLI and its
    /// hashes can't be trusted for a skip decision.
    pub fn stamp_for(&self, req: &StampRequirement) -> Option<Stamp> {
        self.stamps
            .get(&req.relative_path())?
            .as_deref()
            .and_then(|json| Stamp::from_json(json).ok())
    }

    /// Validate `requirements` against freshly computed inputs. See
    /// [`validate_stamps_parsed`] for how requirements are matched to inputs.
    pub fn validate(
        &self,
        requirements: &[StampRequirement],
        current_inputs: &[CurrentInput<'_>],
    ) -> StampValidationResult {
        validate_stamps_parsed(requirements, &self.stamps, current_inputs)
    }
}

/// Read every stamp in `requirements` in one container invocation.
///
/// Stamps live in the `/opt/_avocado` named volume rather than a host bind
/// mount, so reading them always costs a container run — hence batching.
///
/// `base_run_config` supplies the caller's container settings (image,
/// target, repo, container args, arch, …); its `command`, `source_environment`
/// and `interactive` fields are overwritten. Pass `runs_on_context` to read
/// from a remote host set up with `--runs-on`.
pub async fn read_stamps_batch(
    requirements: &[StampRequirement],
    container: &SdkContainer,
    base_run_config: RunConfig,
    runs_on_context: Option<&RunsOnContext>,
) -> Result<StampBatch> {
    let run_config = RunConfig {
        command: generate_batch_read_stamps_script(requirements),
        source_environment: true,
        interactive: false,
        ..base_run_config
    };

    let raw = if let Some(context) = runs_on_context {
        container
            .run_in_container_with_output_remote(&run_config, context)
            .await
            .context("Failed to read stamps in remote SDK container")?
    } else {
        container
            .run_in_container_with_output(run_config)
            .await
            .context("Failed to read stamps in SDK container")?
    }
    .unwrap_or_default();

    Ok(StampBatch {
        stamps: parse_batch_stamps_output(&raw),
    })
}

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

    let batch = read_stamps_batch(
        &requirements,
        container,
        RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            ..Default::default()
        },
        None,
    )
    .await?;

    let validation = batch.validate(&requirements, &[]);

    if !validation.is_satisfied() {
        validation
            .into_error(&task.task_description())
            .print_and_exit();
    }

    Ok(())
}
