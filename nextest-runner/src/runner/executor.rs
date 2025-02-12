// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The executor for tests.
//!
//! This component is responsible for running tests and reporting results to the
//! dispatcher.
//!
//! Note that the executor itself does not communicate directly with the outside
//! world. All communication is mediated by the dispatcher -- doing so is not
//! just a better abstraction, it also provides a better user experience (less
//! inconsistent state).

use super::HandleSignalResult;
use crate::{
    config::{
        EvaluatableProfile, RetryPolicy, ScriptConfig, ScriptId, SetupScriptCommand,
        SetupScriptExecuteData, SlowTimeout, TestGroup, TestSettings,
    },
    double_spawn::DoubleSpawnInfo,
    errors::{ChildError, ChildFdError, ChildStartError, ErrorList},
    list::{TestExecuteContext, TestInstance, TestList},
    reporter::events::{
        AbortStatus, ExecutionResult, InfoResponse, RetryData, SetupScriptInfoResponse,
        TestInfoResponse, UnitKind, UnitState,
    },
    runner::{
        parse_env_file, ExecutorEvent, InternalExecuteStatus, InternalSetupScriptExecuteStatus,
        InternalTerminateReason, RunUnitQuery, RunUnitRequest, SignalRequest, UnitExecuteStatus,
    },
    target_runner::TargetRunner,
    test_command::{ChildAccumulator, ChildFds},
    test_output::{CaptureStrategy, ChildExecutionOutput, ChildOutput, ChildSplitOutput},
    time::{PausableSleep, StopwatchStart},
};
use future_queue::FutureQueueContext;
use nextest_metadata::FilterMatch;
use quick_junit::ReportUuid;
use rand::{distributions::OpenClosed01, thread_rng, Rng};
use std::{
    num::NonZeroUsize,
    pin::Pin,
    process::{ExitStatus, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::{
    process::Child,
    sync::{
        mpsc::{UnboundedReceiver, UnboundedSender},
        oneshot,
    },
};
use tracing::{debug, instrument};

#[derive(Debug)]
pub(super) struct ExecutorContext<'a> {
    run_id: ReportUuid,
    profile: &'a EvaluatableProfile<'a>,
    test_list: &'a TestList<'a>,
    double_spawn: DoubleSpawnInfo,
    target_runner: TargetRunner,
    capture_strategy: CaptureStrategy,
    // This is Some if the user specifies a retry policy over the command-line.
    force_retries: Option<RetryPolicy>,
}

impl<'a> ExecutorContext<'a> {
    pub(super) fn new(
        run_id: ReportUuid,
        profile: &'a EvaluatableProfile<'a>,
        test_list: &'a TestList<'a>,
        double_spawn: DoubleSpawnInfo,
        target_runner: TargetRunner,
        capture_strategy: CaptureStrategy,
        force_retries: Option<RetryPolicy>,
    ) -> Self {
        Self {
            run_id,
            profile,
            test_list,
            double_spawn,
            target_runner,
            capture_strategy,
            force_retries,
        }
    }

    /// Run scripts, returning data about each successfully executed script.
    pub(super) async fn run_setup_scripts(
        &self,
        resp_tx: UnboundedSender<ExecutorEvent<'a>>,
    ) -> SetupScriptExecuteData<'a> {
        let setup_scripts = self.profile.setup_scripts(self.test_list);
        let total = setup_scripts.len();
        debug!("running {} setup scripts", total);

        let mut setup_script_data = SetupScriptExecuteData::new();

        // Run setup scripts one by one.
        for (index, script) in setup_scripts.into_iter().enumerate() {
            let this_resp_tx = resp_tx.clone();

            let script_id = script.id.clone();
            let config = script.config;

            let script_fut = async move {
                let (req_rx_tx, req_rx_rx) = oneshot::channel();
                let _ = this_resp_tx.send(ExecutorEvent::SetupScriptStarted {
                    script_id: script_id.clone(),
                    config,
                    index,
                    total,
                    req_rx_tx,
                });
                let mut req_rx = match req_rx_rx.await {
                    Ok(req_rx) => req_rx,
                    Err(_) => {
                        // The receiver was dropped -- the dispatcher has
                        // signaled that this unit should exit.
                        return None;
                    }
                };

                let packet = SetupScriptPacket {
                    script_id: script_id.clone(),
                    config,
                };

                let status = self
                    .run_setup_script(packet, &this_resp_tx, &mut req_rx)
                    .await;

                // Drain the request receiver, responding to any final requests
                // that may have been sent.
                drain_req_rx(req_rx, UnitExecuteStatus::SetupScript(&status));

                let status = status.into_external();
                let env_map = status.env_map.clone();

                let _ = this_resp_tx.send(ExecutorEvent::SetupScriptFinished {
                    script_id,
                    config,
                    index,
                    total,
                    status,
                });

                env_map.map(|env_map| (script, env_map))
            };

            // Run this setup script to completion.
            if let Some((script, env_map)) = script_fut.await {
                setup_script_data.add_script(script, env_map);
            }
        }

        setup_script_data
    }

    /// Returns a future that runs all attempts of a single test instance.
    pub(super) async fn run_test_instance(
        &self,
        test_instance: TestInstance<'a>,
        cx: FutureQueueContext,
        settings: TestSettings<'a>,
        resp_tx: UnboundedSender<ExecutorEvent<'a>>,
        setup_script_data: Arc<SetupScriptExecuteData<'a>>,
    ) {
        debug!(test_name = test_instance.name, "running test");

        let settings = Arc::new(settings);

        let retry_policy = self.force_retries.unwrap_or_else(|| settings.retries());
        let total_attempts = retry_policy.count() + 1;
        let mut backoff_iter = BackoffIter::new(retry_policy);

        if let FilterMatch::Mismatch { reason } = test_instance.test_info.filter_match {
            debug_assert!(
                false,
                "this test should already have been skipped in a filter step"
            );
            // Failure to send means the receiver was dropped.
            let _ = resp_tx.send(ExecutorEvent::Skipped {
                test_instance,
                reason,
            });
            return;
        }

        let (req_rx_tx, req_rx_rx) = oneshot::channel();

        // Wait for the Started event to be processed by the
        // execution future.
        _ = resp_tx.send(ExecutorEvent::Started {
            test_instance,
            req_rx_tx,
        });
        let mut req_rx = match req_rx_rx.await {
            Ok(rx) => rx,
            Err(_) => {
                // The receiver was dropped -- the dispatcher has signaled that this unit should
                // exit.
                return;
            }
        };

        let mut attempt = 0;
        let mut delay = Duration::ZERO;
        let last_run_status = loop {
            attempt += 1;
            let retry_data = RetryData {
                attempt,
                total_attempts,
            };

            if retry_data.attempt > 1 {
                // Ensure that the dispatcher believes the run is still ongoing.
                // If the run is cancelled, the dispatcher will let us know by
                // dropping the receiver.
                let (tx, rx) = oneshot::channel();
                _ = resp_tx.send(ExecutorEvent::RetryStarted {
                    test_instance,
                    retry_data,
                    tx,
                });

                match rx.await {
                    Ok(()) => {}
                    Err(_) => {
                        // The receiver was dropped -- the dispatcher has
                        // signaled that this unit should exit.
                        return;
                    }
                }
            }

            // Some of this information is only useful for event reporting, but
            // it's a lot easier to pass it in than to try and hook on
            // additional information later.
            let packet = TestPacket {
                test_instance,
                cx: cx.clone(),
                retry_data,
                settings: settings.clone(),
                setup_script_data: setup_script_data.clone(),
                delay_before_start: delay,
            };

            let run_status = self.run_test(packet.clone(), &resp_tx, &mut req_rx).await;

            if run_status.result.is_success() {
                // The test succeeded.
                break run_status;
            } else if retry_data.attempt < retry_data.total_attempts {
                // Retry this test: send a retry event, then retry the loop.
                delay = backoff_iter
                    .next()
                    .expect("backoff delay must be non-empty");

                let run_status = run_status.into_external();
                let previous_result = run_status.result;
                let previous_slow = run_status.is_slow;

                let _ = resp_tx.send(ExecutorEvent::AttemptFailedWillRetry {
                    test_instance,
                    failure_output: settings.failure_output(),
                    run_status,
                    delay_before_next_attempt: delay,
                });

                handle_delay_between_attempts(
                    &packet,
                    previous_result,
                    previous_slow,
                    delay,
                    &mut req_rx,
                )
                .await;
            } else {
                // This test failed and is out of retries.
                break run_status;
            }
        };

        drain_req_rx(req_rx, UnitExecuteStatus::Test(&last_run_status));

        // At this point, either:
        // * the test has succeeded, or
        // * the test has failed and we've run out of retries.
        // In either case, the test is finished.
        let last_run_status = last_run_status.into_external();
        let _ = resp_tx.send(ExecutorEvent::Finished {
            test_instance,
            success_output: settings.success_output(),
            failure_output: settings.failure_output(),
            junit_store_success_output: settings.junit_store_success_output(),
            junit_store_failure_output: settings.junit_store_failure_output(),
            last_run_status,
        });
    }

    // ---
    // Helper methods
    // ---

    /// Run an individual setup script in its own process.
    #[instrument(level = "debug", skip(self, resp_tx, req_rx))]
    async fn run_setup_script(
        &self,
        script: SetupScriptPacket<'a>,
        resp_tx: &UnboundedSender<ExecutorEvent<'a>>,
        req_rx: &mut UnboundedReceiver<RunUnitRequest<'a>>,
    ) -> InternalSetupScriptExecuteStatus<'a> {
        let mut stopwatch = crate::time::stopwatch();

        match self
            .run_setup_script_inner(script.clone(), &mut stopwatch, resp_tx, req_rx)
            .await
        {
            Ok(status) => status,
            Err(error) => InternalSetupScriptExecuteStatus {
                script,
                slow_after: None,
                output: ChildExecutionOutput::StartError(error),
                result: ExecutionResult::ExecFail,
                stopwatch_end: stopwatch.snapshot(),
                env_map: None,
            },
        }
    }

    #[instrument(level = "debug", skip(self, resp_tx, req_rx))]
    async fn run_setup_script_inner(
        &self,
        script: SetupScriptPacket<'a>,
        stopwatch: &mut StopwatchStart,
        resp_tx: &UnboundedSender<ExecutorEvent<'a>>,
        req_rx: &mut UnboundedReceiver<RunUnitRequest<'a>>,
    ) -> Result<InternalSetupScriptExecuteStatus<'a>, ChildStartError> {
        let mut cmd =
            script.make_command(self.profile.name(), &self.double_spawn, self.test_list)?;
        let command_mut = cmd.command_mut();

        command_mut.env("NEXTEST_RUN_ID", format!("{}", self.run_id));
        command_mut.stdin(Stdio::null());
        super::os::set_process_group(command_mut);

        // If creating a job fails, we might be on an old system. Ignore this -- job objects are a
        // best-effort thing.
        let job = super::os::Job::create().ok();

        // The --no-capture CLI argument overrides the config.
        if self.capture_strategy != CaptureStrategy::None {
            if script.config.capture_stdout {
                command_mut.stdout(std::process::Stdio::piped());
            }
            if script.config.capture_stderr {
                command_mut.stderr(std::process::Stdio::piped());
            }
        }

        let (mut child, env_path) = cmd
            .spawn()
            .map_err(|error| ChildStartError::Spawn(Arc::new(error)))?;
        let child_pid = child
            .id()
            .expect("child has never been polled so must return a PID");

        // If assigning the child to the job fails, ignore this. This can happen if the process has
        // exited.
        let _ = super::os::assign_process_to_job(&child, job.as_ref());

        let mut status: Option<ExecutionResult> = None;
        // Unlike with tests, we don't automatically assume setup scripts are slow if they take a
        // long time. For example, consider a setup script that performs a cargo build -- it can
        // take an indeterminate amount of time. That's why we set a very large slow timeout rather
        // than the test default of 60 seconds.
        let slow_timeout = script
            .config
            .slow_timeout
            .unwrap_or(SlowTimeout::VERY_LARGE);
        let leak_timeout = script
            .config
            .leak_timeout
            .unwrap_or(Duration::from_millis(100));

        let mut interval_sleep = std::pin::pin!(crate::time::pausable_sleep(slow_timeout.period));

        let mut timeout_hit = 0;

        let child_fds = ChildFds::new_split(child.stdout.take(), child.stderr.take());
        let mut child_acc = ChildAccumulator::new(child_fds);

        let mut cx = UnitContext {
            packet: UnitPacket::SetupScript(script.clone()),
            slow_after: None,
        };

        let (res, leaked) = {
            let res = loop {
                tokio::select! {
                    () = child_acc.fill_buf(), if !child_acc.fds.is_done() => {}
                    res = child.wait() => {
                        // The setup script finished executing.
                        break res;
                    }
                    _ = &mut interval_sleep, if status.is_none() => {
                        // Mark the script as slow.
                        cx.slow_after = Some(slow_timeout.period);

                        timeout_hit += 1;
                        let will_terminate = if let Some(terminate_after) = slow_timeout.terminate_after {
                            NonZeroUsize::new(timeout_hit as usize)
                                .expect("timeout_hit was just incremented")
                                >= terminate_after
                        } else {
                            false
                        };

                        if !slow_timeout.grace_period.is_zero() {
                            let _ = resp_tx.send(script.slow_event(
                                // Pass in the slow timeout period times timeout_hit, since
                                // stopwatch.elapsed() tends to be slightly longer.
                                timeout_hit * slow_timeout.period,
                                will_terminate.then_some(slow_timeout.grace_period),
                            ));
                        }

                        if will_terminate {
                            // Attempt to terminate the slow script. As there is
                            // a race between shutting down a slow test and its
                            // own completion, we silently ignore errors to
                            // avoid printing false warnings.
                            //
                            // The return result of terminate_child is not used
                            // here, since it is always marked as a timeout.
                            _ = super::os::terminate_child(
                                &cx,
                                &mut child,
                                &mut child_acc,
                                InternalTerminateReason::Timeout,
                                stopwatch,
                                req_rx,
                                job.as_ref(),
                                slow_timeout.grace_period,
                            ).await;
                            status = Some(ExecutionResult::Timeout);
                            if slow_timeout.grace_period.is_zero() {
                                break child.wait().await;
                            }
                            // Don't break here to give the wait task a chance to finish.
                        } else {
                            interval_sleep.as_mut().reset_last_duration();
                        }
                    }
                    recv = req_rx.recv() => {
                        // The sender stays open longer than the whole loop, and the buffer is big
                        // enough for all messages ever sent through this channel, so a RecvError
                        // should never happen.
                        let req = recv.expect("a RecvError should never happen here");

                        match req {
                            RunUnitRequest::Signal(req) => {
                                #[cfg_attr(not(windows), expect(unused_variables))]
                                let res = handle_signal_request(
                                    &cx,
                                    &mut child,
                                    &mut child_acc,
                                    req,
                                    stopwatch,
                                    interval_sleep.as_mut(),
                                    req_rx,
                                    job.as_ref(),
                                    slow_timeout.grace_period
                                ).await;

                                // On Unix, the signal the process exited with
                                // will be picked up by child.wait. On Windows,
                                // termination by job object will show up as
                                // exit code 1 -- we need to be clearer about
                                // that in the UI.
                                //
                                // TODO: Can we do something useful with res on
                                // Unix? For example, it's possible that the
                                // signal we send is not the same as the signal
                                // the child exits with. This might be a good
                                // thing to store in whatever test event log we
                                // end up building.
                                #[cfg(windows)]
                                {
                                    if matches!(
                                        res,
                                        HandleSignalResult::Terminated(super::TerminateChildResult::Killed)
                                    ) {
                                        status = Some(ExecutionResult::Fail {
                                            abort_status: Some(AbortStatus::JobObject),
                                            leaked: false,
                                        });
                                    }
                                }
                            }
                            RunUnitRequest::OtherCancel => {
                                // Ignore non-signal cancellation requests --
                                // let the script finish.
                            }
                            RunUnitRequest::Query(RunUnitQuery::GetInfo(sender)) => {
                                _ = sender.send(script.info_response(
                                    UnitState::Running {
                                        pid: child_pid,
                                        time_taken:             stopwatch.snapshot().active,
                                        slow_after: cx.slow_after,
                                    },
                                    child_acc.snapshot_in_progress(UnitKind::WAITING_ON_SCRIPT_MESSAGE),
                                ));
                            }
                        }
                    }
                }
            };

            // Build a tentative status using status and the exit status.
            let tentative_status = status.or_else(|| {
                res.as_ref()
                    .ok()
                    .map(|res| create_execution_result(*res, &child_acc.errors, false))
            });

            let leaked = detect_fd_leaks(
                &cx,
                child_pid,
                &mut child_acc,
                tentative_status,
                leak_timeout,
                stopwatch,
                req_rx,
            )
            .await;

            (res, leaked)
        };

        let exit_status = match res {
            Ok(exit_status) => Some(exit_status),
            Err(err) => {
                child_acc.errors.push(ChildFdError::Wait(Arc::new(err)));
                None
            }
        };

        let exit_status = exit_status.expect("None always results in early return");

        let exec_result = status
            .unwrap_or_else(|| create_execution_result(exit_status, &child_acc.errors, leaked));

        // Read from the environment map. If there's an error here, add it to the list of child errors.
        let mut errors: Vec<_> = child_acc.errors.into_iter().map(ChildError::from).collect();
        let env_map = if exec_result.is_success() {
            match parse_env_file(&env_path).await {
                Ok(env_map) => Some(env_map),
                Err(error) => {
                    errors.push(ChildError::SetupScriptOutput(error));
                    None
                }
            }
        } else {
            None
        };

        Ok(InternalSetupScriptExecuteStatus {
            script,
            slow_after: cx.slow_after,
            output: ChildExecutionOutput::Output {
                result: Some(exec_result),
                output: child_acc.output.freeze(),
                errors: ErrorList::new(UnitKind::WAITING_ON_SCRIPT_MESSAGE, errors),
            },
            result: exec_result,
            stopwatch_end: stopwatch.snapshot(),
            env_map,
        })
    }

    /// Run an individual test in its own process.
    #[instrument(level = "debug", skip(self, resp_tx, req_rx))]
    async fn run_test(
        &self,
        test: TestPacket<'a>,
        resp_tx: &UnboundedSender<ExecutorEvent<'a>>,
        req_rx: &mut UnboundedReceiver<RunUnitRequest<'a>>,
    ) -> InternalExecuteStatus<'a> {
        let mut stopwatch = crate::time::stopwatch();

        match self
            .run_test_inner(test.clone(), &mut stopwatch, resp_tx, req_rx)
            .await
        {
            Ok(run_status) => run_status,
            Err(error) => InternalExecuteStatus {
                test,
                slow_after: None,
                output: ChildExecutionOutput::StartError(error),
                result: ExecutionResult::ExecFail,
                stopwatch_end: stopwatch.snapshot(),
            },
        }
    }

    #[instrument(level = "debug", skip(self, resp_tx, req_rx))]
    async fn run_test_inner<'test>(
        &self,
        test: TestPacket<'a>,
        stopwatch: &mut StopwatchStart,
        resp_tx: &UnboundedSender<ExecutorEvent<'a>>,
        req_rx: &mut UnboundedReceiver<RunUnitRequest<'a>>,
    ) -> Result<InternalExecuteStatus<'a>, ChildStartError> {
        let ctx = TestExecuteContext {
            profile_name: self.profile.name(),
            double_spawn: &self.double_spawn,
            target_runner: &self.target_runner,
        };
        let mut cmd =
            test.test_instance
                .make_command(&ctx, self.test_list, test.settings.run_extra_args());
        let command_mut = cmd.command_mut();

        // Debug environment variable for testing.
        command_mut.env("__NEXTEST_ATTEMPT", format!("{}", test.retry_data.attempt));

        command_mut.env("NEXTEST_RUN_ID", format!("{}", self.run_id));

        // Set slot environment variables.
        command_mut.env(
            "NEXTEST_TEST_GLOBAL_SLOT",
            test.cx.global_slot().to_string(),
        );
        if let Some(group_slot) = test.cx.group_slot() {
            command_mut.env("NEXTEST_TEST_GROUP_SLOT", group_slot.to_string());
        } else {
            command_mut.env_remove("NEXTEST_TEST_GROUP_SLOT");
        }

        match test.settings.test_group() {
            TestGroup::Custom(name) => {
                debug_assert!(
                    test.cx.group_slot().is_some(),
                    "test_group being set implies group_slot is set"
                );
                command_mut.env("NEXTEST_TEST_GROUP", name.as_str());
            }
            TestGroup::Global => {
                debug_assert!(
                    test.cx.group_slot().is_none(),
                    "test_group being unset implies group_slot is unset"
                );
                command_mut.env_remove("NEXTEST_TEST_GROUP");
            }
        }

        command_mut.stdin(Stdio::null());
        test.setup_script_data.apply(
            &test.test_instance.to_test_query(),
            &self.profile.filterset_ecx(),
            command_mut,
        );
        super::os::set_process_group(command_mut);

        // If creating a job fails, we might be on an old system. Ignore this -- job objects are a
        // best-effort thing.
        let job = super::os::Job::create().ok();

        let crate::test_command::Child {
            mut child,
            child_fds,
        } = cmd
            .spawn(self.capture_strategy)
            .map_err(|error| ChildStartError::Spawn(Arc::new(error)))?;

        // Note: The PID stored here must be used with care -- it might be
        // outdated and have been reused by the kernel in case the process
        // has exited. If using for any real logic (not just reporting) it
        // might be best to always check child.id().
        let child_pid = child
            .id()
            .expect("child has never been polled so must return a PID");

        // If assigning the child to the job fails, ignore this. This can happen if the process has
        // exited.
        let _ = super::os::assign_process_to_job(&child, job.as_ref());

        let mut child_acc = ChildAccumulator::new(child_fds);

        let mut status: Option<ExecutionResult> = None;
        let slow_timeout = test.settings.slow_timeout();
        let leak_timeout = test.settings.leak_timeout();

        // Use a pausable_sleep rather than an interval here because it's much
        // harder to pause and resume an interval.
        let mut interval_sleep = std::pin::pin!(crate::time::pausable_sleep(slow_timeout.period));

        let mut timeout_hit = 0;

        let mut cx = UnitContext {
            packet: UnitPacket::Test(test.clone()),
            slow_after: None,
        };

        let (res, leaked) = {
            let res = loop {
                tokio::select! {
                    () = child_acc.fill_buf(), if !child_acc.fds.is_done() => {}
                    res = child.wait() => {
                        // The test finished executing.
                        break res;
                    }
                    _ = &mut interval_sleep, if status.is_none() => {
                        // Mark the test as slow.
                        cx.slow_after = Some(slow_timeout.period);

                        timeout_hit += 1;
                        let will_terminate = if let Some(terminate_after) = slow_timeout.terminate_after {
                            NonZeroUsize::new(timeout_hit as usize)
                                .expect("timeout_hit was just incremented")
                                >= terminate_after
                        } else {
                            false
                        };

                        if !slow_timeout.grace_period.is_zero() {
                            let _ = resp_tx.send(test.slow_event(
                                // Pass in the slow timeout period times timeout_hit, since
                                // stopwatch.elapsed() tends to be slightly longer.
                                timeout_hit * slow_timeout.period,
                                will_terminate.then_some(slow_timeout.grace_period),
                            ));
                        }

                        if will_terminate {
                            // Attempt to terminate the slow test. As there is a
                            // race between shutting down a slow test and its
                            // own completion, we silently ignore errors to
                            // avoid printing false warnings.
                            //
                            // The return result of terminate_child is not used
                            // here, since it is always marked as a timeout.
                            _ = super::os::terminate_child(
                                &cx,
                                &mut child,
                                &mut child_acc,
                                InternalTerminateReason::Timeout,
                                stopwatch,
                                req_rx,
                                job.as_ref(),
                                slow_timeout.grace_period,
                            ).await;
                            status = Some(ExecutionResult::Timeout);
                            if slow_timeout.grace_period.is_zero() {
                                break child.wait().await;
                            }
                            // Don't break here to give the wait task a chance to finish.
                        } else {
                            interval_sleep.as_mut().reset_last_duration();
                        }
                    }
                    recv = req_rx.recv() => {
                        // The sender stays open longer than the whole loop so a
                        // RecvError should never happen.
                        let req = recv.expect("req_rx sender is open");

                        match req {
                            RunUnitRequest::Signal(req) => {
                                #[cfg_attr(not(windows), expect(unused_variables))]
                                let res = handle_signal_request(
                                    &cx,
                                    &mut child,
                                    &mut child_acc,
                                    req,
                                    stopwatch,
                                    interval_sleep.as_mut(),
                                    req_rx,
                                    job.as_ref(),
                                    slow_timeout.grace_period,
                                ).await;

                                // On Unix, the signal the process exited with
                                // will be picked up by child.wait. On Windows,
                                // termination by job object will show up as
                                // exit code 1 -- we need to be clearer about
                                // that in the UI.
                                //
                                // TODO: Can we do something useful with res on
                                // Unix? For example, it's possible that the
                                // signal we send is not the same as the signal
                                // the child exits with. This might be a good
                                // thing to store in whatever test event log we
                                // end up building.
                                #[cfg(windows)]
                                {
                                    if matches!(
                                        res,
                                        HandleSignalResult::Terminated(super::TerminateChildResult::Killed)
                                    ) {
                                        status = Some(ExecutionResult::Fail {
                                            abort_status: Some(AbortStatus::JobObject),
                                            leaked: false,
                                        });
                                    }
                                }
                            }
                            RunUnitRequest::OtherCancel => {
                                // Ignore non-signal cancellation requests --
                                // let the test finish.
                            }
                            RunUnitRequest::Query(RunUnitQuery::GetInfo(tx)) => {
                                _ = tx.send(test.info_response(
                                    UnitState::Running {
                                        pid: child_pid,
                                        time_taken: stopwatch.snapshot().active,
                                        slow_after: cx.slow_after,
                                    },
                                    child_acc.snapshot_in_progress(UnitKind::WAITING_ON_TEST_MESSAGE),
                                ));
                            }
                        }
                    }
                };
            };

            // Build a tentative status using status and the exit status.
            let tentative_status = status.or_else(|| {
                res.as_ref()
                    .ok()
                    .map(|res| create_execution_result(*res, &child_acc.errors, false))
            });

            let leaked = detect_fd_leaks(
                &cx,
                child_pid,
                &mut child_acc,
                tentative_status,
                leak_timeout,
                stopwatch,
                req_rx,
            )
            .await;

            (res, leaked)
        };

        let exit_status = match res {
            Ok(exit_status) => Some(exit_status),
            Err(err) => {
                child_acc.errors.push(ChildFdError::Wait(Arc::new(err)));
                None
            }
        };

        let exit_status = exit_status.expect("None always results in early return");
        let exec_result = status
            .unwrap_or_else(|| create_execution_result(exit_status, &child_acc.errors, leaked));

        Ok(InternalExecuteStatus {
            test,
            slow_after: cx.slow_after,
            output: ChildExecutionOutput::Output {
                result: Some(exec_result),
                output: child_acc.output.freeze(),
                errors: ErrorList::new(UnitKind::WAITING_ON_TEST_MESSAGE, child_acc.errors),
            },
            result: exec_result,
            stopwatch_end: stopwatch.snapshot(),
        })
    }
}

#[derive(Debug)]
struct BackoffIter {
    policy: RetryPolicy,
    current_factor: f64,
    remaining_attempts: usize,
}

impl BackoffIter {
    const BACKOFF_EXPONENT: f64 = 2.;

    fn new(policy: RetryPolicy) -> Self {
        let remaining_attempts = policy.count();
        Self {
            policy,
            current_factor: 1.,
            remaining_attempts,
        }
    }

    fn next_delay_and_jitter(&mut self) -> (Duration, bool) {
        match self.policy {
            RetryPolicy::Fixed { delay, jitter, .. } => (delay, jitter),
            RetryPolicy::Exponential {
                delay,
                jitter,
                max_delay,
                ..
            } => {
                let factor = self.current_factor;
                let exp_delay = delay.mul_f64(factor);

                // Stop multiplying the exponential factor if delay is greater than max_delay.
                if let Some(max_delay) = max_delay {
                    if exp_delay > max_delay {
                        return (max_delay, jitter);
                    }
                }

                let next_factor = self.current_factor * Self::BACKOFF_EXPONENT;
                self.current_factor = next_factor;

                (exp_delay, jitter)
            }
        }
    }

    fn apply_jitter(duration: Duration) -> Duration {
        let jitter: f64 = thread_rng().sample(OpenClosed01);
        // Apply jitter in the range (0.5, 1].
        duration.mul_f64(0.5 + jitter / 2.)
    }
}

impl Iterator for BackoffIter {
    type Item = Duration;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining_attempts > 0 {
            let (mut delay, jitter) = self.next_delay_and_jitter();
            if jitter {
                delay = Self::apply_jitter(delay);
            }
            self.remaining_attempts -= 1;
            Some(delay)
        } else {
            None
        }
    }
}

/// Either a test or a setup script, along with information about how long the
/// test took.
pub(super) struct UnitContext<'a> {
    packet: UnitPacket<'a>,
    // TODO: This is a bit of a mess. It isn't clear where this kind of state
    // should live -- many parts of the request-response system need various
    // pieces of this code.
    slow_after: Option<Duration>,
}

impl<'a> UnitContext<'a> {
    pub(super) fn packet(&self) -> &UnitPacket<'a> {
        &self.packet
    }

    pub(super) fn info_response(
        &self,
        state: UnitState,
        output: ChildExecutionOutput,
    ) -> InfoResponse<'a> {
        match &self.packet {
            UnitPacket::SetupScript(packet) => packet.info_response(state, output),
            UnitPacket::Test(packet) => packet.info_response(state, output),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) enum UnitPacket<'a> {
    SetupScript(SetupScriptPacket<'a>),
    Test(TestPacket<'a>),
}

impl UnitPacket<'_> {
    pub(super) fn kind(&self) -> UnitKind {
        match self {
            Self::SetupScript(_) => UnitKind::Script,
            Self::Test(_) => UnitKind::Test,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct TestPacket<'a> {
    test_instance: TestInstance<'a>,
    cx: FutureQueueContext,
    retry_data: RetryData,
    settings: Arc<TestSettings<'a>>,
    setup_script_data: Arc<SetupScriptExecuteData<'a>>,
    delay_before_start: Duration,
}

impl<'a> TestPacket<'a> {
    fn slow_event(&self, elapsed: Duration, will_terminate: Option<Duration>) -> ExecutorEvent<'a> {
        ExecutorEvent::Slow {
            test_instance: self.test_instance,
            retry_data: self.retry_data,
            elapsed,
            will_terminate,
        }
    }

    pub(super) fn retry_data(&self) -> RetryData {
        self.retry_data
    }

    pub(super) fn delay_before_start(&self) -> Duration {
        self.delay_before_start
    }

    pub(super) fn info_response(
        &self,
        state: UnitState,
        output: ChildExecutionOutput,
    ) -> InfoResponse<'a> {
        InfoResponse::Test(TestInfoResponse {
            test_instance: self.test_instance.id(),
            state,
            retry_data: self.retry_data,
            output,
        })
    }
}

#[derive(Clone, Debug)]
pub(super) struct SetupScriptPacket<'a> {
    script_id: ScriptId,
    config: &'a ScriptConfig,
}

impl<'a> SetupScriptPacket<'a> {
    /// Turns self into a command that can be executed.
    fn make_command(
        &self,
        profile_name: &str,
        double_spawn: &DoubleSpawnInfo,
        test_list: &TestList<'_>,
    ) -> Result<SetupScriptCommand, ChildStartError> {
        SetupScriptCommand::new(self.config, profile_name, double_spawn, test_list)
    }

    fn slow_event(&self, elapsed: Duration, will_terminate: Option<Duration>) -> ExecutorEvent<'a> {
        ExecutorEvent::SetupScriptSlow {
            script_id: self.script_id.clone(),
            config: self.config,
            elapsed,
            will_terminate,
        }
    }

    pub(super) fn info_response(
        &self,
        state: UnitState,
        output: ChildExecutionOutput,
    ) -> InfoResponse<'a> {
        InfoResponse::SetupScript(SetupScriptInfoResponse {
            script_id: self.script_id.clone(),
            command: self.config.program(),
            args: self.config.args(),
            state,
            output,
        })
    }
}

/// Drains the request receiver of any messages.
fn drain_req_rx<'a>(
    mut receiver: UnboundedReceiver<RunUnitRequest<'a>>,
    status: UnitExecuteStatus<'a, '_>,
) {
    // Mark the receiver closed so no further messages are sent.
    receiver.close();
    loop {
        // Receive anything that's left in the receiver.
        let message = receiver.try_recv();
        match message {
            Ok(message) => {
                message.drain(status);
            }
            Err(_) => {
                break;
            }
        }
    }
}

async fn handle_delay_between_attempts<'a>(
    packet: &TestPacket<'a>,
    previous_result: ExecutionResult,
    previous_slow: bool,
    delay: Duration,
    req_rx: &mut UnboundedReceiver<RunUnitRequest<'a>>,
) {
    let mut sleep = std::pin::pin!(crate::time::pausable_sleep(delay));
    #[cfg_attr(not(unix), expect(unused_mut))]
    let mut waiting_stopwatch = crate::time::stopwatch();

    loop {
        tokio::select! {
            _ = &mut sleep => {
                // The timer has expired.
                break;
            }
            recv = req_rx.recv() => {
                let req = recv.expect("req_rx sender is open");

                match req {
                    #[cfg(unix)]
                    RunUnitRequest::Signal(SignalRequest::Stop(tx)) => {
                        sleep.as_mut().pause();
                        waiting_stopwatch.pause();
                        _ = tx.send(());
                    }
                    #[cfg(unix)]
                    RunUnitRequest::Signal(SignalRequest::Continue) => {
                        if sleep.is_paused() {
                            sleep.as_mut().resume();
                            waiting_stopwatch.resume();
                        }
                    }
                    RunUnitRequest::Signal(SignalRequest::Shutdown(_)) => {
                        // The run was cancelled, so go ahead and perform a
                        // shutdown.
                        break;
                    }
                    RunUnitRequest::OtherCancel => {
                        // If a cancellation was requested, break out of the
                        // loop.
                        break;
                    }
                    RunUnitRequest::Query(RunUnitQuery::GetInfo(tx)) => {
                        let waiting_snapshot = waiting_stopwatch.snapshot();
                        _ = tx.send(
                            packet.info_response(
                                UnitState::DelayBeforeNextAttempt {
                                    previous_result,
                                    previous_slow,
                                    waiting_duration: waiting_snapshot.active,
                                    remaining: delay
                                        .checked_sub(waiting_snapshot.active)
                                        .unwrap_or_default(),
                                },
                                // This field is ignored but our data model
                                // requires it.
                                ChildExecutionOutput::Output {
                                    result: None,
                                    output: ChildOutput::Split(ChildSplitOutput {
                                        stdout: None,
                                        stderr: None,
                                    }),
                                    errors: None,
                                },
                            ),
                        );
                    }
                }
            }
        }
    }
}

/// After a child process has exited, detect if it leaked file handles by
/// leaving long-running grandchildren open.
///
/// This is done by waiting for a short period of time after the child has
/// exited, and checking if stdout and stderr are still open. In the future, we
/// could do more sophisticated checks around e.g. if any processes with the
/// same PGID are around.
async fn detect_fd_leaks<'a>(
    cx: &UnitContext<'a>,
    child_pid: u32,
    child_acc: &mut ChildAccumulator,
    tentative_result: Option<ExecutionResult>,
    leak_timeout: Duration,
    stopwatch: &mut StopwatchStart,
    req_rx: &mut UnboundedReceiver<RunUnitRequest<'a>>,
) -> bool {
    loop {
        // Ignore stop and continue events here since the leak timeout should be very small.
        // TODO: we may want to consider them.
        let mut sleep = std::pin::pin!(tokio::time::sleep(leak_timeout));
        let waiting_stopwatch = crate::time::stopwatch();

        tokio::select! {
            // All of the branches here need to check for
            // `!child_acc.fds.is_done()`, because if child_fds is done we want
            // to hit the `else` block right away.
            () = child_acc.fill_buf(), if !child_acc.fds.is_done() => {}
            () = &mut sleep, if !child_acc.fds.is_done() => {
                break true;
            }
            recv = req_rx.recv(), if !child_acc.fds.is_done() => {
                // The sender stays open longer than the whole loop, and the
                // buffer is big enough for all messages ever sent through this
                // channel, so a RecvError should never happen.
                let req = recv.expect("a RecvError should never happen here");

                match req {
                    RunUnitRequest::Signal(_) => {
                        // The process is done executing, so signals are moot.
                    }
                    RunUnitRequest::OtherCancel => {
                        // Ignore non-signal cancellation requests -- let the
                        // unit finish.
                    }
                    RunUnitRequest::Query(RunUnitQuery::GetInfo(sender)) => {
                        let snapshot = waiting_stopwatch.snapshot();
                        let resp = cx.info_response(
                            UnitState::Exiting {
                                // Because we've polled that the child is done,
                                // child.id() will likely return None at this
                                // point. Use the cached PID since this is just
                                // for reporting.
                                pid: child_pid,
                                time_taken: stopwatch.snapshot().active,
                                slow_after: cx.slow_after,
                                tentative_result,
                                waiting_duration: snapshot.active,
                                remaining: leak_timeout
                                    .checked_sub(snapshot.active)
                                    .unwrap_or_default(),
                            },
                            child_acc.snapshot_in_progress(cx.packet.kind().waiting_on_message()),
                        );

                        _ = sender.send(resp);
                    }
                }
            }
            else => {
                break false;
            }
        }
    }
}

// It would be nice to fix this function to not have so many arguments, but this
// code is actively being refactored right now and imposing too much structure
// can cause more harm than good.
#[expect(clippy::too_many_arguments)]
async fn handle_signal_request<'a>(
    cx: &UnitContext<'a>,
    child: &mut Child,
    child_acc: &mut ChildAccumulator,
    req: SignalRequest,
    stopwatch: &mut StopwatchStart,
    // These annotations are needed to silence lints on non-Unix platforms.
    //
    // It would be nice to use an expect lint here, but Rust 1.81 appears to
    // have a bug where it complains about expectations not being fulfilled on
    // Windows, even though they are in reality. The bug is fixed in Rust 1.83,
    // so we should switch to expect after the MSRV is bumped to 1.83+.
    #[cfg_attr(not(unix), allow(unused_mut, unused_variables))] mut interval_sleep: Pin<
        &mut PausableSleep,
    >,
    req_rx: &mut UnboundedReceiver<RunUnitRequest<'a>>,
    job: Option<&super::os::Job>,
    grace_period: Duration,
) -> HandleSignalResult {
    match req {
        #[cfg(unix)]
        SignalRequest::Stop(sender) => {
            // It isn't possible to receive a stop event twice since it gets
            // debounced in the main signal handler.
            stopwatch.pause();
            interval_sleep.as_mut().pause();
            super::os::job_control_child(child, crate::signal::JobControlEvent::Stop);
            // The receiver being dead probably means the main thread panicked
            // or similar.
            let _ = sender.send(());
            HandleSignalResult::JobControl
        }
        #[cfg(unix)]
        SignalRequest::Continue => {
            // It's possible to receive a resume event right at the beginning of
            // test execution, so debounce it.
            if stopwatch.is_paused() {
                stopwatch.resume();
                interval_sleep.as_mut().resume();
                super::os::job_control_child(child, crate::signal::JobControlEvent::Continue);
            }
            HandleSignalResult::JobControl
        }
        SignalRequest::Shutdown(event) => {
            let res = super::os::terminate_child(
                cx,
                child,
                child_acc,
                InternalTerminateReason::Signal(event),
                stopwatch,
                req_rx,
                job,
                grace_period,
            )
            .await;
            HandleSignalResult::Terminated(res)
        }
    }
}

fn create_execution_result(
    exit_status: ExitStatus,
    child_errors: &[ChildFdError],
    leaked: bool,
) -> ExecutionResult {
    if !child_errors.is_empty() {
        // If an error occurred while waiting on the child handles, treat it as
        // an execution failure.
        ExecutionResult::ExecFail
    } else if exit_status.success() {
        if leaked {
            ExecutionResult::Leak
        } else {
            ExecutionResult::Pass
        }
    } else {
        ExecutionResult::Fail {
            abort_status: AbortStatus::extract(exit_status),
            leaked,
        }
    }
}
