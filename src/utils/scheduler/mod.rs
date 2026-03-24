//! Task scheduler with DAG-based dependency resolution and bounded parallelism.

pub mod dag;
pub mod executor;

pub use dag::TaskGraph;
pub use executor::TaskScheduler;
