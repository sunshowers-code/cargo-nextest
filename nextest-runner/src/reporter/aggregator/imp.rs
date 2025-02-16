// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::junit::MetadataJunit;
use crate::{
    config::EvaluatableProfile,
    errors::{JunitSetupError, WriteEventError},
    reporter::events::TestEvent,
};
use camino::Utf8Path;

/// Aggregator for test events.
///
/// Currently, this aggregator supports JUnit XML output.
#[derive(Clone, Debug)]
pub struct EventAggregator<'cfg> {
    // TODO: log information in a JSONable report (converting that to XML later) instead of directly
    // writing it to XML
    junit: Option<MetadataJunit<'cfg>>,
}

impl<'cfg> EventAggregator<'cfg> {
    /// Creates a new `EventAggregator`.
    pub fn new(
        profile: &EvaluatableProfile<'cfg>,
        target_dir: &Utf8Path,
    ) -> Result<Self, JunitSetupError> {
        let junit = profile
            .junit()
            .map(|config| {
                let store_dir = profile.store_dir(target_dir).to_owned();
                MetadataJunit::new(store_dir, config)
            })
            .transpose()?;
        Ok(Self { junit })
    }

    pub(crate) fn write_event(&mut self, event: TestEvent<'cfg>) -> Result<(), WriteEventError> {
        if let Some(junit) = &mut self.junit {
            junit.write_event(event)?;
        }
        Ok(())
    }
}
