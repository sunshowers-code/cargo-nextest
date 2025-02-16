// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Code to generate JUnit XML reports from test events.

use crate::{
    config::{JunitConfig, ScriptId},
    errors::{DisplayErrorChain, JunitSetupError, WriteEventError},
    list::TestInstanceId,
    reporter::{
        events::{ExecutionDescription, ExecutionResult, TestEvent, TestEventKind, UnitKind},
        UnitErrorDescription,
    },
    test_output::{ChildExecutionOutput, ChildOutput},
};
use camino::Utf8PathBuf;
use debug_ignore::DebugIgnore;
use indexmap::IndexMap;
use nextest_metadata::RustBinaryId;
use quick_junit::{
    NonSuccessKind, Report, TestCase, TestCaseStatus, TestRerun, TestSuite, XmlString,
};
use std::{fmt, fs::File};

static STDOUT_STDERR_COMBINED: &str = "(stdout and stderr are combined)";
static STDOUT_NOT_CAPTURED: &str = "(stdout not captured)";
static STDERR_NOT_CAPTURED: &str = "(stderr not captured)";
static PROCESS_FAILED_TO_START: &str = "(process failed to start)";

#[derive(Clone, Debug)]
pub(super) struct MetadataJunit<'cfg> {
    junit_path: Utf8PathBuf,
    config: JunitConfig<'cfg>,
    test_suites: DebugIgnore<IndexMap<SuiteKey<'cfg>, TestSuite>>,
}

impl<'cfg> MetadataJunit<'cfg> {
    pub(super) fn new(
        store_dir: Utf8PathBuf,
        config: JunitConfig<'cfg>,
    ) -> Result<Self, JunitSetupError> {
        let junit_path = config.path(&store_dir);
        let junit_dir = junit_path.parent().expect("junit path must have a parent");
        std::fs::create_dir_all(junit_dir).map_err(|error| JunitSetupError::CreateStoreDir {
            path: junit_dir.to_path_buf(),
            error,
        })?;

        Ok(Self {
            junit_path,
            config,
            test_suites: DebugIgnore(IndexMap::new()),
        })
    }

    pub(super) fn write_event(&mut self, event: TestEvent<'cfg>) -> Result<(), WriteEventError> {
        match event.kind {
            TestEventKind::RunStarted { .. }
            | TestEventKind::RunPaused { .. }
            | TestEventKind::RunContinued { .. } => {}
            TestEventKind::SetupScriptStarted { .. } | TestEventKind::SetupScriptSlow { .. } => {}
            TestEventKind::SetupScriptFinished {
                index: _,
                total: _,
                script_id,
                command,
                args,
                junit_store_success_output,
                junit_store_failure_output,
                no_capture: _,
                run_status,
            } => {
                let is_success = run_status.result.is_success();

                let test_suite = self.testsuite_for_setup_script(script_id.clone());
                let testcase_status = if is_success {
                    TestCaseStatus::success()
                } else {
                    let (kind, ty) = non_success_kind_and_type(UnitKind::Script, run_status.result);
                    let mut testcase_status = TestCaseStatus::non_success(kind);
                    testcase_status.set_type(ty);
                    testcase_status
                };

                let mut testcase =
                    TestCase::new(script_id.as_identifier().as_str(), testcase_status);
                // classname doesn't quite make sense for setup scripts, but it
                // is required by the spec at https://llg.cubic.org/docs/junit/.
                // We use the same name as the test suite.
                testcase
                    .set_classname(test_suite.name.clone())
                    .set_timestamp(run_status.start_time)
                    .set_time(run_status.time_taken);

                let store_stdout_stderr = (junit_store_success_output && is_success)
                    || (junit_store_failure_output && !is_success);

                set_execute_status_props(
                    &run_status.output,
                    store_stdout_stderr,
                    TestcaseOrRerun::Testcase(&mut testcase),
                );

                test_suite.add_test_case(testcase);

                // Add properties corresponding to the setup script.
                test_suite.add_property(("command", command));
                test_suite.add_property(("args".to_owned(), shell_words::join(args)));
                // Also add environment variables set by the script.
                if let Some(env_map) = run_status.env_map {
                    for (key, value) in env_map.env_map {
                        test_suite.add_property((format!("output-env:{key}"), value));
                    }
                }
            }
            TestEventKind::InfoStarted { .. }
            | TestEventKind::InfoResponse { .. }
            | TestEventKind::InfoFinished { .. } => {}
            TestEventKind::InputEnter { .. } => {}
            TestEventKind::TestStarted { .. } => {}
            TestEventKind::TestSlow { .. } => {}
            TestEventKind::TestAttemptFailedWillRetry { .. }
            | TestEventKind::TestRetryStarted { .. } => {
                // Retries are recorded in TestFinished.
            }
            TestEventKind::TestFinished {
                test_instance,
                run_statuses,
                junit_store_success_output,
                junit_store_failure_output,
                ..
            } => {
                let testsuite = self.testsuite_for_test(test_instance.id());

                let (mut testcase_status, main_status, reruns) = match run_statuses.describe() {
                    ExecutionDescription::Success { single_status } => {
                        (TestCaseStatus::success(), single_status, &[][..])
                    }
                    ExecutionDescription::Flaky {
                        last_status,
                        prior_statuses,
                    } => (TestCaseStatus::success(), last_status, prior_statuses),
                    ExecutionDescription::Failure {
                        first_status,
                        retries,
                        ..
                    } => {
                        let (kind, ty) =
                            non_success_kind_and_type(UnitKind::Test, first_status.result);
                        let mut testcase_status = TestCaseStatus::non_success(kind);
                        testcase_status.set_type(ty);
                        (testcase_status, first_status, retries)
                    }
                };

                for rerun in reruns {
                    let (kind, ty) = non_success_kind_and_type(UnitKind::Test, rerun.result);
                    let mut test_rerun = TestRerun::new(kind);
                    test_rerun
                        .set_timestamp(rerun.start_time)
                        .set_time(rerun.time_taken)
                        .set_type(ty);

                    set_execute_status_props(
                        &rerun.output,
                        junit_store_failure_output,
                        TestcaseOrRerun::Rerun(&mut test_rerun),
                    );

                    // TODO: also publish time? it won't be standard JUnit (but maybe that's ok?)
                    testcase_status.add_rerun(test_rerun);
                }

                let mut testcase = TestCase::new(test_instance.name, testcase_status);
                testcase
                    .set_classname(test_instance.suite_info.binary_id.as_str())
                    .set_timestamp(main_status.start_time)
                    .set_time(main_status.time_taken);

                // TODO: allure seems to want the output to be in a format where text files are
                // written out to disk:
                // https://github.com/allure-framework/allure2/blob/master/plugins/junit-xml-plugin/src/main/java/io/qameta/allure/junitxml/JunitXmlPlugin.java#L192-L196
                // we may have to update this format to handle that.
                let is_success = main_status.result.is_success();
                let store_stdout_stderr = (junit_store_success_output && is_success)
                    || (junit_store_failure_output && !is_success);

                set_execute_status_props(
                    &main_status.output,
                    store_stdout_stderr,
                    TestcaseOrRerun::Testcase(&mut testcase),
                );

                testsuite.add_test_case(testcase);
            }
            TestEventKind::TestSkipped { .. } => {
                // TODO: report skipped tests? causes issues if we want to aggregate runs across
                // skipped and non-skipped tests. Probably needs to be made configurable.

                // let testsuite = self.testsuite_for(test_instance);
                //
                // let mut testcase_status = TestcaseStatus::skipped();
                // testcase_status.set_message(format!("Skipped: {}", reason));
                // let testcase = Testcase::new(test_instance.name, testcase_status);
                //
                // testsuite.add_testcase(testcase);
            }
            TestEventKind::RunBeginCancel { .. } | TestEventKind::RunBeginKill { .. } => {}
            TestEventKind::RunFinished {
                run_id,
                start_time,
                elapsed,
                ..
            } => {
                // Write out the report to the given file.
                let mut report = Report::new(self.config.report_name());
                report
                    .set_report_uuid(run_id)
                    .set_timestamp(start_time)
                    .set_time(elapsed)
                    .add_test_suites(self.test_suites.drain(..).map(|(_, testsuite)| testsuite));

                let f = File::create(&self.junit_path).map_err(|error| WriteEventError::Fs {
                    file: self.junit_path.clone(),
                    error,
                })?;
                report
                    .serialize(f)
                    .map_err(|error| WriteEventError::Junit {
                        file: self.junit_path.clone(),
                        error,
                    })?;
            }
        }

        Ok(())
    }

    fn testsuite_for_setup_script(&mut self, script_id: ScriptId) -> &mut TestSuite {
        let key = SuiteKey::SetupScript(script_id.clone());
        self.test_suites
            .entry(key.clone())
            .or_insert_with(|| TestSuite::new(key.to_string()))
    }

    fn testsuite_for_test(&mut self, test_instance: TestInstanceId<'cfg>) -> &mut TestSuite {
        let key = SuiteKey::TestBinary(test_instance.binary_id);
        self.test_suites
            .entry(key.clone())
            .or_insert_with(|| TestSuite::new(key.to_string()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
enum SuiteKey<'cfg> {
    // Each script gets a separate suite, because in the future we'll likely want to set u
    SetupScript(ScriptId),
    TestBinary(&'cfg RustBinaryId),
}

impl fmt::Display for SuiteKey<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SuiteKey::SetupScript(script_id) => write!(f, "@setup-script:{}", script_id),
            SuiteKey::TestBinary(binary_id) => write!(f, "{}", binary_id),
        }
    }
}

fn non_success_kind_and_type(kind: UnitKind, result: ExecutionResult) -> (NonSuccessKind, String) {
    match result {
        ExecutionResult::Fail {
            abort_status: Some(_),
            leaked: true,
        } => (
            NonSuccessKind::Failure,
            format!("{kind} abort with leaked handles"),
        ),
        ExecutionResult::Fail {
            abort_status: Some(_),
            leaked: false,
        } => (NonSuccessKind::Failure, format!("{kind} abort")),
        ExecutionResult::Fail {
            abort_status: None,
            leaked: true,
        } => (
            NonSuccessKind::Failure,
            format!("{kind} failure with leaked handles"),
        ),
        ExecutionResult::Fail {
            abort_status: None,
            leaked: false,
        } => (NonSuccessKind::Failure, format!("{kind} failure")),
        ExecutionResult::Timeout => (NonSuccessKind::Failure, format!("{kind} timeout")),
        ExecutionResult::ExecFail => (NonSuccessKind::Error, "execution failure".to_owned()),
        ExecutionResult::Leak => (
            NonSuccessKind::Error,
            format!("{kind} passed but leaked handles"),
        ),
        ExecutionResult::Pass => {
            unreachable!("this is a failure status")
        }
    }
}

enum TestcaseOrRerun<'a> {
    Testcase(&'a mut TestCase),
    Rerun(&'a mut TestRerun),
}

impl TestcaseOrRerun<'_> {
    fn set_message(&mut self, message: impl Into<XmlString>) -> &mut Self {
        match self {
            TestcaseOrRerun::Testcase(testcase) => {
                testcase.status.set_message(message.into());
            }
            TestcaseOrRerun::Rerun(rerun) => {
                rerun.set_message(message.into());
            }
        }
        self
    }

    fn set_description(&mut self, description: impl Into<XmlString>) -> &mut Self {
        match self {
            TestcaseOrRerun::Testcase(testcase) => {
                testcase.status.set_description(description.into());
            }
            TestcaseOrRerun::Rerun(rerun) => {
                rerun.set_description(description.into());
            }
        }
        self
    }

    fn set_system_out(&mut self, system_out: impl Into<XmlString>) -> &mut Self {
        match self {
            TestcaseOrRerun::Testcase(testcase) => {
                testcase.set_system_out(system_out.into());
            }
            TestcaseOrRerun::Rerun(rerun) => {
                rerun.set_system_out(system_out.into());
            }
        }
        self
    }

    fn set_system_err(&mut self, system_err: impl Into<XmlString>) -> &mut Self {
        match self {
            TestcaseOrRerun::Testcase(testcase) => {
                testcase.set_system_err(system_err.into());
            }
            TestcaseOrRerun::Rerun(rerun) => {
                rerun.set_system_err(system_err.into());
            }
        }
        self
    }
}

fn set_execute_status_props(
    exec_output: &ChildExecutionOutput,
    store_stdout_stderr: bool,
    mut out: TestcaseOrRerun<'_>,
) {
    // Currently we only aggregate test results, so always specify UnitKind::Test.
    let description = UnitErrorDescription::new(UnitKind::Test, exec_output);
    if let Some(errors) = description.all_error_list() {
        out.set_message(errors.short_message());
        out.set_description(DisplayErrorChain::new(errors).to_string());
    }

    if store_stdout_stderr {
        match exec_output {
            ChildExecutionOutput::Output {
                output: ChildOutput::Split(split),
                ..
            } => {
                if let Some(stdout) = &split.stdout {
                    out.set_system_out(stdout.as_str_lossy());
                } else {
                    out.set_system_out(STDOUT_NOT_CAPTURED);
                }
                if let Some(stderr) = &split.stderr {
                    out.set_system_err(stderr.as_str_lossy());
                } else {
                    out.set_system_err(STDERR_NOT_CAPTURED);
                }
            }
            ChildExecutionOutput::Output {
                output: ChildOutput::Combined { output },
                ..
            } => {
                out.set_system_out(output.as_str_lossy())
                    .set_system_err(STDOUT_STDERR_COMBINED);
            }
            ChildExecutionOutput::StartError(_) => {
                out.set_system_out(PROCESS_FAILED_TO_START)
                    .set_system_err(PROCESS_FAILED_TO_START);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::reporter::events::AbortStatus;
    use crate::{
        errors::{ChildError, ChildFdError, ChildStartError, ErrorList},
        test_output::ChildSplitOutput,
    };
    use bytes::Bytes;
    use std::{io, sync::Arc};

    #[test]
    fn test_set_execute_status_props() {
        let cases = [
            ExecuteStatusPropsCase {
                comment: "success + combined + store",
                status: TestCaseStatus::success(),
                output: ChildExecutionOutput::Output {
                    result: Some(ExecutionResult::Pass),
                    output: ChildOutput::Combined {
                        output: Bytes::from("stdout\nstderr").into(),
                    },
                    errors: None,
                },
                store_stdout_stderr: true,
                message: None,
                description: None,
                system_out: Some("stdout\nstderr"),
                system_err: Some(STDOUT_STDERR_COMBINED),
            },
            ExecuteStatusPropsCase {
                comment: "success + combined + no store",
                status: TestCaseStatus::success(),
                output: ChildExecutionOutput::Output {
                    result: Some(ExecutionResult::Pass),
                    output: ChildOutput::Combined {
                        output: Bytes::from("stdout\nstderr").into(),
                    },
                    errors: None,
                },
                store_stdout_stderr: false,
                message: None,
                description: None,
                system_out: None,
                system_err: None,
            },
            ExecuteStatusPropsCase {
                comment: "success + split + store",
                status: TestCaseStatus::success(),
                output: ChildExecutionOutput::Output {
                    result: Some(ExecutionResult::Pass),
                    output: ChildOutput::Split(ChildSplitOutput {
                        stdout: Some(Bytes::from("stdout").into()),
                        stderr: Some(Bytes::from("stderr").into()),
                    }),
                    errors: None,
                },
                store_stdout_stderr: true,
                message: None,
                description: None,
                system_out: Some("stdout"),
                system_err: Some("stderr"),
            },
            // success + split + no store is not hugely important to test --
            // it's just another combination of the above.
            ExecuteStatusPropsCase {
                comment: "failure + combined + store",
                status: TestCaseStatus::non_success(NonSuccessKind::Failure),
                output: ChildExecutionOutput::Output {
                    result: Some(ExecutionResult::Fail {
                        abort_status: None,
                        leaked: true,
                    }),
                    output: ChildOutput::Combined {
                        output: Bytes::from(
                            "stdout\nstderr\nthread 'foo' panicked at xyz.rs:40:\nstrange\n\
                             extra\nextra2",
                        )
                        .into(),
                    },
                    errors: None,
                },
                store_stdout_stderr: true,
                message: Some("thread 'foo' panicked at xyz.rs:40"),
                description: Some("thread 'foo' panicked at xyz.rs:40:\nstrange\nextra\nextra2"),
                system_out: Some(
                    "stdout\nstderr\nthread 'foo' panicked at xyz.rs:40:\nstrange\n\
                     extra\nextra2",
                ),
                system_err: Some(STDOUT_STDERR_COMBINED),
            },
            ExecuteStatusPropsCase {
                comment: "failure + split + no store",
                status: TestCaseStatus::non_success(NonSuccessKind::Failure),
                output: ChildExecutionOutput::Output {
                    result: Some(ExecutionResult::Fail {
                        abort_status: None,
                        leaked: false,
                    }),
                    output: ChildOutput::Split(ChildSplitOutput {
                        stdout: None,
                        stderr: Some(
                            Bytes::from(
                                "stdout\nstderr\nthread 'foo' panicked at xyz.rs:40:\n\
                                 strange\nextra\nextra2",
                            )
                            .into(),
                        ),
                    }),
                    errors: None,
                },
                store_stdout_stderr: false,
                message: Some("thread 'foo' panicked at xyz.rs:40"),
                description: Some(
                    "thread 'foo' panicked at xyz.rs:40:\n\
                     strange\nextra\nextra2",
                ),
                system_out: None,
                system_err: None,
            },
            #[cfg(unix)]
            ExecuteStatusPropsCase {
                comment: "abort + split + store (unix)",
                status: TestCaseStatus::non_success(NonSuccessKind::Failure),
                output: ChildExecutionOutput::Output {
                    result: Some(ExecutionResult::Fail {
                        abort_status: Some(AbortStatus::UnixSignal(libc::SIGTERM)),
                        leaked: false,
                    }),
                    output: ChildOutput::Split(ChildSplitOutput {
                        stdout: Some(Bytes::from("stdout\nstdout 2\n").into()),
                        stderr: None,
                    }),
                    errors: None,
                },
                store_stdout_stderr: true,
                message: Some("process aborted with signal 15 (SIGTERM)"),
                description: Some("process aborted with signal 15 (SIGTERM)"),
                system_out: Some("stdout\nstdout 2\n"),
                system_err: Some(STDERR_NOT_CAPTURED),
            },
            #[cfg(unix)]
            ExecuteStatusPropsCase {
                comment: "abort + multiple errors + no store (unix)",
                status: TestCaseStatus::non_success(NonSuccessKind::Failure),
                output: ChildExecutionOutput::Output {
                    result: Some(ExecutionResult::Fail {
                        abort_status: Some(AbortStatus::UnixSignal(libc::SIGTERM)),
                        leaked: true,
                    }),
                    output: ChildOutput::Split(ChildSplitOutput {
                        stdout: None,
                        stderr: Some(
                            Bytes::from("stdout\nthread 'foo' panicked at xyz.rs:40").into(),
                        ),
                    }),
                    errors: ErrorList::new(
                        "collecting child output",
                        vec![ChildError::Fd(ChildFdError::Wait(Arc::new(
                            io::Error::new(io::ErrorKind::Other, "huh"),
                        )))],
                    ),
                },
                store_stdout_stderr: false,
                message: Some("3 errors occurred executing test"),
                description: Some(indoc::indoc! {"
                    3 errors occurred executing test:
                    * error waiting for child process to exit
                        caused by:
                        - huh
                    * process aborted with signal 15 (SIGTERM), and also leaked handles
                    * thread 'foo' panicked at xyz.rs:40
                "}),
                system_out: None,
                system_err: None,
            },
            ExecuteStatusPropsCase {
                comment: "multiple errors + store",
                status: TestCaseStatus::non_success(NonSuccessKind::Failure),
                output: ChildExecutionOutput::Output {
                    result: Some(ExecutionResult::Fail {
                        abort_status: None,
                        leaked: false,
                    }),
                    output: ChildOutput::Split(ChildSplitOutput {
                        stdout: None,
                        stderr: Some(
                            Bytes::from("stdout\nthread 'foo' panicked at xyz.rs:40").into(),
                        ),
                    }),
                    errors: ErrorList::new(
                        "collecting child output",
                        vec![ChildError::Fd(ChildFdError::ReadStdout(Arc::new(
                            io::Error::new(io::ErrorKind::Other, "stdout error"),
                        )))],
                    ),
                },
                store_stdout_stderr: false,
                message: Some("2 errors occurred executing test"),
                description: Some(indoc::indoc! {"
                    2 errors occurred executing test:
                    * error reading standard output
                        caused by:
                        - stdout error
                    * thread 'foo' panicked at xyz.rs:40
                "}),
                system_out: None,
                system_err: None,
            },
            ExecuteStatusPropsCase {
                comment: "exec fail + combined + store (exec fail means nothing to store)",
                status: TestCaseStatus::non_success(NonSuccessKind::Error),
                output: ChildExecutionOutput::StartError(ChildStartError::Spawn(Arc::new(
                    io::Error::new(io::ErrorKind::Other, "start error"),
                ))),
                store_stdout_stderr: true,
                message: Some("error spawning child process"),
                description: Some(indoc::indoc! {"
                    error spawning child process
                      caused by:
                      - start error"
                }),
                system_out: Some(PROCESS_FAILED_TO_START),
                system_err: Some(PROCESS_FAILED_TO_START),
            },
        ];

        for case in cases {
            eprintln!("** testing: {}", case.comment);

            let mut testcase = TestCase::new("test", case.status);
            set_execute_status_props(
                &case.output,
                case.store_stdout_stderr,
                TestcaseOrRerun::Testcase(&mut testcase),
            );
            assert_eq!(
                get_message(&testcase.status),
                case.message,
                "message matches"
            );
            assert_eq!(
                get_description(&testcase.status),
                case.description,
                "description matches"
            );
            assert_eq!(
                testcase.system_out.as_ref().map(|s| s.as_str()),
                case.system_out,
                "system_out matches"
            );
            assert_eq!(
                testcase.system_err.as_ref().map(|s| s.as_str()),
                case.system_err,
                "system_err matches"
            );
        }
    }

    #[derive(Debug)]
    struct ExecuteStatusPropsCase<'a> {
        comment: &'a str,
        status: TestCaseStatus,
        output: ChildExecutionOutput,
        store_stdout_stderr: bool,
        message: Option<&'a str>,
        description: Option<&'a str>,
        system_out: Option<&'a str>,
        system_err: Option<&'a str>,
    }

    fn get_message(status: &TestCaseStatus) -> Option<&str> {
        match status {
            TestCaseStatus::Success { .. } => None,
            TestCaseStatus::NonSuccess { message, .. } => message.as_deref(),
            TestCaseStatus::Skipped { message, .. } => message.as_deref(),
        }
    }

    fn get_description(status: &TestCaseStatus) -> Option<&str> {
        match status {
            TestCaseStatus::Success { .. } => None,
            TestCaseStatus::NonSuccess { description, .. } => description.as_deref(),
            TestCaseStatus::Skipped { description, .. } => description.as_deref(),
        }
    }
}
