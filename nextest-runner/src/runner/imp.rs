// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::{DispatcherContext, ExecutorContext, RunnerTaskState};
use crate::{
    config::{
        EvaluatableProfile, MaxFail, RetryPolicy, SetupScriptExecuteData, TestGroup, TestThreads,
    },
    double_spawn::DoubleSpawnInfo,
    errors::{ConfigureHandleInheritanceError, TestRunnerBuildError, TestRunnerExecuteErrors},
    input::{InputHandler, InputHandlerKind, InputHandlerStatus},
    list::{TestInstanceWithSettings, TestList},
    reporter::events::{RunStats, TestEvent},
    runner::ExecutorEvent,
    signal::{SignalHandler, SignalHandlerKind},
    target_runner::TargetRunner,
    test_output::CaptureStrategy,
};
use async_scoped::TokioScope;
use future_queue::{FutureQueueContext, StreamExt};
use futures::prelude::*;
use nextest_metadata::FilterMatch;
use quick_junit::ReportUuid;
use std::{convert::Infallible, fmt, sync::Arc};
use tokio::{
    runtime::Runtime,
    sync::{mpsc::unbounded_channel, oneshot},
    task::JoinError,
};
use tracing::{debug, warn};

/// Test runner options.
#[derive(Debug, Default)]
pub struct TestRunnerBuilder {
    capture_strategy: CaptureStrategy,
    retries: Option<RetryPolicy>,
    max_fail: Option<MaxFail>,
    test_threads: Option<TestThreads>,
}

impl TestRunnerBuilder {
    /// Sets the capture strategy for the test runner
    ///
    /// * [`CaptureStrategy::Split`]
    ///   * pro: output from `stdout` and `stderr` can be identified and easily split
    ///   * con: ordering between the streams cannot be guaranteed
    /// * [`CaptureStrategy::Combined`]
    ///   * pro: output is guaranteed to be ordered as it would in a terminal emulator
    ///   * con: distinction between `stdout` and `stderr` is lost
    /// * [`CaptureStrategy::None`] -
    ///   * In this mode, tests will always be run serially: `test_threads` will always be 1.
    pub fn set_capture_strategy(&mut self, strategy: CaptureStrategy) -> &mut Self {
        self.capture_strategy = strategy;
        self
    }

    /// Sets the number of retries for this test runner.
    pub fn set_retries(&mut self, retries: RetryPolicy) -> &mut Self {
        self.retries = Some(retries);
        self
    }

    /// Sets the max-fail value for this test runner.
    pub fn set_max_fail(&mut self, max_fail: MaxFail) -> &mut Self {
        self.max_fail = Some(max_fail);
        self
    }

    /// Sets the number of tests to run simultaneously.
    pub fn set_test_threads(&mut self, test_threads: TestThreads) -> &mut Self {
        self.test_threads = Some(test_threads);
        self
    }

    /// Creates a new test runner.
    #[expect(clippy::too_many_arguments)]
    pub fn build<'a>(
        self,
        test_list: &'a TestList,
        profile: &'a EvaluatableProfile<'a>,
        cli_args: Vec<String>,
        signal_handler: SignalHandlerKind,
        input_handler: InputHandlerKind,
        double_spawn: DoubleSpawnInfo,
        target_runner: TargetRunner,
    ) -> Result<TestRunner<'a>, TestRunnerBuildError> {
        let test_threads = match self.capture_strategy {
            CaptureStrategy::None => 1,
            CaptureStrategy::Combined | CaptureStrategy::Split => self
                .test_threads
                .unwrap_or_else(|| profile.test_threads())
                .compute(),
        };
        let max_fail = self.max_fail.unwrap_or_else(|| profile.max_fail());

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("nextest-runner-worker")
            .build()
            .map_err(TestRunnerBuildError::TokioRuntimeCreate)?;
        let _guard = runtime.enter();

        // signal_handler.build() must be called from within the guard.
        let signal_handler = signal_handler.build()?;

        let input_handler = input_handler.build();

        Ok(TestRunner {
            inner: TestRunnerInner {
                run_id: ReportUuid::new_v4(),
                profile,
                test_list,
                test_threads,
                double_spawn,
                target_runner,
                capture_strategy: self.capture_strategy,
                force_retries: self.retries,
                cli_args,
                max_fail,
                runtime,
            },
            signal_handler,
            input_handler,
        })
    }
}

/// Context for running tests.
///
/// Created using [`TestRunnerBuilder::build`].
#[derive(Debug)]
pub struct TestRunner<'a> {
    inner: TestRunnerInner<'a>,
    signal_handler: SignalHandler,
    input_handler: InputHandler,
}

impl<'a> TestRunner<'a> {
    /// Returns the status of the input handler.
    pub fn input_handler_status(&self) -> InputHandlerStatus {
        self.input_handler.status()
    }

    /// Executes the listed tests, each one in its own process.
    ///
    /// The callback is called with the results of each test.
    ///
    /// Returns an error if any of the tasks panicked.
    pub fn execute<F>(
        self,
        mut callback: F,
    ) -> Result<RunStats, TestRunnerExecuteErrors<Infallible>>
    where
        F: FnMut(TestEvent<'a>) + Send,
    {
        self.try_execute::<Infallible, _>(|test_event| {
            callback(test_event);
            Ok(())
        })
    }

    /// Executes the listed tests, each one in its own process.
    ///
    /// Accepts a callback that is called with the results of each test. If the callback returns an
    /// error, the test run terminates and the callback is no longer called.
    ///
    /// Returns an error if any of the tasks panicked.
    pub fn try_execute<E, F>(
        mut self,
        mut callback: F,
    ) -> Result<RunStats, TestRunnerExecuteErrors<E>>
    where
        F: FnMut(TestEvent<'a>) -> Result<(), E> + Send,
        E: fmt::Debug + Send,
    {
        let (report_cancel_tx, report_cancel_rx) = oneshot::channel();

        // If report_cancel_tx is None, at least one error has occurred and the
        // runner has been instructed to shut down. first_error is also set to
        // Some in that case.
        let mut report_cancel_tx = Some(report_cancel_tx);
        let mut first_error = None;

        let res = self.inner.execute(
            &mut self.signal_handler,
            &mut self.input_handler,
            report_cancel_rx,
            |event| {
                match callback(event) {
                    Ok(()) => {}
                    Err(error) => {
                        // If the callback fails, we need to let the runner know to start shutting
                        // down. But we keep reporting results in case the callback starts working
                        // again.
                        if let Some(report_cancel_tx) = report_cancel_tx.take() {
                            let _ = report_cancel_tx.send(());
                            first_error = Some(error);
                        }
                    }
                }
            },
        );

        // On Windows, the stdout and stderr futures might spawn processes that keep the runner
        // stuck indefinitely if it's dropped the normal way. Shut it down aggressively, being OK
        // with leaked resources.
        self.inner.runtime.shutdown_background();

        match (res, first_error) {
            (Ok(run_stats), None) => Ok(run_stats),
            (Ok(_), Some(report_error)) => Err(TestRunnerExecuteErrors {
                report_error: Some(report_error),
                join_errors: Vec::new(),
            }),
            (Err(join_errors), report_error) => Err(TestRunnerExecuteErrors {
                report_error,
                join_errors,
            }),
        }
    }
}

#[derive(Debug)]
struct TestRunnerInner<'a> {
    run_id: ReportUuid,
    profile: &'a EvaluatableProfile<'a>,
    test_list: &'a TestList<'a>,
    test_threads: usize,
    double_spawn: DoubleSpawnInfo,
    target_runner: TargetRunner,
    capture_strategy: CaptureStrategy,
    force_retries: Option<RetryPolicy>,
    cli_args: Vec<String>,
    max_fail: MaxFail,
    runtime: Runtime,
}

impl<'a> TestRunnerInner<'a> {
    fn execute<F>(
        &self,
        signal_handler: &mut SignalHandler,
        input_handler: &mut InputHandler,
        report_cancel_rx: oneshot::Receiver<()>,
        callback: F,
    ) -> Result<RunStats, Vec<JoinError>>
    where
        F: FnMut(TestEvent<'a>) + Send,
    {
        // TODO: add support for other test-running approaches, measure performance.

        let mut dispatcher_cx = DispatcherContext::new(
            callback,
            self.run_id,
            self.profile.name(),
            self.cli_args.clone(),
            self.test_list.run_count(),
            self.max_fail,
        );

        let executor_cx = ExecutorContext::new(
            self.run_id,
            self.profile,
            self.test_list,
            self.double_spawn.clone(),
            self.target_runner.clone(),
            self.capture_strategy,
            self.force_retries,
        );

        // Send the initial event.
        // (Don't need to set the cancelled atomic if this fails because the run hasn't started
        // yet.)
        dispatcher_cx.run_started(self.test_list);

        let executor_cx_ref = &executor_cx;
        let dispatcher_cx_mut = &mut dispatcher_cx;

        let _guard = self.runtime.enter();

        let ((), results) = TokioScope::scope_and_block(move |scope| {
            let (resp_tx, resp_rx) = unbounded_channel::<ExecutorEvent<'a>>();

            // Run the dispatcher to completion in a task.
            let dispatcher_fut =
                dispatcher_cx_mut.run(resp_rx, signal_handler, input_handler, report_cancel_rx);
            scope.spawn_cancellable(dispatcher_fut, || RunnerTaskState::Cancelled);

            let (script_tx, mut script_rx) = unbounded_channel::<SetupScriptExecuteData<'a>>();
            let script_resp_tx = resp_tx.clone();
            let run_scripts_fut = async move {
                // Since script tasks are run serially, we just reuse the one
                // script task.
                let script_data = executor_cx_ref.run_setup_scripts(script_resp_tx).await;
                if script_tx.send(script_data).is_err() {
                    // The dispatcher has shut down, so we should too.
                    debug!("script_tx.send failed, shutting down");
                }
                RunnerTaskState::finished_no_children()
            };
            scope.spawn_cancellable(run_scripts_fut, || RunnerTaskState::Cancelled);

            let Some(script_data) = script_rx.blocking_recv() else {
                // Most likely the harness is shutting down, so we should too.
                debug!("no script data received, shutting down");
                return;
            };

            // groups is going to be passed to future_queue_grouped.
            let groups = self
                .profile
                .test_group_config()
                .iter()
                .map(|(group_name, config)| (group_name, config.max_threads.compute()));

            let setup_script_data = Arc::new(script_data);

            let filter_resp_tx = resp_tx.clone();

            let tests = self.test_list.to_priority_queue(self.profile);
            let run_tests_fut = futures::stream::iter(tests)
                .filter_map(move |test| {
                    // Filter tests before assigning a FutureQueueContext to
                    // them.
                    //
                    // Note that this function is called lazily due to the
                    // `future_queue_grouped` below. This means that skip
                    // notifications will go out as tests are iterated over, not
                    // all at once.
                    let filter_resp_tx = filter_resp_tx.clone();
                    async move {
                        if let FilterMatch::Mismatch { reason } =
                            test.instance.test_info.filter_match
                        {
                            // Failure to send means the receiver was dropped.
                            let _ = filter_resp_tx.send(ExecutorEvent::Skipped {
                                test_instance: test.instance,
                                reason,
                            });
                            return None;
                        }
                        Some(test)
                    }
                })
                .map(move |test: TestInstanceWithSettings<'a>| {
                    let threads_required =
                        test.settings.threads_required().compute(self.test_threads);
                    let test_group = match test.settings.test_group() {
                        TestGroup::Global => None,
                        TestGroup::Custom(name) => Some(name.clone()),
                    };
                    let resp_tx = resp_tx.clone();
                    let setup_script_data = setup_script_data.clone();

                    let test_instance = test.instance;

                    let f = move |cx: FutureQueueContext| {
                        debug!("running test instance: {}; cx: {cx:?}", test_instance.id());
                        // Use a separate Tokio task for each test. For repos
                        // with lots of small tests, this has been observed to
                        // be much faster than using a single task for all tests
                        // (what we used to do). It also provides some degree of
                        // per-test isolation.
                        async move {
                            // SAFETY: Within an outer scope_and_block (which we
                            // have here), scope_and_collect is safe as long as
                            // the returned future isn't forgotten. We're not
                            // forgetting it below -- we're running it to
                            // completion immediately.
                            //
                            // But recursive scoped calls really feel like
                            // pushing against the limits of async-scoped. For
                            // example, there's no way built into async-scoped
                            // to propagate a cancellation signal from the outer
                            // scope to the inner scope. (But there could be,
                            // right? That seems solvable via channels. And we
                            // could likely do our own channels here.)
                            let ((), mut ret) = unsafe {
                                TokioScope::scope_and_collect(move |scope| {
                                    scope.spawn(executor_cx_ref.run_test_instance(
                                        test,
                                        cx,
                                        resp_tx.clone(),
                                        setup_script_data,
                                    ))
                                })
                            }
                            .await;

                            // If no future was started, that's really strange.
                            // Worth at least logging.
                            let Some(result) = ret.pop() else {
                                warn!(
                                    "no task was started for test instance: {}",
                                    test_instance.id()
                                );
                                return None;
                            };
                            match result {
                                Ok(()) => None,
                                Err(join_error) => Some(join_error),
                            }
                        }
                    };

                    (threads_required, test_group, f)
                })
                // future_queue_grouped means tests are spawned in the order
                // defined, but returned in any order.
                .future_queue_grouped(self.test_threads, groups)
                // Drop the None values.
                .filter_map(std::future::ready)
                .collect::<Vec<_>>()
                // Interestingly, using a more idiomatic `async move {
                // run_tests_fut.await ... }` block causes Rust 1.83 to complain
                // about a weird lifetime mismatch. FutureExt::map as used below
                // does not.
                .map(|child_join_errors| RunnerTaskState::Finished { child_join_errors });

            scope.spawn_cancellable(run_tests_fut, || RunnerTaskState::Cancelled);
        });

        dispatcher_cx.run_finished();

        // Were there any join errors in tasks?
        //
        // If one of the tasks panics, we likely end up stuck because the
        // dispatcher, which is spawned in the same async-scoped block, doesn't
        // get relayed the panic immediately. That should probably be fixed at
        // some point.
        let mut cancelled_count = 0;
        let join_errors = results
            .into_iter()
            .flat_map(|r| {
                match r {
                    Ok(RunnerTaskState::Finished { child_join_errors }) => child_join_errors,
                    // Largely ignore cancelled tasks since it most likely means
                    // shutdown -- we don't cancel tasks manually.
                    Ok(RunnerTaskState::Cancelled) => {
                        cancelled_count += 1;
                        Vec::new()
                    }
                    Err(join_error) => vec![join_error],
                }
            })
            .collect::<Vec<_>>();

        if cancelled_count > 0 {
            debug!(
                "{} tasks were cancelled -- this \
                 generally should only happen due to panics",
                cancelled_count
            );
        }
        if !join_errors.is_empty() {
            return Err(join_errors);
        }
        Ok(dispatcher_cx.run_stats())
    }
}

/// Configures stdout, stdin and stderr inheritance by test processes on Windows.
///
/// With Rust on Windows, these handles can be held open by tests (and therefore by grandchild processes)
/// even if we run the tests with `Stdio::inherit`. This can cause problems with leaky tests.
///
/// This changes global state on the Win32 side, so the application must manage mutual exclusion
/// around it. Call this right before [`TestRunner::try_execute`].
///
/// This is a no-op on non-Windows platforms.
///
/// See [this issue on the Rust repository](https://github.com/rust-lang/rust/issues/54760) for more
/// discussion.
pub fn configure_handle_inheritance(
    no_capture: bool,
) -> Result<(), ConfigureHandleInheritanceError> {
    super::os::configure_handle_inheritance_impl(no_capture)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::NextestConfig, platform::BuildPlatforms};

    #[test]
    fn no_capture_settings() {
        // Ensure that output settings are ignored with no-capture.
        let mut builder = TestRunnerBuilder::default();
        builder
            .set_capture_strategy(CaptureStrategy::None)
            .set_test_threads(TestThreads::Count(20));
        let test_list = TestList::empty();
        let config = NextestConfig::default_config("/fake/dir");
        let profile = config.profile(NextestConfig::DEFAULT_PROFILE).unwrap();
        let build_platforms = BuildPlatforms::new_with_no_target().unwrap();
        let signal_handler = SignalHandlerKind::Noop;
        let input_handler = InputHandlerKind::Noop;
        let profile = profile.into_evaluatable(&build_platforms);
        let runner = builder
            .build(
                &test_list,
                &profile,
                vec![],
                signal_handler,
                input_handler,
                DoubleSpawnInfo::disabled(),
                TargetRunner::empty(),
            )
            .unwrap();
        assert_eq!(runner.inner.capture_strategy, CaptureStrategy::None);
        assert_eq!(runner.inner.test_threads, 1, "tests run serially");
    }
}
