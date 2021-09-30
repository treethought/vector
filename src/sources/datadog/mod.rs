#[cfg(test)]
mod tests;

pub mod agent;
pub mod config;
pub mod logs;
pub mod metrics;
pub mod sketch_parser;
pub mod traces;

use crate::config::SourceDescription;
use crate::sources::datadog::config::DatadogAgentConfig;

inventory::submit! {
    SourceDescription::new::<DatadogAgentConfig>("datadog_agent")
}
