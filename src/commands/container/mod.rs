//! `avocado container` subcommands.
//!
//! Top-level noun for Container Dev Mode. v1 exposes only the `dev` command
//! family (`up`/`sync`/`status`/`down`/`prune`); dev-to-prod graduation is
//! deliberately out of scope for v1 (see the design doc).

pub mod dev;
