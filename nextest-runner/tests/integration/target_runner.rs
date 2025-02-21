// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::fixtures::*;
use camino::Utf8Path;
use color_eyre::{eyre::ensure, Result};
use fixture_data::nextest_tests::EXPECTED_TEST_SUITES;
use nextest_runner::{
    cargo_config::{CargoConfigs, TargetTriple},
    config::NextestConfig,
    double_spawn::DoubleSpawnInfo,
    input::InputHandlerKind,
    platform::{BuildPlatforms, HostPlatform, PlatformLibdir, TargetPlatform},
    reporter::events::{FinalRunStats, RunStatsFailureKind},
    runner::TestRunnerBuilder,
    signal::SignalHandlerKind,
    target_runner::{PlatformRunner, TargetRunner},
    test_filter::{RunIgnored, TestFilterBuilder},
    RustcCli,
};
use std::env;
use target_spec::Platform;

fn runner_for_target(triple: Option<&str>) -> Result<(BuildPlatforms, TargetRunner)> {
    let configs = CargoConfigs::new_with_isolation(
        Vec::<String>::new(),
        &workspace_root(),
        &workspace_root(),
        Vec::new(),
    )
    .unwrap();

    let build_platforms = {
        let host = HostPlatform::detect(PlatformLibdir::from_rustc_stdout(
            RustcCli::print_host_libdir().read(),
        ))?;
        let target = if let Some(triple) = TargetTriple::find(&configs, triple)? {
            let libdir =
                PlatformLibdir::from_rustc_stdout(RustcCli::print_target_libdir(&triple).read());
            Some(TargetPlatform::new(triple, libdir))
        } else {
            None
        };
        BuildPlatforms { host, target }
    };

    let target_runner = TargetRunner::new(&configs, &build_platforms)?;
    Ok((build_platforms, target_runner))
}

#[test]
fn parses_cargo_env() {
    set_env_vars();
    std::env::set_var(current_runner_env_var(), "cargo_with_default --arg --arg2");

    let (_, def_runner) = runner_for_target(None).unwrap();

    for (_, platform_runner) in def_runner.all_build_platforms() {
        let platform_runner = platform_runner.expect("env var means runner should be defined");
        assert_eq!("cargo_with_default", platform_runner.binary());
        assert_eq!(
            vec!["--arg", "--arg2"],
            platform_runner.args().collect::<Vec<_>>()
        );
    }

    std::env::set_var(
        "CARGO_TARGET_AARCH64_LINUX_ANDROID_RUNNER",
        "cargo_with_specific",
    );

    let (_, specific_runner) = runner_for_target(Some("aarch64-linux-android")).unwrap();

    let platform_runner = specific_runner.target().unwrap();
    assert_eq!("cargo_with_specific", platform_runner.binary());
    assert_eq!(0, platform_runner.args().count());
}

fn parse_triple(triple: &'static str) -> target_spec::Platform {
    target_spec::Platform::new(triple, target_spec::TargetFeatures::Unknown).unwrap()
}

#[test]
fn parses_cargo_config_exact() {
    let workspace_root = workspace_root();
    let windows = parse_triple("x86_64-pc-windows-gnu");
    let configs = CargoConfigs::new_with_isolation(
        Vec::<String>::new(),
        &workspace_root,
        &workspace_root,
        Vec::new(),
    )
    .unwrap();
    let runner = PlatformRunner::find_config(&configs, &windows)
        .unwrap()
        .unwrap();

    assert_eq!("wine", runner.binary());
    assert_eq!(0, runner.args().count());
}

#[test]
fn disregards_non_matching() {
    let workspace_root = workspace_root();
    let windows = parse_triple("x86_64-unknown-linux-gnu");
    let configs = CargoConfigs::new_with_isolation(
        Vec::<String>::new(),
        &workspace_root,
        &workspace_root,
        Vec::new(),
    )
    .unwrap();
    assert!(PlatformRunner::find_config(&configs, &windows)
        .unwrap()
        .is_none());
}

#[test]
fn parses_cargo_config_cfg() {
    let workspace_root = workspace_root();
    let android = parse_triple("aarch64-linux-android");
    let configs = CargoConfigs::new_with_isolation(
        Vec::<String>::new(),
        &workspace_root,
        &workspace_root,
        Vec::new(),
    )
    .unwrap();
    let runner = PlatformRunner::find_config(&configs, &android)
        .unwrap()
        .unwrap();

    assert_eq!("android-runner", runner.binary());
    assert_eq!(vec!["-x"], runner.args().collect::<Vec<_>>());

    let linux = parse_triple("x86_64-unknown-linux-musl");
    let runner = PlatformRunner::find_config(&configs, &linux)
        .unwrap()
        .unwrap();

    assert_eq!("passthrough", runner.binary());
    assert_eq!(
        vec!["--ensure-this-arg-is-sent"],
        runner.args().collect::<Vec<_>>()
    );
}

#[test]
fn falls_back_to_cargo_config() {
    let linux = parse_triple("x86_64-unknown-linux-musl");
    std::env::set_var(
        "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUNNER",
        "cargo-runner-windows",
    );

    let (_, target_runner) = runner_for_target(Some(linux.triple_str())).unwrap();

    let platform_runner = target_runner.target().unwrap();

    assert_eq!("passthrough", platform_runner.binary());
    assert_eq!(
        vec!["--ensure-this-arg-is-sent"],
        platform_runner.args().collect::<Vec<_>>()
    );
}

fn passthrough_path() -> &'static Utf8Path {
    Utf8Path::new(env!("CARGO_BIN_EXE_passthrough"))
}

fn current_runner_env_var() -> String {
    PlatformRunner::runner_env_var(
        &Platform::build_target().expect("current platform is known to target-spec"),
    )
}

#[test]
fn test_listing_with_target_runner() -> Result<()> {
    set_env_vars();

    let test_filter = TestFilterBuilder::default_set(RunIgnored::Default);
    let test_list = FIXTURE_TARGETS.make_test_list(
        NextestConfig::DEFAULT_PROFILE,
        &test_filter,
        &TargetRunner::empty(),
    )?;

    let bin_count = test_list.binary_count();
    let test_count = test_list.test_count();

    {
        std::env::set_var(
            current_runner_env_var(),
            format!("{} --ensure-this-arg-is-sent", passthrough_path()),
        );
        let (_, target_runner) = runner_for_target(None).unwrap();

        let test_list = FIXTURE_TARGETS.make_test_list(
            NextestConfig::DEFAULT_PROFILE,
            &test_filter,
            &target_runner,
        )?;

        assert_eq!(bin_count, test_list.binary_count());
        assert_eq!(test_count, test_list.test_count());
    }

    {
        // cargo unfortunately doesn't handle relative paths for runner binaries,
        // it will just assume they are in PATH if they are not absolute paths,
        // and thus makes testing it a bit annoying, so we just punt and rely
        // on the tests for parsing the runner in the proper precedence
    }

    Ok(())
}

#[test]
fn test_run_with_target_runner() -> Result<()> {
    set_env_vars();

    let test_filter = TestFilterBuilder::default_set(RunIgnored::Default);

    std::env::set_var(
        current_runner_env_var(),
        format!("{} --ensure-this-arg-is-sent", passthrough_path()),
    );
    let (build_platforms, target_runner) = runner_for_target(None).unwrap();

    for (_, platform_runner) in target_runner.all_build_platforms() {
        let runner = platform_runner.expect("current platform runner was set through env var");
        assert_eq!(passthrough_path(), runner.binary());
    }

    let test_list = FIXTURE_TARGETS.make_test_list(
        NextestConfig::DEFAULT_PROFILE,
        &test_filter,
        &target_runner,
    )?;

    let config = load_config();
    let profile = config
        .profile(NextestConfig::DEFAULT_PROFILE)
        .expect("default config is valid")
        .into_evaluatable(&build_platforms);

    let runner = TestRunnerBuilder::default();
    let runner = runner
        .build(
            &test_list,
            &profile,
            vec![],
            SignalHandlerKind::Noop,
            InputHandlerKind::Noop,
            DoubleSpawnInfo::disabled(),
            target_runner,
        )
        .unwrap();

    let (instance_statuses, run_stats) = execute_collect(runner);

    for (name, expected) in &*EXPECTED_TEST_SUITES {
        let test_binary = FIXTURE_TARGETS
            .test_artifacts
            .get(name)
            .unwrap_or_else(|| panic!("unexpected test name {name}"));
        for fixture in &expected.test_cases {
            let instance_value = instance_statuses
                .get(&(test_binary.binary_path.as_path(), fixture.name))
                .unwrap_or_else(|| {
                    panic!(
                        "no instance status found for key ({}, {})",
                        test_binary.binary_path.as_path(),
                        fixture.name
                    )
                });
            let valid = match &instance_value.status {
                InstanceStatus::Skipped(_) => {
                    ensure!(fixture.status.is_ignored(), "test should be skipped");
                    Ok(())
                }
                InstanceStatus::Finished(run_statuses) => {
                    // This test should not have been retried since retries aren't configured.
                    assert_eq!(
                        run_statuses.len(),
                        1,
                        "test {} should have been run exactly once",
                        fixture.name
                    );
                    let run_status = run_statuses.last_status();

                    cfg_if::cfg_if! {
                        if #[cfg(unix)] {
                            // On Unix, segfaults aren't passed through by the
                            // passthrough runner.
                            if fixture.status == fixture_data::models::TestCaseFixtureStatus::Segfault {
                                ensure_execution_result(
                                    &run_status.result,
                                    fixture_data::models::TestCaseFixtureStatus::Fail,
                                    1,
                                )
                            } else {
                                ensure_execution_result(&run_status.result, fixture.status, 1)
                            }
                        } else if #[cfg(windows)] {
                            ensure_execution_result(&run_status.result, fixture.status, 1)
                        } else {
                            compile_error!("unsupported platform")
                        }
                    }
                }
            };
            if let Err(error) = valid {
                panic!(
                    "for test {}, mismatch in status: expected {:?}, actual {:?}, error: {}",
                    fixture.name, fixture.status, instance_value.status, error
                );
            }
        }
    }

    // Note: can't compare not_run because its exact value would depend on the number of threads on
    // the machine.
    assert!(
        matches!(
            run_stats.summarize_final(),
            FinalRunStats::Failed(RunStatsFailureKind::Test { .. })
        ),
        "run should be marked failed, but got {:?}",
        run_stats.summarize_final(),
    );

    Ok(())
}
