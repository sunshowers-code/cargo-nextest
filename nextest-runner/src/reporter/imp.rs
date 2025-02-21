// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Prints out and aggregates test execution statuses.
//!
//! The main structure in this module is [`TestReporter`].

use super::{
    displayer::{DisplayReporter, DisplayReporterBuilder, StatusLevels},
    FinalStatusLevel, StatusLevel, TestOutputDisplay,
};
use crate::{
    config::EvaluatableProfile,
    errors::WriteEventError,
    list::TestList,
    reporter::{aggregator::EventAggregator, events::*, structured::StructuredReporter},
};

/// Standard error destination for the reporter.
///
/// This is usually a terminal, but can be an in-memory buffer for tests.
pub enum ReporterStderr<'a> {
    /// Produce output on the (possibly piped) terminal.
    ///
    /// If the terminal isn't piped, produce output to a progress bar.
    Terminal,

    /// Write output to a buffer.
    Buffer(&'a mut Vec<u8>),
}

/// Test reporter builder.
#[derive(Debug, Default)]
pub struct ReporterBuilder {
    no_capture: bool,
    should_colorize: bool,
    failure_output: Option<TestOutputDisplay>,
    success_output: Option<TestOutputDisplay>,
    status_level: Option<StatusLevel>,
    final_status_level: Option<FinalStatusLevel>,

    verbose: bool,
    hide_progress_bar: bool,
}

impl ReporterBuilder {
    /// Sets no-capture mode.
    ///
    /// In this mode, `failure_output` and `success_output` will be ignored, and `status_level`
    /// will be at least [`StatusLevel::Pass`].
    pub fn set_no_capture(&mut self, no_capture: bool) -> &mut Self {
        self.no_capture = no_capture;
        self
    }

    /// Set to true if the reporter should colorize output.
    pub fn set_colorize(&mut self, should_colorize: bool) -> &mut Self {
        self.should_colorize = should_colorize;
        self
    }

    /// Sets the conditions under which test failures are output.
    pub fn set_failure_output(&mut self, failure_output: TestOutputDisplay) -> &mut Self {
        self.failure_output = Some(failure_output);
        self
    }

    /// Sets the conditions under which test successes are output.
    pub fn set_success_output(&mut self, success_output: TestOutputDisplay) -> &mut Self {
        self.success_output = Some(success_output);
        self
    }

    /// Sets the kinds of statuses to output.
    pub fn set_status_level(&mut self, status_level: StatusLevel) -> &mut Self {
        self.status_level = Some(status_level);
        self
    }

    /// Sets the kinds of statuses to output at the end of the run.
    pub fn set_final_status_level(&mut self, final_status_level: FinalStatusLevel) -> &mut Self {
        self.final_status_level = Some(final_status_level);
        self
    }

    /// Sets verbose output.
    pub fn set_verbose(&mut self, verbose: bool) -> &mut Self {
        self.verbose = verbose;
        self
    }

    /// Sets visibility of the progress bar.
    /// The progress bar is also hidden if `no_capture` is set.
    pub fn set_hide_progress_bar(&mut self, hide_progress_bar: bool) -> &mut Self {
        self.hide_progress_bar = hide_progress_bar;
        self
    }
}

impl ReporterBuilder {
    /// Creates a new test reporter.
    pub fn build<'a>(
        &self,
        test_list: &TestList,
        profile: &EvaluatableProfile<'a>,
        output: ReporterStderr<'a>,
        aggregator: EventAggregator<'a>,
        structured_reporter: StructuredReporter<'a>,
    ) -> Reporter<'a> {
        let status_level = self.status_level.unwrap_or_else(|| profile.status_level());
        let final_status_level = self
            .final_status_level
            .unwrap_or_else(|| profile.final_status_level());

        let display_reporter = DisplayReporterBuilder {
            default_filter: profile.default_filter().clone(),
            status_levels: StatusLevels {
                status_level,
                final_status_level,
            },
            test_count: test_list.test_count(),
            success_output: self.success_output,
            failure_output: self.failure_output,
            should_colorize: self.should_colorize,
            no_capture: self.no_capture,
            hide_progress_bar: self.hide_progress_bar,
        }
        .build(output);

        Reporter {
            display_reporter,
            structured_reporter,
            metadata_reporter: aggregator,
        }
    }
}

/// Functionality to report test results to stderr, JUnit, and/or structured,
/// machine-readable results to stdout.
pub struct Reporter<'a> {
    /// Used to display results to standard error.
    display_reporter: DisplayReporter<'a>,
    /// Used to aggregate events for JUnit reports written to disk
    metadata_reporter: EventAggregator<'a>,
    /// Used to emit test events in machine-readable format(s) to stdout
    structured_reporter: StructuredReporter<'a>,
}

impl<'a> Reporter<'a> {
    /// Report a test event.
    pub fn report_event(&mut self, event: TestEvent<'a>) -> Result<(), WriteEventError> {
        self.write_event(event)
    }

    /// Mark the reporter done.
    pub fn finish(&mut self) {
        self.display_reporter.finish();
    }

    // ---
    // Helper methods
    // ---

    /// Report this test event to the given writer.
    fn write_event(&mut self, event: TestEvent<'a>) -> Result<(), WriteEventError> {
        // TODO: write to all of these even if one of them fails?
        self.display_reporter.write_event(&event)?;
        self.structured_reporter.write_event(&event)?;
        self.metadata_reporter.write_event(event)?;
        Ok(())
    }
}
