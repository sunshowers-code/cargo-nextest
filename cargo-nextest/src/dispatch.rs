// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{
    cargo_cli::{CargoCli, CargoOptions},
    output::{should_redact, OutputContext, OutputOpts, OutputWriter, StderrStyles},
    reuse_build::{make_path_mapper, ArchiveFormatOpt, ReuseBuildOpts},
    version, ExpectedError, Result, ReuseBuildKind,
};
use camino::{Utf8Path, Utf8PathBuf};
use clap::{builder::BoolishValueParser, ArgAction, Args, Parser, Subcommand, ValueEnum};
use guppy::graph::PackageGraph;
use itertools::Itertools;
use nextest_filtering::{EvalContext, Filterset, FiltersetKind, ParseContext};
use nextest_metadata::BuildPlatform;
use nextest_runner::{
    cargo_config::{CargoConfigs, EnvironmentMap, TargetTriple},
    config::{
        get_num_cpus, ConfigExperimental, EarlyProfile, MaxFail, NextestConfig,
        NextestVersionConfig, NextestVersionEval, RetryPolicy, TestGroup, TestThreads,
        ToolConfigFile, VersionOnlyConfig,
    },
    double_spawn::DoubleSpawnInfo,
    errors::{TargetTripleError, WriteTestListError},
    input::InputHandlerKind,
    list::{
        BinaryList, OutputFormat, RustBuildMeta, RustTestArtifact, SerializableFormat,
        TestExecuteContext, TestList, TestListState,
    },
    partition::PartitionerBuilder,
    platform::{BuildPlatforms, HostPlatform, PlatformLibdir, TargetPlatform},
    redact::Redactor,
    reporter::{
        events::{FinalRunStats, RunStatsFailureKind},
        highlight_end, structured, EventAggregator, FinalStatusLevel, ReporterBuilder, StatusLevel,
        TestOutputDisplay, TestOutputErrorSlice,
    },
    reuse_build::{archive_to_file, ArchiveReporter, PathMapper, ReuseBuildInfo},
    runner::{configure_handle_inheritance, TestRunnerBuilder},
    show_config::{ShowNextestVersion, ShowTestGroupSettings, ShowTestGroups, ShowTestGroupsMode},
    signal::SignalHandlerKind,
    target_runner::{PlatformRunner, TargetRunner},
    test_filter::{FilterBound, RunIgnored, TestFilterBuilder, TestFilterPatterns},
    write_str::WriteStr,
    RustcCli,
};
use owo_colors::OwoColorize;
use quick_junit::XmlString;
use semver::Version;
use std::{
    collections::BTreeSet,
    env::VarError,
    fmt,
    io::{Cursor, Write},
    sync::{Arc, OnceLock},
};
use swrite::{swrite, SWrite};
use tracing::{debug, info, warn, Level};

/// A next-generation test runner for Rust.
///
/// This binary should typically be invoked as `cargo nextest` (in which case
/// this message will not be seen), not `cargo-nextest`.
#[derive(Debug, Parser)]
#[command(
    version = version::short(),
    long_version = version::long(),
    bin_name = "cargo",
    styles = crate::output::clap_styles::style(),
    max_term_width = 100,
)]
pub struct CargoNextestApp {
    #[clap(subcommand)]
    subcommand: NextestSubcommand,
}

impl CargoNextestApp {
    /// Initializes the output context.
    pub fn init_output(&self) -> OutputContext {
        match &self.subcommand {
            NextestSubcommand::Nextest(args) => args.common.output.init(),
            NextestSubcommand::Ntr(args) => args.common.output.init(),
            #[cfg(unix)]
            // Double-spawned processes should never use coloring.
            NextestSubcommand::DoubleSpawn(_) => OutputContext::color_never_init(),
        }
    }

    /// Executes the app.
    pub fn exec(
        self,
        cli_args: Vec<String>,
        output: OutputContext,
        output_writer: &mut OutputWriter,
    ) -> Result<i32> {
        match self.subcommand {
            NextestSubcommand::Nextest(app) => app.exec(cli_args, output, output_writer),
            NextestSubcommand::Ntr(opts) => opts.exec(cli_args, output, output_writer),
            #[cfg(unix)]
            NextestSubcommand::DoubleSpawn(opts) => opts.exec(output),
        }
    }
}

#[derive(Debug, Subcommand)]
enum NextestSubcommand {
    /// A next-generation test runner for Rust. <https://nexte.st>
    Nextest(Box<AppOpts>),
    /// Build and run tests: a shortcut for `cargo nextest run`.
    Ntr(Box<NtrOpts>),
    /// Private command, used to double-spawn test processes.
    #[cfg(unix)]
    #[command(name = nextest_runner::double_spawn::DoubleSpawnInfo::SUBCOMMAND_NAME, hide = true)]
    DoubleSpawn(crate::double_spawn::DoubleSpawnOpts),
}

#[derive(Debug, Args)]
#[clap(
    version = version::short(),
    long_version = version::long(),
    display_name = "cargo-nextest",
)]
struct AppOpts {
    #[clap(flatten)]
    common: CommonOpts,

    #[clap(subcommand)]
    command: Command,
}

impl AppOpts {
    /// Execute the command.
    ///
    /// Returns the exit code.
    fn exec(
        self,
        cli_args: Vec<String>,
        output: OutputContext,
        output_writer: &mut OutputWriter,
    ) -> Result<i32> {
        match self.command {
            Command::List {
                cargo_options,
                build_filter,
                message_format,
                list_type,
                reuse_build,
                ..
            } => {
                let base = BaseApp::new(
                    output,
                    reuse_build,
                    cargo_options,
                    self.common.config_opts,
                    self.common.manifest_path,
                    output_writer,
                )?;
                let app = App::new(base, build_filter)?;
                app.exec_list(message_format, list_type, output_writer)?;
                Ok(0)
            }
            Command::Run(run_opts) => {
                let base = BaseApp::new(
                    output,
                    run_opts.reuse_build,
                    run_opts.cargo_options,
                    self.common.config_opts,
                    self.common.manifest_path,
                    output_writer,
                )?;
                let app = App::new(base, run_opts.build_filter)?;
                app.exec_run(
                    run_opts.no_capture,
                    &run_opts.runner_opts,
                    &run_opts.reporter_opts,
                    cli_args,
                    output_writer,
                )?;
                Ok(0)
            }
            Command::Archive {
                cargo_options,
                archive_file,
                archive_format,
                zstd_level,
            } => {
                let app = BaseApp::new(
                    output,
                    ReuseBuildOpts::default(),
                    cargo_options,
                    self.common.config_opts,
                    self.common.manifest_path,
                    output_writer,
                )?;
                app.exec_archive(&archive_file, archive_format, zstd_level, output_writer)?;
                Ok(0)
            }
            Command::ShowConfig { command } => command.exec(
                self.common.manifest_path,
                self.common.config_opts,
                output,
                output_writer,
            ),
            Command::Self_ { command } => command.exec(self.common.output),
            Command::Debug { command } => command.exec(self.common.output),
        }
    }
}

// Options shared between cargo nextest and cargo ntr.
#[derive(Debug, Args)]
struct CommonOpts {
    /// Path to Cargo.toml
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        help_heading = "Manifest options"
    )]
    manifest_path: Option<Utf8PathBuf>,

    #[clap(flatten)]
    output: OutputOpts,

    #[clap(flatten)]
    config_opts: ConfigOpts,
}

#[derive(Debug, Args)]
#[command(next_help_heading = "Config options")]
struct ConfigOpts {
    /// Config file [default: workspace-root/.config/nextest.toml]
    #[arg(long, global = true, value_name = "PATH")]
    pub config_file: Option<Utf8PathBuf>,

    /// Tool-specific config files
    ///
    /// Some tools on top of nextest may want to set up their own default configuration but
    /// prioritize user configuration on top. Use this argument to insert configuration
    /// that's lower than --config-file in priority but above the default config shipped with
    /// nextest.
    ///
    /// Arguments are specified in the format "tool:abs_path", for example
    /// "my-tool:/path/to/nextest.toml" (or "my-tool:C:\\path\\to\\nextest.toml" on Windows).
    /// Paths must be absolute.
    ///
    /// This argument may be specified multiple times. Files that come later are lower priority
    /// than those that come earlier.
    #[arg(long = "tool-config-file", global = true, value_name = "TOOL:ABS_PATH")]
    pub tool_config_files: Vec<ToolConfigFile>,

    /// Override checks for the minimum version defined in nextest's config.
    ///
    /// Repository and tool-specific configuration files can specify minimum required and
    /// recommended versions of nextest. This option overrides those checks.
    #[arg(long, global = true)]
    pub override_version_check: bool,

    /// The nextest profile to use.
    ///
    /// Nextest's configuration supports multiple profiles, which can be used to set up different
    /// configurations for different purposes. (For example, a configuration for local runs and one
    /// for CI.) This option selects the profile to use.
    #[arg(
        long,
        short = 'P',
        env = "NEXTEST_PROFILE",
        global = true,
        help_heading = "Config options"
    )]
    profile: Option<String>,
}

impl ConfigOpts {
    /// Creates a nextest version-only config with the given options.
    pub fn make_version_only_config(&self, workspace_root: &Utf8Path) -> Result<VersionOnlyConfig> {
        VersionOnlyConfig::from_sources(
            workspace_root,
            self.config_file.as_deref(),
            &self.tool_config_files,
        )
        .map_err(ExpectedError::config_parse_error)
    }

    /// Creates a nextest config with the given options.
    pub fn make_config(
        &self,
        workspace_root: &Utf8Path,
        pcx: &ParseContext<'_>,
        experimental: &BTreeSet<ConfigExperimental>,
    ) -> Result<NextestConfig> {
        NextestConfig::from_sources(
            workspace_root,
            pcx,
            self.config_file.as_deref(),
            &self.tool_config_files,
            experimental,
        )
        .map_err(ExpectedError::config_parse_error)
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List tests in workspace
    ///
    /// This command builds test binaries and queries them for the tests they contain.
    ///
    /// Use --verbose to get more information about tests, including test binary paths and skipped
    /// tests.
    ///
    /// Use --message-format json to get machine-readable output.
    ///
    /// For more information, see <https://nexte.st/docs/listing>.
    List {
        #[clap(flatten)]
        cargo_options: CargoOptions,

        #[clap(flatten)]
        build_filter: TestBuildFilter,

        /// Output format
        #[arg(
            short = 'T',
            long,
            value_enum,
            default_value_t,
            help_heading = "Output options",
            value_name = "FMT"
        )]
        message_format: MessageFormatOpts,

        /// Type of listing
        #[arg(
            long,
            value_enum,
            default_value_t,
            help_heading = "Output options",
            value_name = "TYPE"
        )]
        list_type: ListType,

        #[clap(flatten)]
        reuse_build: ReuseBuildOpts,
    },
    /// Build and run tests
    ///
    /// This command builds test binaries and queries them for the tests they contain,
    /// then runs each test in parallel.
    ///
    /// For more information, see <https://nexte.st/docs/running>.
    #[command(visible_alias = "r")]
    Run(RunOpts),
    /// Build and archive tests
    ///
    /// This command builds test binaries and archives them to a file. The archive can then be
    /// transferred to another machine, and tests within it can be run with `cargo nextest run
    /// --archive-file`.
    ///
    /// The archive is a tarball compressed with Zstandard (.tar.zst).
    Archive {
        #[clap(flatten)]
        cargo_options: CargoOptions,

        /// File to write archive to
        #[arg(
            long,
            name = "archive-file",
            help_heading = "Archive options",
            value_name = "PATH"
        )]
        archive_file: Utf8PathBuf,

        /// Archive format
        ///
        /// `auto` uses the file extension to determine the archive format. Currently supported is
        /// `.tar.zst`.
        #[arg(
            long,
            value_enum,
            help_heading = "Archive options",
            value_name = "FORMAT",
            default_value_t
        )]
        archive_format: ArchiveFormatOpt,

        /// Zstandard compression level (-7 to 22, higher is more compressed + slower)
        #[arg(
            long,
            help_heading = "Archive options",
            value_name = "LEVEL",
            default_value_t = 0,
            allow_negative_numbers = true
        )]
        zstd_level: i32,
        // ReuseBuildOpts, while it can theoretically work, is way too confusing so skip it.
    },
    /// Show information about nextest's configuration in this workspace.
    ///
    /// This command shows configuration information about nextest, including overrides applied to
    /// individual tests.
    ///
    /// In the future, this will show more information about configurations and overrides.
    ShowConfig {
        #[clap(subcommand)]
        command: ShowConfigCommand,
    },
    /// Manage the nextest installation
    #[clap(name = "self")]
    Self_ {
        #[clap(subcommand)]
        command: SelfCommand,
    },
    /// Debug commands
    ///
    /// The commands in this section are for nextest's own developers and those integrating with it
    /// to debug issues. They are not part of the public API and may change at any time.
    #[clap(hide = true)]
    Debug {
        #[clap(subcommand)]
        command: DebugCommand,
    },
}

#[derive(Debug, Args)]
struct NtrOpts {
    #[clap(flatten)]
    common: CommonOpts,

    #[clap(flatten)]
    run_opts: RunOpts,
}

impl NtrOpts {
    fn exec(
        self,
        cli_args: Vec<String>,
        output: OutputContext,
        output_writer: &mut OutputWriter,
    ) -> Result<i32> {
        let base = BaseApp::new(
            output,
            self.run_opts.reuse_build,
            self.run_opts.cargo_options,
            self.common.config_opts,
            self.common.manifest_path,
            output_writer,
        )?;
        let app = App::new(base, self.run_opts.build_filter)?;
        app.exec_run(
            self.run_opts.no_capture,
            &self.run_opts.runner_opts,
            &self.run_opts.reporter_opts,
            cli_args,
            output_writer,
        )
    }
}

#[derive(Debug, Args)]
struct RunOpts {
    #[clap(flatten)]
    cargo_options: CargoOptions,

    #[clap(flatten)]
    build_filter: TestBuildFilter,

    #[clap(flatten)]
    runner_opts: TestRunnerOpts,

    /// Run tests serially and do not capture output
    #[arg(
        long,
        name = "no-capture",
        alias = "nocapture",
        help_heading = "Runner options",
        display_order = 100
    )]
    no_capture: bool,

    #[clap(flatten)]
    reporter_opts: ReporterOpts,

    #[clap(flatten)]
    reuse_build: ReuseBuildOpts,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub(crate) enum PlatformFilterOpts {
    Target,
    Host,
    Any,
}

impl Default for PlatformFilterOpts {
    fn default() -> Self {
        Self::Any
    }
}

impl From<PlatformFilterOpts> for Option<BuildPlatform> {
    fn from(opt: PlatformFilterOpts) -> Self {
        match opt {
            PlatformFilterOpts::Target => Some(BuildPlatform::Target),
            PlatformFilterOpts::Host => Some(BuildPlatform::Host),
            PlatformFilterOpts::Any => None,
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum ListType {
    Full,
    BinariesOnly,
}

impl Default for ListType {
    fn default() -> Self {
        Self::Full
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum MessageFormatOpts {
    Human,
    Json,
    JsonPretty,
}

impl MessageFormatOpts {
    fn to_output_format(self, verbose: bool) -> OutputFormat {
        match self {
            Self::Human => OutputFormat::Human { verbose },
            Self::Json => OutputFormat::Serializable(SerializableFormat::Json),
            Self::JsonPretty => OutputFormat::Serializable(SerializableFormat::JsonPretty),
        }
    }
}

impl Default for MessageFormatOpts {
    fn default() -> Self {
        Self::Human
    }
}

#[derive(Debug, Args)]
#[command(next_help_heading = "Filter options")]
struct TestBuildFilter {
    /// Run ignored tests
    #[arg(long, value_enum, value_name = "WHICH")]
    run_ignored: Option<RunIgnoredOpt>,

    /// Test partition, e.g. hash:1/2 or count:2/3
    #[arg(long)]
    partition: Option<PartitionerBuilder>,

    /// Filter test binaries by build platform (DEPRECATED)
    ///
    /// Instead, use -E with 'platform(host)' or 'platform(target)'.
    #[arg(
        long,
        hide_short_help = true,
        value_enum,
        value_name = "PLATFORM",
        default_value_t
    )]
    pub(crate) platform_filter: PlatformFilterOpts,

    /// Test filterset (see {n}<https://nexte.st/docs/filtersets>).
    #[arg(
        long,
        alias = "filter-expr",
        short = 'E',
        value_name = "EXPR",
        action(ArgAction::Append)
    )]
    filterset: Vec<String>,

    /// Ignore the default filter configured in the profile.
    ///
    /// By default, all filtersets are intersected with the default filter configured in the
    /// profile. This flag disables that behavior.
    ///
    /// This flag doesn't change the definition of the `default()` filterset.
    #[arg(long)]
    ignore_default_filter: bool,

    /// Test name filters.
    #[arg(help_heading = None, name = "FILTERS")]
    pre_double_dash_filters: Vec<String>,

    /// Test name filters and emulated test binary arguments.
    ///
    /// Supported arguments:{n}
    /// - --ignored:         Only run ignored tests{n}
    /// - --include-ignored: Run both ignored and non-ignored tests{n}
    /// - --skip PATTERN:    Skip tests that match the pattern{n}
    /// - --exact:           Run tests that exactly match patterns after `--`
    #[arg(help_heading = None, value_name = "FILTERS_AND_ARGS", last = true)]
    filters: Vec<String>,
}

impl TestBuildFilter {
    #[expect(clippy::too_many_arguments)]
    fn compute_test_list<'g>(
        &self,
        ctx: &TestExecuteContext<'_>,
        graph: &'g PackageGraph,
        workspace_root: Utf8PathBuf,
        binary_list: Arc<BinaryList>,
        test_filter_builder: TestFilterBuilder,
        env: EnvironmentMap,
        ecx: &EvalContext<'_>,
        rust_build_meta: RustBuildMeta<TestListState>,
        path_mapper: &PathMapper,
    ) -> Result<TestList<'g>> {
        let test_artifacts = RustTestArtifact::from_binary_list(
            graph,
            binary_list,
            &rust_build_meta,
            path_mapper,
            self.platform_filter.into(),
        )?;
        TestList::new(
            ctx,
            test_artifacts,
            rust_build_meta,
            &test_filter_builder,
            workspace_root,
            env,
            ecx,
            if self.ignore_default_filter {
                FilterBound::All
            } else {
                FilterBound::DefaultSet
            },
            // TODO: do we need to allow customizing this?
            get_num_cpus(),
        )
        .map_err(|err| ExpectedError::CreateTestListError { err })
    }

    fn make_test_filter_builder(&self, filter_exprs: Vec<Filterset>) -> Result<TestFilterBuilder> {
        // Merge the test binary args into the patterns.
        let mut run_ignored = self.run_ignored.map(Into::into);
        let mut patterns = TestFilterPatterns::new(self.pre_double_dash_filters.clone());
        self.merge_test_binary_args(&mut run_ignored, &mut patterns)?;

        Ok(TestFilterBuilder::new(
            run_ignored.unwrap_or_default(),
            self.partition.clone(),
            patterns,
            filter_exprs,
        )?)
    }

    fn merge_test_binary_args(
        &self,
        run_ignored: &mut Option<RunIgnored>,
        patterns: &mut TestFilterPatterns,
    ) -> Result<()> {
        // First scan to see if `--exact` is specified. If so, then everything here will be added to
        // `--exact`.
        let mut is_exact = false;
        for arg in &self.filters {
            if arg == "--" {
                break;
            }
            if arg == "--exact" {
                if is_exact {
                    return Err(ExpectedError::test_binary_args_parse_error(
                        "duplicated",
                        vec![arg.clone()],
                    ));
                }
                is_exact = true;
            }
        }

        let mut ignore_filters = Vec::new();
        let mut read_trailing_filters = false;

        let mut unsupported_args = Vec::new();

        let mut it = self.filters.iter();
        while let Some(arg) = it.next() {
            if read_trailing_filters || !arg.starts_with('-') {
                if is_exact {
                    patterns.add_exact_pattern(arg.clone());
                } else {
                    patterns.add_substring_pattern(arg.clone());
                }
            } else if arg == "--include-ignored" {
                ignore_filters.push((arg.clone(), RunIgnored::All));
            } else if arg == "--ignored" {
                ignore_filters.push((arg.clone(), RunIgnored::Only));
            } else if arg == "--" {
                read_trailing_filters = true;
            } else if arg == "--skip" {
                let skip_arg = it.next().ok_or_else(|| {
                    ExpectedError::test_binary_args_parse_error(
                        "missing required argument",
                        vec![arg.clone()],
                    )
                })?;

                if is_exact {
                    patterns.add_skip_exact_pattern(skip_arg.clone());
                } else {
                    patterns.add_skip_pattern(skip_arg.clone());
                }
            } else if arg == "--exact" {
                // Already handled above.
            } else {
                unsupported_args.push(arg.clone());
            }
        }

        for (s, f) in ignore_filters {
            if let Some(run_ignored) = run_ignored {
                if *run_ignored != f {
                    return Err(ExpectedError::test_binary_args_parse_error(
                        "mutually exclusive",
                        vec![s],
                    ));
                } else {
                    return Err(ExpectedError::test_binary_args_parse_error(
                        "duplicated",
                        vec![s],
                    ));
                }
            } else {
                *run_ignored = Some(f);
            }
        }

        if !unsupported_args.is_empty() {
            return Err(ExpectedError::test_binary_args_parse_error(
                "unsupported",
                unsupported_args,
            ));
        }

        Ok(())
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum RunIgnoredOpt {
    /// Run non-ignored tests.
    Default,

    /// Run ignored tests.
    #[clap(alias = "ignored-only")]
    Only,

    /// Run both ignored and non-ignored tests.
    All,
}

impl From<RunIgnoredOpt> for RunIgnored {
    fn from(opt: RunIgnoredOpt) -> Self {
        match opt {
            RunIgnoredOpt::Default => RunIgnored::Default,
            RunIgnoredOpt::Only => RunIgnored::Only,
            RunIgnoredOpt::All => RunIgnored::All,
        }
    }
}

impl CargoOptions {
    fn compute_binary_list(
        &self,
        graph: &PackageGraph,
        manifest_path: Option<&Utf8Path>,
        output: OutputContext,
        build_platforms: BuildPlatforms,
    ) -> Result<BinaryList> {
        // Don't use the manifest path from the graph to ensure that if the user cd's into a
        // particular crate and runs cargo nextest, then it behaves identically to cargo test.
        let mut cargo_cli = CargoCli::new("test", manifest_path, output);

        // Only build tests in the cargo test invocation, do not run them.
        cargo_cli.add_args(["--no-run", "--message-format", "json-render-diagnostics"]);
        cargo_cli.add_options(self);

        let expression = cargo_cli.to_expression();
        let output = expression
            .stdout_capture()
            .unchecked()
            .run()
            .map_err(|err| ExpectedError::build_exec_failed(cargo_cli.all_args(), err))?;
        if !output.status.success() {
            return Err(ExpectedError::build_failed(
                cargo_cli.all_args(),
                output.status.code(),
            ));
        }

        let test_binaries =
            BinaryList::from_messages(Cursor::new(output.stdout), graph, build_platforms)?;
        Ok(test_binaries)
    }
}

/// Test runner options.
#[derive(Debug, Default, Args)]
#[command(next_help_heading = "Runner options")]
pub struct TestRunnerOpts {
    /// Compile, but don't run tests
    #[arg(long, name = "no-run")]
    no_run: bool,

    /// Number of tests to run simultaneously [possible values: integer or "num-cpus"]
    /// [default: from profile]
    #[arg(
        long,
        short = 'j',
        visible_alias = "jobs",
        value_name = "N",
        conflicts_with_all = &["no-capture", "no-run"],
        env = "NEXTEST_TEST_THREADS",
        allow_negative_numbers = true,
    )]
    test_threads: Option<TestThreads>,

    /// Number of retries for failing tests [default: from profile]
    #[arg(
        long,
        env = "NEXTEST_RETRIES",
        value_name = "N",
        conflicts_with = "no-run"
    )]
    retries: Option<usize>,

    /// Cancel test run on the first failure
    #[arg(long, name = "fail-fast", conflicts_with = "no-run")]
    fail_fast: bool,

    /// Run all tests regardless of failure
    #[arg(
        long,
        name = "no-fail-fast",
        conflicts_with = "no-run",
        overrides_with = "fail-fast"
    )]
    no_fail_fast: bool,

    /// Number of tests that can fail before exiting test run [possible values: integer or "all"]
    #[arg(
        long,
        name = "max-fail",
        value_name = "N",
        conflicts_with_all = &["no-run", "fail-fast", "no-fail-fast"],
    )]
    max_fail: Option<MaxFail>,

    /// Behavior if there are no tests to run [default: fail]
    #[arg(
        long,
        value_enum,
        conflicts_with = "no-run",
        value_name = "ACTION",
        env = "NEXTEST_NO_TESTS"
    )]
    no_tests: Option<NoTestsBehavior>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum NoTestsBehavior {
    /// Silently exit with code 0.
    Pass,

    /// Produce a warning and exit with code 0.
    Warn,

    /// Produce an error message and exit with code 4.
    #[clap(alias = "error")]
    Fail,
}

impl TestRunnerOpts {
    fn to_builder(
        &self,
        cap_strat: nextest_runner::test_output::CaptureStrategy,
    ) -> Option<TestRunnerBuilder> {
        if self.no_run {
            return None;
        }

        let mut builder = TestRunnerBuilder::default();
        builder.set_capture_strategy(cap_strat);
        if let Some(retries) = self.retries {
            builder.set_retries(RetryPolicy::new_without_delay(retries));
        }

        if let Some(max_fail) = self.max_fail {
            builder.set_max_fail(max_fail);
            debug!(max_fail = ?max_fail, "set max fail");
        } else if self.no_fail_fast {
            builder.set_max_fail(MaxFail::from_fail_fast(false));
            debug!("set max fail via from_fail_fast(false)");
        } else if self.fail_fast {
            builder.set_max_fail(MaxFail::from_fail_fast(true));
            debug!("set max fail via from_fail_fast(true)");
        }

        if let Some(test_threads) = self.test_threads {
            builder.set_test_threads(test_threads);
        }

        Some(builder)
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum IgnoreOverridesOpt {
    Retries,
    All,
}

#[derive(Clone, Copy, Debug, ValueEnum, Default)]
enum MessageFormat {
    /// The default output format.
    #[default]
    Human,
    /// Output test information in the same format as libtest.
    LibtestJson,
    /// Output test information in the same format as libtest, with a `nextest` subobject that
    /// includes additional metadata.
    LibtestJsonPlus,
}

#[derive(Debug, Default, Args)]
#[command(next_help_heading = "Reporter options")]
struct ReporterOpts {
    /// Output stdout and stderr on failure
    #[arg(
        long,
        value_enum,
        conflicts_with_all = &["no-capture", "no-run"],
        value_name = "WHEN",
        env = "NEXTEST_FAILURE_OUTPUT",
    )]
    failure_output: Option<TestOutputDisplayOpt>,

    /// Output stdout and stderr on success
    #[arg(
        long,
        value_enum,
        conflicts_with_all = &["no-capture", "no-run"],
        value_name = "WHEN",
        env = "NEXTEST_SUCCESS_OUTPUT",
    )]
    success_output: Option<TestOutputDisplayOpt>,

    // status_level does not conflict with --no-capture because pass vs skip still makes sense.
    /// Test statuses to output
    #[arg(
        long,
        value_enum,
        conflicts_with = "no-run",
        value_name = "LEVEL",
        env = "NEXTEST_STATUS_LEVEL"
    )]
    status_level: Option<StatusLevelOpt>,

    /// Test statuses to output at the end of the run.
    #[arg(
        long,
        value_enum,
        conflicts_with = "no-run",
        value_name = "LEVEL",
        env = "NEXTEST_FINAL_STATUS_LEVEL"
    )]
    final_status_level: Option<FinalStatusLevelOpt>,

    /// Do not display the progress bar
    #[arg(long, env = "NEXTEST_HIDE_PROGRESS_BAR", value_parser = BoolishValueParser::new())]
    hide_progress_bar: bool,

    /// Disable handling of input keys from the terminal.
    ///
    /// By default, when running a terminal, nextest accepts the `t` key to dump
    /// test information. This flag disables that behavior.
    #[arg(long, env = "NEXTEST_NO_INPUT_HANDLER", value_parser = BoolishValueParser::new())]
    no_input_handler: bool,

    /// Format to use for test results (experimental).
    #[arg(
        long,
        name = "message-format",
        value_enum,
        default_value_t,
        conflicts_with = "no-run",
        value_name = "FORMAT",
        env = "NEXTEST_MESSAGE_FORMAT"
    )]
    message_format: MessageFormat,

    /// Version of structured message-format to use (experimental).
    ///
    /// This allows the machine-readable formats to use a stable structure for consistent
    /// consumption across changes to nextest. If not specified, the latest version is used.
    #[arg(
        long,
        conflicts_with = "no-run",
        requires = "message-format",
        value_name = "VERSION",
        env = "NEXTEST_MESSAGE_FORMAT_VERSION"
    )]
    message_format_version: Option<String>,
}

impl ReporterOpts {
    fn to_builder(&self, no_capture: bool, should_colorize: bool) -> ReporterBuilder {
        let mut builder = ReporterBuilder::default();
        builder.set_no_capture(no_capture);
        builder.set_colorize(should_colorize);

        if let Some(failure_output) = self.failure_output {
            builder.set_failure_output(failure_output.into());
        }
        if let Some(success_output) = self.success_output {
            builder.set_success_output(success_output.into());
        }
        if let Some(status_level) = self.status_level {
            builder.set_status_level(status_level.into());
        }
        if let Some(final_status_level) = self.final_status_level {
            builder.set_final_status_level(final_status_level.into());
        }
        builder.set_hide_progress_bar(self.hide_progress_bar);
        builder
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum TestOutputDisplayOpt {
    Immediate,
    ImmediateFinal,
    Final,
    Never,
}

impl From<TestOutputDisplayOpt> for TestOutputDisplay {
    fn from(opt: TestOutputDisplayOpt) -> Self {
        match opt {
            TestOutputDisplayOpt::Immediate => TestOutputDisplay::Immediate,
            TestOutputDisplayOpt::ImmediateFinal => TestOutputDisplay::ImmediateFinal,
            TestOutputDisplayOpt::Final => TestOutputDisplay::Final,
            TestOutputDisplayOpt::Never => TestOutputDisplay::Never,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum StatusLevelOpt {
    None,
    Fail,
    Retry,
    Slow,
    Leak,
    Pass,
    Skip,
    All,
}

impl From<StatusLevelOpt> for StatusLevel {
    fn from(opt: StatusLevelOpt) -> Self {
        match opt {
            StatusLevelOpt::None => StatusLevel::None,
            StatusLevelOpt::Fail => StatusLevel::Fail,
            StatusLevelOpt::Retry => StatusLevel::Retry,
            StatusLevelOpt::Slow => StatusLevel::Slow,
            StatusLevelOpt::Leak => StatusLevel::Leak,
            StatusLevelOpt::Pass => StatusLevel::Pass,
            StatusLevelOpt::Skip => StatusLevel::Skip,
            StatusLevelOpt::All => StatusLevel::All,
        }
    }
}

/// This is copied from `FinalStatusLevel` except it also has a retry option.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum FinalStatusLevelOpt {
    None,
    Fail,
    #[clap(alias = "retry")]
    Flaky,
    Slow,
    Skip,
    Pass,
    All,
}

impl From<FinalStatusLevelOpt> for FinalStatusLevel {
    fn from(opt: FinalStatusLevelOpt) -> FinalStatusLevel {
        match opt {
            FinalStatusLevelOpt::None => FinalStatusLevel::None,
            FinalStatusLevelOpt::Fail => FinalStatusLevel::Fail,
            FinalStatusLevelOpt::Flaky => FinalStatusLevel::Flaky,
            FinalStatusLevelOpt::Slow => FinalStatusLevel::Slow,
            FinalStatusLevelOpt::Skip => FinalStatusLevel::Skip,
            FinalStatusLevelOpt::Pass => FinalStatusLevel::Pass,
            FinalStatusLevelOpt::All => FinalStatusLevel::All,
        }
    }
}

#[derive(Debug)]
struct BaseApp {
    output: OutputContext,
    // TODO: support multiple --target options
    build_platforms: BuildPlatforms,
    cargo_metadata_json: Arc<String>,
    package_graph: Arc<PackageGraph>,
    // Potentially remapped workspace root (might not be the same as the graph).
    workspace_root: Utf8PathBuf,
    manifest_path: Option<Utf8PathBuf>,
    reuse_build: ReuseBuildInfo,
    cargo_opts: CargoOptions,
    config_opts: ConfigOpts,
    current_version: Version,

    cargo_configs: CargoConfigs,
    double_spawn: OnceLock<DoubleSpawnInfo>,
    target_runner: OnceLock<TargetRunner>,
}

impl BaseApp {
    fn new(
        output: OutputContext,
        reuse_build: ReuseBuildOpts,
        cargo_opts: CargoOptions,
        config_opts: ConfigOpts,
        manifest_path: Option<Utf8PathBuf>,
        writer: &mut OutputWriter,
    ) -> Result<Self> {
        reuse_build.check_experimental(output);

        let reuse_build = reuse_build.process(output, writer)?;

        // First obtain the Cargo configs.
        let cargo_configs = CargoConfigs::new(&cargo_opts.config).map_err(Box::new)?;

        // Next, read the build platforms.
        let build_platforms = match reuse_build.binaries_metadata() {
            Some(kind) => kind.binary_list.rust_build_meta.build_platforms.clone(),
            None => detect_build_platforms(&cargo_configs, cargo_opts.target.as_deref())?,
        };

        // Read the Cargo metadata.
        let (cargo_metadata_json, package_graph) = match reuse_build.cargo_metadata() {
            Some(m) => (m.json.clone(), m.graph.clone()),
            None => {
                let json = acquire_graph_data(
                    manifest_path.as_deref(),
                    cargo_opts.target_dir.as_deref(),
                    &cargo_opts,
                    &build_platforms,
                    output,
                )?;
                let graph = PackageGraph::from_json(&json)
                    .map_err(|err| ExpectedError::cargo_metadata_parse_error(None, err))?;
                (Arc::new(json), Arc::new(graph))
            }
        };

        let manifest_path = if reuse_build.cargo_metadata.is_some() {
            Some(package_graph.workspace().root().join("Cargo.toml"))
        } else {
            manifest_path
        };

        let workspace_root = match reuse_build.workspace_remap() {
            Some(path) => path.to_owned(),
            _ => package_graph.workspace().root().to_owned(),
        };

        let root_manifest_path = workspace_root.join("Cargo.toml");
        if !root_manifest_path.exists() {
            // This doesn't happen in normal use, but is a common situation if the build is being
            // reused.
            let reuse_build_kind = if reuse_build.workspace_remap().is_some() {
                ReuseBuildKind::ReuseWithWorkspaceRemap { workspace_root }
            } else if reuse_build.is_active() {
                ReuseBuildKind::Reuse
            } else {
                ReuseBuildKind::Normal
            };

            return Err(ExpectedError::RootManifestNotFound {
                path: root_manifest_path,
                reuse_build_kind,
            });
        }

        let current_version = current_version();

        Ok(Self {
            output,
            build_platforms,
            cargo_metadata_json,
            package_graph,
            workspace_root,
            reuse_build,
            manifest_path,
            cargo_opts,
            config_opts,
            cargo_configs,
            current_version,

            double_spawn: OnceLock::new(),
            target_runner: OnceLock::new(),
        })
    }

    fn load_config(&self, pcx: &ParseContext<'_>) -> Result<(VersionOnlyConfig, NextestConfig)> {
        // Load the version-only config first to avoid incompatibilities with parsing the rest of
        // the config.
        let version_only_config = self
            .config_opts
            .make_version_only_config(&self.workspace_root)?;
        self.check_version_config_initial(version_only_config.nextest_version())?;

        let experimental = version_only_config.experimental();
        if !experimental.is_empty() {
            info!(
                "experimental features enabled: {}",
                experimental
                    .iter()
                    .map(|x| x.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        let config = self.config_opts.make_config(
            &self.workspace_root,
            pcx,
            version_only_config.experimental(),
        )?;

        Ok((version_only_config, config))
    }

    fn check_version_config_initial(&self, version_cfg: &NextestVersionConfig) -> Result<()> {
        let styles = self.output.stderr_styles();

        match version_cfg.eval(
            &self.current_version,
            self.config_opts.override_version_check,
        ) {
            NextestVersionEval::Satisfied => Ok(()),
            NextestVersionEval::Error {
                required,
                current,
                tool,
            } => Err(ExpectedError::RequiredVersionNotMet {
                required,
                current,
                tool,
            }),
            NextestVersionEval::Warn {
                recommended: required,
                current,
                tool,
            } => {
                warn!(
                    "this repository recommends nextest version {}, but the current version is {}",
                    required.style(styles.bold),
                    current.style(styles.bold),
                );
                if let Some(tool) = tool {
                    info!(
                        target: "cargo_nextest::no_heading",
                        "(recommended version specified by tool `{}`)",
                        tool,
                    );
                }

                Ok(())
            }
            NextestVersionEval::ErrorOverride {
                required,
                current,
                tool,
            } => {
                info!(
                    "overriding version check (required: {}, current: {})",
                    required, current
                );
                if let Some(tool) = tool {
                    info!(
                        target: "cargo_nextest::no_heading",
                        "(required version specified by tool `{}`)",
                        tool,
                    );
                }

                Ok(())
            }
            NextestVersionEval::WarnOverride {
                recommended,
                current,
                tool,
            } => {
                info!(
                    "overriding version check (recommended: {}, current: {})",
                    recommended, current,
                );
                if let Some(tool) = tool {
                    info!(
                        target: "cargo_nextest::no_heading",
                        "(recommended version specified by tool `{}`)",
                        tool,
                    );
                }

                Ok(())
            }
        }
    }

    fn check_version_config_final(&self, version_cfg: &NextestVersionConfig) -> Result<()> {
        let styles = self.output.stderr_styles();

        match version_cfg.eval(
            &self.current_version,
            self.config_opts.override_version_check,
        ) {
            NextestVersionEval::Satisfied => Ok(()),
            NextestVersionEval::Error {
                required,
                current,
                tool,
            } => Err(ExpectedError::RequiredVersionNotMet {
                required,
                current,
                tool,
            }),
            NextestVersionEval::Warn {
                recommended: required,
                current,
                tool,
            } => {
                warn!(
                    "this repository recommends nextest version {}, but the current version is {}",
                    required.style(styles.bold),
                    current.style(styles.bold),
                );
                if let Some(tool) = tool {
                    info!(
                        target: "cargo_nextest::no_heading",
                        "(recommended version specified by tool `{}`)",
                        tool,
                    );
                }

                // Don't need to print extra text here -- this is a warning, not an error.
                crate::helpers::log_needs_update(
                    Level::INFO,
                    crate::helpers::BYPASS_VERSION_TEXT,
                    &styles,
                );

                Ok(())
            }
            NextestVersionEval::ErrorOverride { .. } | NextestVersionEval::WarnOverride { .. } => {
                // Don't print overrides at the end since users have already opted into overrides --
                // just be ok with the one at the beginning.
                Ok(())
            }
        }
    }

    fn load_double_spawn(&self) -> &DoubleSpawnInfo {
        self.double_spawn.get_or_init(|| {
            if std::env::var("NEXTEST_EXPERIMENTAL_DOUBLE_SPAWN").is_ok() {
                warn!(
                    "double-spawn is no longer experimental: \
                     NEXTEST_EXPERIMENTAL_DOUBLE_SPAWN does not need to be set"
                );
            }
            if std::env::var("NEXTEST_DOUBLE_SPAWN") == Ok("0".to_owned()) {
                info!("NEXTEST_DOUBLE_SPAWN=0 set, disabling double-spawn for test processes");
                DoubleSpawnInfo::disabled()
            } else {
                DoubleSpawnInfo::try_enable()
            }
        })
    }

    fn load_runner(&self, build_platforms: &BuildPlatforms) -> &TargetRunner {
        self.target_runner.get_or_init(|| {
            runner_for_target(
                &self.cargo_configs,
                build_platforms,
                &self.output.stderr_styles(),
            )
        })
    }

    fn exec_archive(
        &self,
        output_file: &Utf8Path,
        format: ArchiveFormatOpt,
        zstd_level: i32,
        output_writer: &mut OutputWriter,
    ) -> Result<()> {
        // Do format detection first so we fail immediately.
        let format = format.to_archive_format(output_file)?;
        let binary_list = self.build_binary_list()?;
        let path_mapper = PathMapper::noop();

        let pcx = ParseContext::new(self.graph());
        let (_, config) = self.load_config(&pcx)?;
        let profile = self
            .load_profile(&config)?
            .into_evaluatable(&binary_list.rust_build_meta.build_platforms);

        let redactor = if should_redact() {
            Redactor::build_active(&binary_list.rust_build_meta)
                .with_path(output_file.to_path_buf(), "<archive-file>".to_owned())
                .build()
        } else {
            Redactor::noop()
        };

        let mut reporter = ArchiveReporter::new(self.output.verbose, redactor.clone());
        if self
            .output
            .color
            .should_colorize(supports_color::Stream::Stderr)
        {
            reporter.colorize();
        }

        let mut writer = output_writer.stderr_writer();
        archive_to_file(
            profile,
            &binary_list,
            &self.cargo_metadata_json,
            &self.package_graph,
            // Note that path_mapper is currently a no-op -- we don't support reusing builds for
            // archive creation because it's too confusing.
            &path_mapper,
            format,
            zstd_level,
            output_file,
            |event| {
                reporter.report_event(event, &mut writer)?;
                writer.flush()
            },
            redactor.clone(),
        )
        .map_err(|err| ExpectedError::ArchiveCreateError {
            archive_file: output_file.to_owned(),
            err,
            redactor,
        })?;

        Ok(())
    }

    fn build_binary_list(&self) -> Result<Arc<BinaryList>> {
        let binary_list = match self.reuse_build.binaries_metadata() {
            Some(m) => m.binary_list.clone(),
            None => Arc::new(self.cargo_opts.compute_binary_list(
                self.graph(),
                self.manifest_path.as_deref(),
                self.output,
                self.build_platforms.clone(),
            )?),
        };
        Ok(binary_list)
    }

    #[inline]
    fn graph(&self) -> &PackageGraph {
        &self.package_graph
    }

    fn load_profile<'cfg>(&self, config: &'cfg NextestConfig) -> Result<EarlyProfile<'cfg>> {
        let profile_name = self.config_opts.profile.as_deref().unwrap_or_else(|| {
            // The "official" way to detect a miri environment is with MIRI_SYSROOT.
            // https://github.com/rust-lang/miri/pull/2398#issuecomment-1190747685
            if std::env::var_os("MIRI_SYSROOT").is_some() {
                NextestConfig::DEFAULT_MIRI_PROFILE
            } else {
                NextestConfig::DEFAULT_PROFILE
            }
        });
        let profile = config
            .profile(profile_name)
            .map_err(ExpectedError::profile_not_found)?;
        Ok(profile)
    }
}

fn current_version() -> Version {
    // This is a test-only, not part of the public API.
    match std::env::var("__NEXTEST_TEST_VERSION") {
        Ok(version) => version
            .parse()
            .expect("__NEXTEST_TEST_VERSION should be a valid semver version"),
        Err(VarError::NotPresent) => env!("CARGO_PKG_VERSION")
            .parse()
            .expect("CARGO_PKG_VERSION should be a valid semver version"),
        Err(error) => {
            panic!("error reading __NEXTEST_TEST_VERSION: {error}");
        }
    }
}

#[derive(Debug)]
struct App {
    base: BaseApp,
    build_filter: TestBuildFilter,
}

// (_output is not used, but must be passed in to ensure that the output is properly initialized
// before calling this method)
fn check_experimental_filtering(_output: OutputContext) {
    const EXPERIMENTAL_ENV: &str = "NEXTEST_EXPERIMENTAL_FILTER_EXPR";
    if std::env::var(EXPERIMENTAL_ENV).is_ok() {
        warn!("filtersets are no longer experimental: NEXTEST_EXPERIMENTAL_FILTER_EXPR does not need to be set");
    }
}

impl App {
    fn new(base: BaseApp, build_filter: TestBuildFilter) -> Result<Self> {
        check_experimental_filtering(base.output);

        Ok(Self { base, build_filter })
    }

    fn build_filtering_expressions(&self, pcx: &ParseContext<'_>) -> Result<Vec<Filterset>> {
        let (exprs, all_errors): (Vec<_>, Vec<_>) = self
            .build_filter
            .filterset
            .iter()
            .map(|input| Filterset::parse(input.clone(), pcx, FiltersetKind::Test))
            .partition_result();

        if !all_errors.is_empty() {
            Err(ExpectedError::filter_expression_parse_error(all_errors))
        } else {
            Ok(exprs)
        }
    }

    fn build_test_list(
        &self,
        ctx: &TestExecuteContext<'_>,
        binary_list: Arc<BinaryList>,
        test_filter_builder: TestFilterBuilder,
        ecx: &EvalContext<'_>,
        rust_build_meta: RustBuildMeta<TestListState>,
        path_mapper: &PathMapper,
    ) -> Result<TestList> {
        let env = EnvironmentMap::new(&self.base.cargo_configs);
        self.build_filter.compute_test_list(
            ctx,
            self.base.graph(),
            self.base.workspace_root.clone(),
            binary_list,
            test_filter_builder,
            env,
            ecx,
            rust_build_meta,
            path_mapper,
        )
    }

    fn exec_list(
        &self,
        message_format: MessageFormatOpts,
        list_type: ListType,
        output_writer: &mut OutputWriter,
    ) -> Result<()> {
        let pcx = ParseContext::new(self.base.graph());

        let (version_only_config, config) = self.base.load_config(&pcx)?;
        let profile = self.base.load_profile(&config)?;
        let filter_exprs = self.build_filtering_expressions(&pcx)?;
        let test_filter_builder = self.build_filter.make_test_filter_builder(filter_exprs)?;

        let binary_list = self.base.build_binary_list()?;

        match list_type {
            ListType::BinariesOnly => {
                let mut writer = output_writer.stdout_writer();
                binary_list.write(
                    message_format.to_output_format(self.base.output.verbose),
                    &mut writer,
                    self.base
                        .output
                        .color
                        .should_colorize(supports_color::Stream::Stdout),
                )?;
                writer.write_str_flush().map_err(WriteTestListError::Io)?;
            }
            ListType::Full => {
                let double_spawn = self.base.load_double_spawn();
                let target_runner = self
                    .base
                    .load_runner(&binary_list.rust_build_meta.build_platforms);
                let profile =
                    profile.into_evaluatable(&binary_list.rust_build_meta.build_platforms);
                let ctx = TestExecuteContext {
                    profile_name: profile.name(),
                    double_spawn,
                    target_runner,
                };
                let ecx = profile.filterset_ecx();

                let path_mapper = make_path_mapper(
                    &self.base.reuse_build,
                    self.base.graph(),
                    &binary_list.rust_build_meta.target_directory,
                )?;
                let rust_build_meta = binary_list.rust_build_meta.map_paths(&path_mapper);

                let test_list = self.build_test_list(
                    &ctx,
                    binary_list,
                    test_filter_builder,
                    &ecx,
                    rust_build_meta,
                    &path_mapper,
                )?;

                let mut writer = output_writer.stdout_writer();
                test_list.write(
                    message_format.to_output_format(self.base.output.verbose),
                    &mut writer,
                    self.base
                        .output
                        .color
                        .should_colorize(supports_color::Stream::Stdout),
                )?;
                writer.write_str_flush().map_err(WriteTestListError::Io)?;
            }
        }

        self.base
            .check_version_config_final(version_only_config.nextest_version())?;
        Ok(())
    }

    fn exec_show_test_groups(
        &self,
        show_default: bool,
        groups: Vec<TestGroup>,
        output_writer: &mut OutputWriter,
    ) -> Result<()> {
        let pcx = ParseContext::new(self.base.graph());
        let (_, config) = self.base.load_config(&pcx)?;
        let profile = self.base.load_profile(&config)?;

        // Validate test groups before doing any other work.
        let mode = if groups.is_empty() {
            ShowTestGroupsMode::All
        } else {
            let groups = ShowTestGroups::validate_groups(&profile, groups)?;
            ShowTestGroupsMode::Only(groups)
        };
        let settings = ShowTestGroupSettings { mode, show_default };

        let filter_exprs = self.build_filtering_expressions(&pcx)?;
        let test_filter_builder = self.build_filter.make_test_filter_builder(filter_exprs)?;

        let binary_list = self.base.build_binary_list()?;
        let build_platforms = binary_list.rust_build_meta.build_platforms.clone();

        let double_spawn = self.base.load_double_spawn();
        let target_runner = self.base.load_runner(&build_platforms);
        let profile = profile.into_evaluatable(&build_platforms);
        let ctx = TestExecuteContext {
            profile_name: profile.name(),
            double_spawn,
            target_runner,
        };
        let ecx = profile.filterset_ecx();

        let path_mapper = make_path_mapper(
            &self.base.reuse_build,
            self.base.graph(),
            &binary_list.rust_build_meta.target_directory,
        )?;
        let rust_build_meta = binary_list.rust_build_meta.map_paths(&path_mapper);

        let test_list = self.build_test_list(
            &ctx,
            binary_list,
            test_filter_builder,
            &ecx,
            rust_build_meta,
            &path_mapper,
        )?;

        let mut writer = output_writer.stdout_writer();

        let show_test_groups = ShowTestGroups::new(&profile, &test_list, &settings);
        show_test_groups
            .write_human(
                &mut writer,
                self.base
                    .output
                    .color
                    .should_colorize(supports_color::Stream::Stdout),
            )
            .map_err(WriteTestListError::Io)?;
        writer.write_str_flush().map_err(WriteTestListError::Io)?;

        Ok(())
    }

    fn exec_run(
        &self,
        no_capture: bool,
        runner_opts: &TestRunnerOpts,
        reporter_opts: &ReporterOpts,
        cli_args: Vec<String>,
        output_writer: &mut OutputWriter,
    ) -> Result<i32> {
        let pcx = ParseContext::new(self.base.graph());
        let (version_only_config, config) = self.base.load_config(&pcx)?;
        let profile = self.base.load_profile(&config)?;

        // Construct this here so that errors are reported before the build
        // step.
        let mut structured_reporter = structured::StructuredReporter::new();
        match reporter_opts.message_format {
            MessageFormat::Human => {}
            MessageFormat::LibtestJson | MessageFormat::LibtestJsonPlus => {
                // This is currently an experimental feature, and is gated on this environment
                // variable.
                const EXPERIMENTAL_ENV: &str = "NEXTEST_EXPERIMENTAL_LIBTEST_JSON";
                if std::env::var(EXPERIMENTAL_ENV).as_deref() != Ok("1") {
                    return Err(ExpectedError::ExperimentalFeatureNotEnabled {
                        name: "libtest JSON output",
                        var_name: EXPERIMENTAL_ENV,
                    });
                }

                let libtest = structured::LibtestReporter::new(
                    reporter_opts.message_format_version.as_deref(),
                    if matches!(reporter_opts.message_format, MessageFormat::LibtestJsonPlus) {
                        structured::EmitNextestObject::Yes
                    } else {
                        structured::EmitNextestObject::No
                    },
                )?;
                structured_reporter.set_libtest(libtest);
            }
        };
        use nextest_runner::test_output::CaptureStrategy;

        let cap_strat = if no_capture {
            CaptureStrategy::None
        } else if matches!(reporter_opts.message_format, MessageFormat::Human) {
            CaptureStrategy::Split
        } else {
            CaptureStrategy::Combined
        };

        let filter_exprs = self.build_filtering_expressions(&pcx)?;
        let test_filter_builder = self.build_filter.make_test_filter_builder(filter_exprs)?;

        let binary_list = self.base.build_binary_list()?;
        let path_mapper = make_path_mapper(
            &self.base.reuse_build,
            self.base.graph(),
            &binary_list.rust_build_meta.target_directory,
        )?;
        let rust_build_meta = binary_list.rust_build_meta.map_paths(&path_mapper);

        let profile = profile.into_evaluatable(&binary_list.rust_build_meta.build_platforms);

        // This is the earliest point where we can create the aggregator, since
        // we need the remapped target directory which is only available after
        // the test list is built.
        let aggregator = EventAggregator::new(&profile, &rust_build_meta.target_directory)?;

        let double_spawn = self.base.load_double_spawn();
        let target_runner = self
            .base
            .load_runner(&binary_list.rust_build_meta.build_platforms);

        let ctx = TestExecuteContext {
            profile_name: profile.name(),
            double_spawn,
            target_runner,
        };
        let ecx = profile.filterset_ecx();

        let test_list = self.build_test_list(
            &ctx,
            binary_list,
            test_filter_builder,
            &ecx,
            rust_build_meta,
            &path_mapper,
        )?;

        let output = output_writer.reporter_output();
        let should_colorize = self
            .base
            .output
            .color
            .should_colorize(supports_color::Stream::Stderr);

        let signal_handler = SignalHandlerKind::Standard;
        let input_handler = if reporter_opts.no_input_handler {
            InputHandlerKind::Noop
        } else {
            // This means that the input handler determines whether it should be
            // enabled.
            InputHandlerKind::Standard
        };

        // Make the runner.
        let runner_builder = match runner_opts.to_builder(cap_strat) {
            Some(runner_builder) => runner_builder,
            None => {
                // This means --no-run was passed in. Exit.
                return Ok(0);
            }
        };

        let runner = runner_builder.build(
            &test_list,
            &profile,
            cli_args,
            signal_handler,
            input_handler,
            double_spawn.clone(),
            target_runner.clone(),
        )?;

        // Make the reporter.
        let mut reporter = reporter_opts
            .to_builder(no_capture, should_colorize)
            .set_verbose(self.base.output.verbose)
            .build(
                &test_list,
                &profile,
                output,
                aggregator,
                structured_reporter,
            );

        configure_handle_inheritance(no_capture)?;
        let run_stats = runner.try_execute(|event| {
            // Write and flush the event.
            reporter.report_event(event)
        })?;
        reporter.finish();
        self.base
            .check_version_config_final(version_only_config.nextest_version())?;

        match run_stats.summarize_final() {
            FinalRunStats::Success => Ok(0),
            FinalRunStats::NoTestsRun => match runner_opts.no_tests {
                Some(NoTestsBehavior::Pass) => Ok(0),
                Some(NoTestsBehavior::Warn) => {
                    warn!("no tests to run");
                    Ok(0)
                }
                Some(NoTestsBehavior::Fail) => Err(ExpectedError::NoTestsRun { is_default: false }),
                None => Err(ExpectedError::NoTestsRun { is_default: true }),
            },
            FinalRunStats::Cancelled(RunStatsFailureKind::SetupScript)
            | FinalRunStats::Failed(RunStatsFailureKind::SetupScript) => {
                Err(ExpectedError::setup_script_failed())
            }
            FinalRunStats::Cancelled(RunStatsFailureKind::Test { .. })
            | FinalRunStats::Failed(RunStatsFailureKind::Test { .. }) => {
                Err(ExpectedError::test_run_failed())
            }
        }
    }
}

#[derive(Debug, Subcommand)]
enum ShowConfigCommand {
    /// Show version-related configuration.
    Version {},
    /// Show defined test groups and their associated tests.
    TestGroups {
        /// Show default test groups
        #[arg(long)]
        show_default: bool,

        /// Show only the named groups
        #[arg(long)]
        groups: Vec<TestGroup>,

        #[clap(flatten)]
        cargo_options: Box<CargoOptions>,

        #[clap(flatten)]
        build_filter: TestBuildFilter,

        #[clap(flatten)]
        reuse_build: Box<ReuseBuildOpts>,
    },
}

impl ShowConfigCommand {
    fn exec(
        self,
        manifest_path: Option<Utf8PathBuf>,
        config_opts: ConfigOpts,
        output: OutputContext,
        output_writer: &mut OutputWriter,
    ) -> Result<i32> {
        match self {
            Self::Version {} => {
                let mut cargo_cli =
                    CargoCli::new("locate-project", manifest_path.as_deref(), output);
                cargo_cli.add_args(["--workspace", "--message-format=plain"]);
                let locate_project_output = cargo_cli
                    .to_expression()
                    .stdout_capture()
                    .unchecked()
                    .run()
                    .map_err(|error| {
                        ExpectedError::cargo_locate_project_exec_failed(cargo_cli.all_args(), error)
                    })?;
                if !locate_project_output.status.success() {
                    return Err(ExpectedError::cargo_locate_project_failed(
                        cargo_cli.all_args(),
                    ));
                }
                let workspace_root = String::from_utf8(locate_project_output.stdout)
                    .map_err(|err| ExpectedError::WorkspaceRootInvalidUtf8 { err })?;
                // trim_end because the output ends with a newline.
                let workspace_root = Utf8Path::new(workspace_root.trim_end());
                // parent() because the output includes Cargo.toml at the end.
                let workspace_root =
                    workspace_root
                        .parent()
                        .ok_or_else(|| ExpectedError::WorkspaceRootInvalid {
                            workspace_root: workspace_root.to_owned(),
                        })?;

                let config = config_opts.make_version_only_config(workspace_root)?;
                let current_version = current_version();

                let show = ShowNextestVersion::new(
                    config.nextest_version(),
                    &current_version,
                    config_opts.override_version_check,
                );
                show.write_human(
                    &mut output_writer.stdout_writer(),
                    output.color.should_colorize(supports_color::Stream::Stdout),
                )
                .map_err(WriteTestListError::Io)?;

                match config
                    .nextest_version()
                    .eval(&current_version, config_opts.override_version_check)
                {
                    NextestVersionEval::Satisfied => Ok(0),
                    NextestVersionEval::Error { .. } => {
                        crate::helpers::log_needs_update(
                            Level::ERROR,
                            crate::helpers::BYPASS_VERSION_TEXT,
                            &output.stderr_styles(),
                        );
                        Ok(nextest_metadata::NextestExitCode::REQUIRED_VERSION_NOT_MET)
                    }
                    NextestVersionEval::Warn { .. } => {
                        crate::helpers::log_needs_update(
                            Level::WARN,
                            crate::helpers::BYPASS_VERSION_TEXT,
                            &output.stderr_styles(),
                        );
                        Ok(nextest_metadata::NextestExitCode::RECOMMENDED_VERSION_NOT_MET)
                    }
                    NextestVersionEval::ErrorOverride { .. }
                    | NextestVersionEval::WarnOverride { .. } => Ok(0),
                }
            }
            Self::TestGroups {
                show_default,
                groups,
                cargo_options,
                build_filter,
                reuse_build,
            } => {
                let base = BaseApp::new(
                    output,
                    *reuse_build,
                    *cargo_options,
                    config_opts,
                    manifest_path,
                    output_writer,
                )?;
                let app = App::new(base, build_filter)?;

                app.exec_show_test_groups(show_default, groups, output_writer)?;

                Ok(0)
            }
        }
    }
}

#[derive(Debug, Subcommand)]
enum SelfCommand {
    #[clap(hide = true)]
    /// Perform setup actions (currently a no-op)
    Setup {
        /// The entity running the setup command.
        #[arg(long, value_enum, default_value_t = SetupSource::User)]
        source: SetupSource,
    },
    #[cfg_attr(
        not(feature = "self-update"),
        doc = "This version of nextest does not have self-update enabled\n\
        \n\
        Always exits with code 93 (SELF_UPDATE_UNAVAILABLE).
        "
    )]
    #[cfg_attr(
        feature = "self-update",
        doc = "Download and install updates to nextest\n\
        \n\
        This command checks the internet for updates to nextest, then downloads and
        installs them if an update is available."
    )]
    Update {
        /// Version or version range to download
        #[arg(long, default_value = "latest")]
        version: String,

        /// Check for updates rather than downloading them
        ///
        /// If no update is available, exits with code 0. If an update is available, exits with code
        /// 80 (UPDATE_AVAILABLE).
        #[arg(short = 'n', long)]
        check: bool,

        /// Do not prompt for confirmation
        #[arg(short = 'y', long, conflicts_with = "check")]
        yes: bool,

        /// Force downgrades and reinstalls
        #[arg(short, long)]
        force: bool,

        /// URL or path to fetch releases.json from
        #[arg(long)]
        releases_url: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SetupSource {
    User,
    SelfUpdate,
    PackageManager,
}

impl SelfCommand {
    #[cfg_attr(not(feature = "self-update"), expect(unused_variables))]
    fn exec(self, output: OutputOpts) -> Result<i32> {
        let output = output.init();

        match self {
            Self::Setup { source: _source } => {
                // Currently a no-op.
                Ok(0)
            }
            Self::Update {
                version,
                check,
                yes,
                force,
                releases_url,
            } => {
                cfg_if::cfg_if! {
                    if #[cfg(feature = "self-update")] {
                        crate::update::perform_update(
                            &version,
                            check,
                            yes,
                            force,
                            releases_url,
                            output,
                        )
                    } else {
                        info!("this version of cargo-nextest cannot perform self-updates\n\
                                    (hint: this usually means nextest was installed by a package manager)");
                        Ok(nextest_metadata::NextestExitCode::SELF_UPDATE_UNAVAILABLE)
                    }
                }
            }
        }
    }
}

#[derive(Debug, Subcommand)]
enum DebugCommand {
    /// Show the data that nextest would extract from standard output or standard error.
    ///
    /// Text extraction is a heuristic process driven by a bunch of regexes and other similar logic.
    /// This command shows what nextest would extract from a given input.
    Extract {
        /// The path to the standard output produced by the test process.
        #[arg(long, required_unless_present_any = ["stderr", "combined"])]
        stdout: Option<Utf8PathBuf>,

        /// The path to the standard error produced by the test process.
        #[arg(long, required_unless_present_any = ["stdout", "combined"])]
        stderr: Option<Utf8PathBuf>,

        /// The combined output produced by the test process.
        #[arg(long, conflicts_with_all = ["stdout", "stderr"])]
        combined: Option<Utf8PathBuf>,

        /// The kind of output to produce.
        #[arg(value_enum)]
        output_format: ExtractOutputFormat,
    },

    /// Print the current executable path.
    CurrentExe,

    /// Show the build platforms that nextest would use.
    BuildPlatforms {
        /// The target triple to use.
        #[arg(long)]
        target: Option<String>,

        /// Override a Cargo configuration value.
        #[arg(long, value_name = "KEY=VALUE")]
        config: Vec<String>,

        /// Output format.
        #[arg(long, value_enum, default_value_t)]
        output_format: BuildPlatformsOutputFormat,
    },
}

impl DebugCommand {
    fn exec(self, output: OutputOpts) -> Result<i32> {
        let _ = output.init();

        match self {
            DebugCommand::Extract {
                stdout,
                stderr,
                combined,
                output_format,
            } => {
                // Either stdout + stderr or combined must be present.
                if let Some(combined) = combined {
                    let combined = std::fs::read(&combined).map_err(|err| {
                        ExpectedError::DebugExtractReadError {
                            kind: "combined",
                            path: combined,
                            err,
                        }
                    })?;

                    let description_kind = extract_slice_from_output(&combined, &combined);
                    display_output_slice(description_kind, output_format)?;
                } else {
                    let stdout = stdout
                        .map(|path| {
                            std::fs::read(&path).map_err(|err| {
                                ExpectedError::DebugExtractReadError {
                                    kind: "stdout",
                                    path,
                                    err,
                                }
                            })
                        })
                        .transpose()?
                        .unwrap_or_default();
                    let stderr = stderr
                        .map(|path| {
                            std::fs::read(&path).map_err(|err| {
                                ExpectedError::DebugExtractReadError {
                                    kind: "stderr",
                                    path,
                                    err,
                                }
                            })
                        })
                        .transpose()?
                        .unwrap_or_default();

                    let output_slice = extract_slice_from_output(&stdout, &stderr);
                    display_output_slice(output_slice, output_format)?;
                }
            }
            DebugCommand::CurrentExe => {
                let exe = std::env::current_exe()
                    .map_err(|err| ExpectedError::GetCurrentExeFailed { err })?;
                println!("{}", exe.display());
            }
            DebugCommand::BuildPlatforms {
                target,
                config,
                output_format,
            } => {
                let cargo_configs = CargoConfigs::new(&config).map_err(Box::new)?;
                let build_platforms = detect_build_platforms(&cargo_configs, target.as_deref())?;
                match output_format {
                    BuildPlatformsOutputFormat::Debug => {
                        println!("{:#?}", build_platforms);
                    }
                    BuildPlatformsOutputFormat::Triple => {
                        println!(
                            "host triple: {}",
                            build_platforms.host.platform.triple().as_str()
                        );
                        if let Some(target) = &build_platforms.target {
                            println!(
                                "target triple: {}",
                                target.triple.platform.triple().as_str()
                            );
                        } else {
                            println!("target triple: (none)");
                        }
                    }
                }
            }
        }

        Ok(0)
    }
}

fn extract_slice_from_output<'a>(
    stdout: &'a [u8],
    stderr: &'a [u8],
) -> Option<TestOutputErrorSlice<'a>> {
    TestOutputErrorSlice::heuristic_extract(Some(stdout), Some(stderr))
}

fn display_output_slice(
    output_slice: Option<TestOutputErrorSlice<'_>>,
    output_format: ExtractOutputFormat,
) -> Result<()> {
    match output_format {
        ExtractOutputFormat::Raw => {
            if let Some(kind) = output_slice {
                if let Some(out) = kind.combined_subslice() {
                    return std::io::stdout().write_all(out.slice).map_err(|err| {
                        ExpectedError::DebugExtractWriteError {
                            format: output_format,
                            err,
                        }
                    });
                }
            }
        }
        ExtractOutputFormat::JunitDescription => {
            if let Some(kind) = output_slice {
                println!("{}", XmlString::new(kind.to_string()).as_str());
            }
        }
        ExtractOutputFormat::Highlight => {
            if let Some(kind) = output_slice {
                if let Some(out) = kind.combined_subslice() {
                    let end = highlight_end(out.slice);
                    return std::io::stdout()
                        .write_all(&out.slice[..end])
                        .map_err(|err| ExpectedError::DebugExtractWriteError {
                            format: output_format,
                            err,
                        });
                }
            }
        }
    }

    eprintln!("(no description found)");
    Ok(())
}

/// Output format for `nextest debug extract`.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ExtractOutputFormat {
    /// Show the raw text extracted.
    Raw,

    /// Show what would be put in the description field of JUnit reports.
    ///
    /// This is similar to `Raw`, but is valid Unicode, and strips out ANSI escape codes and other
    /// invalid XML characters.
    JunitDescription,

    /// Show what would be highlighted in nextest's output.
    Highlight,
}

impl fmt::Display for ExtractOutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Raw => write!(f, "raw"),
            Self::JunitDescription => write!(f, "junit-description"),
            Self::Highlight => write!(f, "highlight"),
        }
    }
}

/// Output format for `nextest debug build-platforms`.
#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum BuildPlatformsOutputFormat {
    /// Show Debug output.
    #[default]
    Debug,

    /// Show just the triple.
    Triple,
}

fn acquire_graph_data(
    manifest_path: Option<&Utf8Path>,
    target_dir: Option<&Utf8Path>,
    cargo_opts: &CargoOptions,
    build_platforms: &BuildPlatforms,
    output: OutputContext,
) -> Result<String> {
    let cargo_target_arg = build_platforms.to_cargo_target_arg()?;
    let cargo_target_arg_str = cargo_target_arg.to_string();

    let mut cargo_cli = CargoCli::new("metadata", manifest_path, output);
    cargo_cli
        .add_args(["--format-version=1", "--all-features"])
        .add_args(["--filter-platform", &cargo_target_arg_str])
        .add_generic_cargo_options(cargo_opts);

    // We used to be able to pass in --no-deps in common cases, but that was (a) error-prone and (b)
    // a bit harder to do given that some nextest config options depend on the graph. Maybe we could
    // reintroduce it some day.

    let mut expression = cargo_cli.to_expression().stdout_capture().unchecked();
    // cargo metadata doesn't support "--target-dir" but setting the environment
    // variable works.
    if let Some(target_dir) = target_dir {
        expression = expression.env("CARGO_TARGET_DIR", target_dir);
    }
    // Capture stdout but not stderr.
    let output = expression
        .run()
        .map_err(|err| ExpectedError::cargo_metadata_exec_failed(cargo_cli.all_args(), err))?;
    if !output.status.success() {
        return Err(ExpectedError::cargo_metadata_failed(cargo_cli.all_args()));
    }

    let json = String::from_utf8(output.stdout).map_err(|error| {
        let io_error = std::io::Error::new(std::io::ErrorKind::InvalidData, error);
        ExpectedError::cargo_metadata_exec_failed(cargo_cli.all_args(), io_error)
    })?;
    Ok(json)
}

fn detect_build_platforms(
    cargo_configs: &CargoConfigs,
    target_cli_option: Option<&str>,
) -> Result<BuildPlatforms, ExpectedError> {
    let host = HostPlatform::detect(PlatformLibdir::from_rustc_stdout(
        RustcCli::print_host_libdir().read(),
    ))?;
    let triple_info = discover_target_triple(cargo_configs, target_cli_option)?;
    let target = triple_info.map(|triple| {
        let libdir =
            PlatformLibdir::from_rustc_stdout(RustcCli::print_target_libdir(&triple).read());
        TargetPlatform::new(triple, libdir)
    });
    Ok(BuildPlatforms { host, target })
}

fn discover_target_triple(
    cargo_configs: &CargoConfigs,
    target_cli_option: Option<&str>,
) -> Result<Option<TargetTriple>, TargetTripleError> {
    TargetTriple::find(cargo_configs, target_cli_option).inspect(|v| {
        if let Some(triple) = v {
            debug!(
                "using target triple `{}` defined by `{}`; {}",
                triple.platform.triple_str(),
                triple.source,
                triple.location,
            );
        } else {
            debug!("no target triple found, assuming no cross-compilation");
        }
    })
}

fn runner_for_target(
    cargo_configs: &CargoConfigs,
    build_platforms: &BuildPlatforms,
    styles: &StderrStyles,
) -> TargetRunner {
    match TargetRunner::new(cargo_configs, build_platforms) {
        Ok(runner) => {
            if build_platforms.target.is_some() {
                if let Some(runner) = runner.target() {
                    log_platform_runner("for the target platform, ", runner, styles);
                }
                if let Some(runner) = runner.host() {
                    log_platform_runner("for the host platform, ", runner, styles);
                }
            } else {
                // If triple is None, then the host and target platforms use the same runner if
                // any.
                if let Some(runner) = runner.target() {
                    log_platform_runner("", runner, styles);
                }
            }
            runner
        }
        Err(err) => {
            warn_on_err("target runner", &err, styles);
            TargetRunner::empty()
        }
    }
}

fn log_platform_runner(prefix: &str, runner: &PlatformRunner, styles: &StderrStyles) {
    let runner_command = shell_words::join(std::iter::once(runner.binary()).chain(runner.args()));
    info!(
        "{prefix}using target runner `{}` defined by {}",
        runner_command.style(styles.bold),
        runner.source()
    )
}

fn warn_on_err(thing: &str, err: &(dyn std::error::Error), styles: &StderrStyles) {
    let mut s = String::with_capacity(256);
    swrite!(s, "could not determine {thing}: {err}");
    let mut next_error = err.source();
    while let Some(err) = next_error {
        swrite!(s, "\n  {} {}", "caused by:".style(styles.warning_text), err);
        next_error = err.source();
    }

    warn!("{}", s);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_argument_parsing() {
        use clap::error::ErrorKind::{self, *};

        let valid: &[&'static str] = &[
            // ---
            // Basic commands
            // ---
            "cargo nextest list",
            "cargo nextest run",
            // ---
            // Commands with arguments
            // ---
            "cargo nextest list --list-type binaries-only",
            "cargo nextest list --list-type full",
            "cargo nextest list --message-format json-pretty",
            "cargo nextest run --failure-output never",
            "cargo nextest run --success-output=immediate",
            "cargo nextest run --status-level=all",
            "cargo nextest run --no-capture",
            "cargo nextest run --nocapture",
            "cargo nextest run --no-run",
            "cargo nextest run --final-status-level flaky",
            "cargo nextest run --max-fail 3",
            "cargo nextest run --max-fail=all",
            // retry is an alias for flaky -- ensure that it parses
            "cargo nextest run --final-status-level retry",
            "NEXTEST_HIDE_PROGRESS_BAR=1 cargo nextest run",
            "NEXTEST_HIDE_PROGRESS_BAR=true cargo nextest run",
            // ---
            // Cargo options
            // ---
            "cargo nextest list --lib --bins",
            "cargo nextest run --ignore-rust-version --unit-graph",
            // ---
            // Reuse build options
            // ---
            "cargo nextest list --binaries-metadata=foo",
            "cargo nextest run --binaries-metadata=foo --target-dir-remap=bar",
            "cargo nextest list --cargo-metadata path",
            "cargo nextest run --cargo-metadata=path --workspace-remap remapped-path",
            "cargo nextest archive --archive-file my-archive.tar.zst --zstd-level -1",
            "cargo nextest archive --archive-file my-archive.foo --archive-format tar-zst",
            "cargo nextest archive --archive-file my-archive.foo --archive-format tar-zstd",
            "cargo nextest list --archive-file my-archive.tar.zst",
            "cargo nextest list --archive-file my-archive.tar.zst --archive-format tar-zst",
            "cargo nextest list --archive-file my-archive.tar.zst --extract-to my-path",
            "cargo nextest list --archive-file my-archive.tar.zst --extract-to my-path --extract-overwrite",
            "cargo nextest list --archive-file my-archive.tar.zst --persist-extract-tempdir",
            "cargo nextest list --archive-file my-archive.tar.zst --workspace-remap foo",
            "cargo nextest list --archive-file my-archive.tar.zst --config target.'cfg(all())'.runner=\"my-runner\"",
            // ---
            // Filtersets
            // ---
            "cargo nextest list -E deps(foo)",
            "cargo nextest run --filterset 'test(bar)' --package=my-package test-filter",
            "cargo nextest run --filter-expr 'test(bar)' --package=my-package test-filter",
            "cargo nextest list -E 'deps(foo)' --ignore-default-filter",
            // ---
            // Test binary arguments
            // ---
            "cargo nextest run -- --a an arbitrary arg",
            // Test negative test threads
            "cargo nextest run --jobs -3",
            "cargo nextest run --jobs 3",
            // Test negative cargo build jobs
            "cargo nextest run --build-jobs -1",
            "cargo nextest run --build-jobs 1",
        ];

        let invalid: &[(&'static str, ErrorKind)] = &[
            // ---
            // --no-capture and these options conflict
            // ---
            (
                "cargo nextest run --no-capture --test-threads=24",
                ArgumentConflict,
            ),
            (
                "cargo nextest run --no-capture --failure-output=never",
                ArgumentConflict,
            ),
            (
                "cargo nextest run --no-capture --success-output=final",
                ArgumentConflict,
            ),
            // ---
            // --no-run and these options conflict
            // ---
            ("cargo nextest run --no-run -j8", ArgumentConflict),
            ("cargo nextest run --no-run --retries 3", ArgumentConflict),
            ("cargo nextest run --no-run --fail-fast", ArgumentConflict),
            (
                "cargo nextest run --no-run --no-fail-fast",
                ArgumentConflict,
            ),
            ("cargo nextest run --no-run --max-fail=3", ArgumentConflict),
            (
                "cargo nextest run --no-run --failure-output immediate",
                ArgumentConflict,
            ),
            (
                "cargo nextest run --no-run --success-output never",
                ArgumentConflict,
            ),
            (
                "cargo nextest run --no-run --status-level pass",
                ArgumentConflict,
            ),
            (
                "cargo nextest run --no-run --final-status-level skip",
                ArgumentConflict,
            ),
            // ---
            // --max-fail and these options conflict
            // ---
            (
                "cargo nextest run --max-fail=3 --no-fail-fast",
                ArgumentConflict,
            ),
            // ---
            // Reuse build options conflict with cargo options
            // ---
            (
                // NOTE: cargo nextest --manifest-path foo run --cargo-metadata bar is currently
                // accepted. This is a bug: https://github.com/clap-rs/clap/issues/1204
                "cargo nextest run --manifest-path foo --cargo-metadata bar",
                ArgumentConflict,
            ),
            (
                "cargo nextest run --binaries-metadata=foo --lib",
                ArgumentConflict,
            ),
            // ---
            // workspace-remap requires cargo-metadata
            // ---
            (
                "cargo nextest run --workspace-remap foo",
                MissingRequiredArgument,
            ),
            // ---
            // target-dir-remap requires binaries-metadata
            // ---
            (
                "cargo nextest run --target-dir-remap bar",
                MissingRequiredArgument,
            ),
            // ---
            // Archive options
            // ---
            (
                "cargo nextest run --archive-format tar-zst",
                MissingRequiredArgument,
            ),
            (
                "cargo nextest run --archive-file foo --archive-format no",
                InvalidValue,
            ),
            (
                "cargo nextest run --extract-to foo",
                MissingRequiredArgument,
            ),
            (
                "cargo nextest run --archive-file foo --extract-overwrite",
                MissingRequiredArgument,
            ),
            (
                "cargo nextest run --extract-to foo --extract-overwrite",
                MissingRequiredArgument,
            ),
            (
                "cargo nextest run --persist-extract-tempdir",
                MissingRequiredArgument,
            ),
            (
                "cargo nextest run --archive-file foo --extract-to bar --persist-extract-tempdir",
                ArgumentConflict,
            ),
            (
                "cargo nextest run --archive-file foo --cargo-metadata bar",
                ArgumentConflict,
            ),
            (
                "cargo nextest run --archive-file foo --binaries-metadata bar",
                ArgumentConflict,
            ),
            (
                "cargo nextest run --archive-file foo --target-dir-remap bar",
                ArgumentConflict,
            ),
            // Invalid test threads: 0
            ("cargo nextest run --jobs 0", ValueValidation),
            // Test threads must be a number
            ("cargo nextest run --jobs -twenty", UnknownArgument),
            ("cargo nextest run --build-jobs -inf1", UnknownArgument),
        ];

        // Unset all NEXTEST_ env vars because they can conflict with the try_parse_from below.
        for (k, _) in std::env::vars() {
            if k.starts_with("NEXTEST_") {
                std::env::remove_var(k);
            }
        }

        for valid_args in valid {
            let cmd = shell_words::split(valid_args).expect("valid command line");
            // Any args in the beginning with an equals sign should be parsed as environment variables.
            let env_vars: Vec<_> = cmd
                .iter()
                .take_while(|arg| arg.contains('='))
                .cloned()
                .collect();

            let mut env_keys = Vec::with_capacity(env_vars.len());
            for k_v in &env_vars {
                let (k, v) = k_v.split_once('=').expect("valid env var");
                std::env::set_var(k, v);
                env_keys.push(k);
            }

            let cmd = cmd.iter().skip(env_vars.len());

            if let Err(error) = CargoNextestApp::try_parse_from(cmd) {
                panic!("{valid_args} should have successfully parsed, but didn't: {error}");
            }

            // Unset any environment variables we set. (Don't really need to preserve the old value
            // for now.)
            for &k in &env_keys {
                std::env::remove_var(k);
            }
        }

        for &(invalid_args, kind) in invalid {
            match CargoNextestApp::try_parse_from(
                shell_words::split(invalid_args).expect("valid command"),
            ) {
                Ok(_) => {
                    panic!("{invalid_args} should have errored out but successfully parsed");
                }
                Err(error) => {
                    let actual_kind = error.kind();
                    if kind != actual_kind {
                        panic!(
                            "{invalid_args} should error with kind {kind:?}, but actual kind was {actual_kind:?}",
                        );
                    }
                }
            }
        }
    }

    #[derive(Debug, Parser)]
    struct TestCli {
        #[structopt(flatten)]
        build_filter: TestBuildFilter,
    }

    #[test]
    fn test_test_binary_argument_parsing() {
        fn get_test_filter_builder(cmd: &str) -> Result<TestFilterBuilder> {
            let app = TestCli::try_parse_from(shell_words::split(cmd).expect("valid command line"))
                .unwrap_or_else(|_| panic!("{cmd} should have successfully parsed"));
            app.build_filter.make_test_filter_builder(vec![])
        }

        let valid = &[
            // ---
            // substring filter
            // ---
            ("foo -- str1", "foo str1"),
            ("foo -- str2 str3", "foo str2 str3"),
            // ---
            // ignored
            // ---
            ("foo -- --ignored", "foo --run-ignored only"),
            ("foo -- --ignored", "foo --run-ignored ignored-only"),
            ("foo -- --include-ignored", "foo --run-ignored all"),
            // ---
            // two escapes
            // ---
            (
                "foo -- --ignored -- str --- --ignored",
                "foo --run-ignored ignored-only str -- -- --- --ignored",
            ),
            ("foo -- -- str1 str2 --", "foo str1 str2 -- -- --"),
        ];
        let skip_exact = &[
            // ---
            // skip
            // ---
            ("foo -- --skip my-pattern --skip your-pattern", {
                let mut patterns = TestFilterPatterns::default();
                patterns.add_skip_pattern("my-pattern".to_owned());
                patterns.add_skip_pattern("your-pattern".to_owned());
                patterns
            }),
            ("foo -- pattern1 --skip my-pattern --skip your-pattern", {
                let mut patterns = TestFilterPatterns::default();
                patterns.add_substring_pattern("pattern1".to_owned());
                patterns.add_skip_pattern("my-pattern".to_owned());
                patterns.add_skip_pattern("your-pattern".to_owned());
                patterns
            }),
            // ---
            // skip and exact
            // ---
            (
                "foo -- --skip my-pattern --skip your-pattern exact1 --exact pattern2",
                {
                    let mut patterns = TestFilterPatterns::default();
                    patterns.add_skip_exact_pattern("my-pattern".to_owned());
                    patterns.add_skip_exact_pattern("your-pattern".to_owned());
                    patterns.add_exact_pattern("exact1".to_owned());
                    patterns.add_exact_pattern("pattern2".to_owned());
                    patterns
                },
            ),
        ];
        let invalid = &[
            // ---
            // duplicated
            // ---
            ("foo -- --include-ignored --include-ignored", "duplicated"),
            ("foo -- --ignored --ignored", "duplicated"),
            ("foo -- --exact --exact", "duplicated"),
            // ---
            // mutually exclusive
            // ---
            ("foo -- --ignored --include-ignored", "mutually exclusive"),
            ("foo --run-ignored all -- --ignored", "mutually exclusive"),
            // ---
            // missing required argument
            // ---
            ("foo -- --skip", "missing required argument"),
            // ---
            // unsupported
            // ---
            ("foo -- --bar", "unsupported"),
        ];

        for (a, b) in valid {
            let a_str = format!(
                "{:?}",
                get_test_filter_builder(a).unwrap_or_else(|_| panic!("failed to parse {a}"))
            );
            let b_str = format!(
                "{:?}",
                get_test_filter_builder(b).unwrap_or_else(|_| panic!("failed to parse {b}"))
            );
            assert_eq!(a_str, b_str);
        }

        for (args, patterns) in skip_exact {
            let builder =
                get_test_filter_builder(args).unwrap_or_else(|_| panic!("failed to parse {args}"));

            let builder2 =
                TestFilterBuilder::new(RunIgnored::Default, None, patterns.clone(), Vec::new())
                    .unwrap_or_else(|_| panic!("failed to build TestFilterBuilder"));

            assert_eq!(builder, builder2, "{args} matches expected");
        }

        for (s, r) in invalid {
            let res = get_test_filter_builder(s);
            if let Err(ExpectedError::TestBinaryArgsParseError { reason, .. }) = &res {
                assert_eq!(reason, r);
            } else {
                panic!(
                    "{s} should have errored out with TestBinaryArgsParseError, actual: {res:?}",
                );
            }
        }
    }
}
