//! CVE correlation commands.
//!
//! The Yocto side publishes one JSON per machine mapping every runtime package to the recipe that
//! built it and that recipe's unpatched CVEs. These commands join that document with what is
//! actually installed in a project's sysroots.

pub mod report;
