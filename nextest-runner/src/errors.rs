// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Errors produced by nextest.

use crate::{
    cargo_config::{TargetTriple, TargetTripleSource},
    config::{ConfigExperimental, CustomTestGroup, ScriptId, TestGroup},
    helpers::{display_exited_with, dylib_path_envvar},
    redact::Redactor,
    reuse_build::{ArchiveFormat, ArchiveStep},
    target_runner::PlatformRunnerSource,
};
use camino::{FromPathBufError, Utf8Path, Utf8PathBuf};
use config::ConfigError;
use indent_write::{fmt::IndentWriter, indentable::Indented};
use itertools::{Either, Itertools};
use nextest_filtering::errors::FiltersetParseErrors;
use nextest_metadata::RustBinaryId;
use smol_str::SmolStr;
use std::{
    borrow::Cow,
    collections::BTreeSet,
    env::JoinPathsError,
    fmt::{self, Write as _},
    process::ExitStatus,
    sync::Arc,
};
use target_spec_miette::IntoMietteDiagnostic;
use thiserror::Error;

/// An error that occurred while parsing the config.
#[derive(Debug, Error)]
#[error(
    "failed to parse nextest config at `{config_file}`{}",
    provided_by_tool(tool.as_deref())
)]
#[non_exhaustive]
pub struct ConfigParseError {
    config_file: Utf8PathBuf,
    tool: Option<String>,
    #[source]
    kind: ConfigParseErrorKind,
}

impl ConfigParseError {
    pub(crate) fn new(
        config_file: impl Into<Utf8PathBuf>,
        tool: Option<&str>,
        kind: ConfigParseErrorKind,
    ) -> Self {
        Self {
            config_file: config_file.into(),
            tool: tool.map(|s| s.to_owned()),
            kind,
        }
    }

    /// Returns the config file for this error.
    pub fn config_file(&self) -> &Utf8Path {
        &self.config_file
    }

    /// Returns the tool name associated with this error.
    pub fn tool(&self) -> Option<&str> {
        self.tool.as_deref()
    }

    /// Returns the kind of error this is.
    pub fn kind(&self) -> &ConfigParseErrorKind {
        &self.kind
    }
}

/// Returns the string ` provided by tool <tool>`, if `tool` is `Some`.
pub fn provided_by_tool(tool: Option<&str>) -> String {
    match tool {
        Some(tool) => format!(" provided by tool `{tool}`"),
        None => String::new(),
    }
}

/// The kind of error that occurred while parsing a config.
///
/// Returned by [`ConfigParseError::kind`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConfigParseErrorKind {
    /// An error occurred while building the config.
    #[error(transparent)]
    BuildError(Box<ConfigError>),
    #[error(transparent)]
    /// An error occurred while deserializing the config.
    DeserializeError(Box<serde_path_to_error::Error<ConfigError>>),
    /// An error occurred while reading the config file (version only).
    #[error(transparent)]
    VersionOnlyReadError(std::io::Error),
    /// An error occurred while deserializing the config (version only).
    #[error(transparent)]
    VersionOnlyDeserializeError(Box<serde_path_to_error::Error<toml::de::Error>>),
    /// Errors occurred while compiling configuration strings.
    #[error("error parsing compiled data (destructure this variant for more details)")]
    CompileErrors(Vec<ConfigCompileError>),
    /// An invalid set of test groups was defined by the user.
    #[error("invalid test groups defined: {}\n(test groups cannot start with '@tool:' unless specified by a tool)", .0.iter().join(", "))]
    InvalidTestGroupsDefined(BTreeSet<CustomTestGroup>),
    /// An invalid set of test groups was defined by a tool config file.
    #[error(
        "invalid test groups defined by tool: {}\n(test groups must start with '@tool:<tool-name>:')", .0.iter().join(", "))]
    InvalidTestGroupsDefinedByTool(BTreeSet<CustomTestGroup>),
    /// Some test groups were unknown.
    #[error("unknown test groups specified by config (destructure this variant for more details)")]
    UnknownTestGroups {
        /// The list of errors that occurred.
        errors: Vec<UnknownTestGroupError>,

        /// Known groups up to this point.
        known_groups: BTreeSet<TestGroup>,
    },
    /// An invalid set of config scripts was defined by the user.
    #[error("invalid config scripts defined: {}\n(config scripts cannot start with '@tool:' unless specified by a tool)", .0.iter().join(", "))]
    InvalidConfigScriptsDefined(BTreeSet<ScriptId>),
    /// An invalid set of config scripts was defined by a tool config file.
    #[error(
        "invalid config scripts defined by tool: {}\n(config scripts must start with '@tool:<tool-name>:')", .0.iter().join(", "))]
    InvalidConfigScriptsDefinedByTool(BTreeSet<ScriptId>),
    /// Some config scripts were unknown.
    #[error(
        "unknown config scripts specified by config (destructure this variant for more details)"
    )]
    UnknownConfigScripts {
        /// The list of errors that occurred.
        errors: Vec<UnknownConfigScriptError>,

        /// Known scripts up to this point.
        known_scripts: BTreeSet<ScriptId>,
    },
    /// An unknown experimental feature or features were defined.
    #[error("unknown experimental features defined (destructure this variant for more details)")]
    UnknownExperimentalFeatures {
        /// The set of unknown features.
        unknown: BTreeSet<String>,

        /// The set of known features.
        known: BTreeSet<ConfigExperimental>,
    },
    /// A tool specified an experimental feature.
    ///
    /// Tools are not allowed to specify experimental features.
    #[error(
        "tool config file specifies experimental features `{}` \
         -- only repository config files can do so",
        .features.iter().join(", "),
    )]
    ExperimentalFeaturesInToolConfig {
        /// The name of the experimental feature.
        features: BTreeSet<String>,
    },
    /// An experimental feature was used but not enabled.
    #[error("experimental feature `{feature}` is used but not enabled")]
    ExperimentalFeatureNotEnabled {
        /// The feature that was not enabled.
        feature: ConfigExperimental,
    },
}

/// An error that occurred while compiling overrides or scripts specified in
/// configuration.
#[derive(Debug)]
#[non_exhaustive]
pub struct ConfigCompileError {
    /// The name of the profile under which the data was found.
    pub profile_name: String,

    /// The section within the profile where the error occurred.
    pub section: ConfigCompileSection,

    /// The kind of error that occurred.
    pub kind: ConfigCompileErrorKind,
}

/// For a [`ConfigCompileError`], the section within the profile where the error
/// occurred.
#[derive(Debug)]
pub enum ConfigCompileSection {
    /// `profile.<profile-name>.default-filter`.
    DefaultFilter,

    /// `[[profile.<profile-name>.overrides]]` at the corresponding index.
    Override(usize),

    /// `[[profile.<profile-name>.scripts]]` at the corresponding index.
    Script(usize),
}

/// The kind of error that occurred while parsing config overrides.
#[derive(Debug)]
#[non_exhaustive]
pub enum ConfigCompileErrorKind {
    /// Neither `platform` nor `filter` were specified.
    ConstraintsNotSpecified {
        /// Whether `default-filter` was specified.
        ///
        /// If default-filter is specified, then specifying `filter` is not
        /// allowed -- so we show a different message in that case.
        default_filter_specified: bool,
    },

    /// Both `filter` and `default-filter` were specified.
    ///
    /// It only makes sense to specify one of the two.
    FilterAndDefaultFilterSpecified,

    /// One or more errors occured while parsing expressions.
    Parse {
        /// A potential error that occurred while parsing the host platform expression.
        host_parse_error: Option<target_spec::Error>,

        /// A potential error that occurred while parsing the target platform expression.
        target_parse_error: Option<target_spec::Error>,

        /// Filterset or default filter parse errors.
        filter_parse_errors: Vec<FiltersetParseErrors>,
    },
}

impl ConfigCompileErrorKind {
    /// Returns [`miette::Report`]s for each error recorded by self.
    pub fn reports(&self) -> impl Iterator<Item = miette::Report> + '_ {
        match self {
            Self::ConstraintsNotSpecified {
                default_filter_specified,
            } => {
                let message = if *default_filter_specified {
                    "for override with `default-filter`, `platform` must also be specified"
                } else {
                    "at least one of `platform` and `filter` must be specified"
                };
                Either::Left(std::iter::once(miette::Report::msg(message)))
            }
            Self::FilterAndDefaultFilterSpecified => {
                Either::Left(std::iter::once(miette::Report::msg(
                    "at most one of `filter` and `default-filter` must be specified",
                )))
            }
            Self::Parse {
                host_parse_error,
                target_parse_error,
                filter_parse_errors,
            } => {
                let host_parse_report = host_parse_error
                    .as_ref()
                    .map(|error| miette::Report::new_boxed(error.clone().into_diagnostic()));
                let target_parse_report = target_parse_error
                    .as_ref()
                    .map(|error| miette::Report::new_boxed(error.clone().into_diagnostic()));
                let filter_parse_reports =
                    filter_parse_errors.iter().flat_map(|filter_parse_errors| {
                        filter_parse_errors.errors.iter().map(|single_error| {
                            miette::Report::new(single_error.clone())
                                .with_source_code(filter_parse_errors.input.to_owned())
                        })
                    });

                Either::Right(
                    host_parse_report
                        .into_iter()
                        .chain(target_parse_report)
                        .chain(filter_parse_reports),
                )
            }
        }
    }
}

/// A test priority specified was out of range.
#[derive(Clone, Debug, Error)]
#[error("test priority ({priority}) out of range: must be between -100 and 100, both inclusive")]
pub struct TestPriorityOutOfRange {
    /// The priority that was out of range.
    pub priority: i8,
}

/// An execution error occurred while attempting to start a test.
#[derive(Clone, Debug, Error)]
pub enum ChildStartError {
    /// An error occurred while creating a temporary path for a setup script.
    #[error("error creating temporary path for setup script")]
    TempPath(#[source] Arc<std::io::Error>),

    /// An error occurred while spawning the child process.
    #[error("error spawning child process")]
    Spawn(#[source] Arc<std::io::Error>),
}

/// An error that occurred while reading the output of a setup script.
#[derive(Clone, Debug, Error)]
pub enum SetupScriptOutputError {
    /// An error occurred while opening the setup script environment file.
    #[error("error opening environment file `{path}`")]
    EnvFileOpen {
        /// The path to the environment file.
        path: Utf8PathBuf,

        /// The underlying error.
        #[source]
        error: Arc<std::io::Error>,
    },

    /// An error occurred while reading the setup script environment file.
    #[error("error reading environment file `{path}`")]
    EnvFileRead {
        /// The path to the environment file.
        path: Utf8PathBuf,

        /// The underlying error.
        #[source]
        error: Arc<std::io::Error>,
    },

    /// An error occurred while parsing the setup script environment file.
    #[error("line `{line}` in environment file `{path}` not in KEY=VALUE format")]
    EnvFileParse {
        /// The path to the environment file.
        path: Utf8PathBuf,
        /// The line at issue.
        line: String,
    },

    /// An environment variable key was reserved.
    #[error("key `{key}` begins with `NEXTEST`, which is reserved for internal use")]
    EnvFileReservedKey {
        /// The environment variable name.
        key: String,
    },
}

/// A list of errors that implements `Error`.
///
/// In the future, we'll likely want to replace this with a `miette::Diagnostic`-based error, since
/// that supports multiple causes via "related".
#[derive(Clone, Debug)]
pub struct ErrorList<T> {
    // A description of what the errors are.
    description: &'static str,
    // Invariant: this list is non-empty.
    inner: Vec<T>,
}

impl<T: std::error::Error> ErrorList<T> {
    pub(crate) fn new<U>(description: &'static str, errors: Vec<U>) -> Option<Self>
    where
        T: From<U>,
    {
        if errors.is_empty() {
            None
        } else {
            Some(Self {
                description,
                inner: errors.into_iter().map(T::from).collect(),
            })
        }
    }

    /// Returns a short summary of the error list.
    pub(crate) fn short_message(&self) -> String {
        let string = self.to_string();
        match string.lines().next() {
            // Remove a trailing colon if it exists for a better UX.
            Some(first_line) => first_line.trim_end_matches(':').to_string(),
            None => String::new(),
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> {
        self.inner.iter()
    }
}

impl<T: std::error::Error> fmt::Display for ErrorList<T> {
    fn fmt(&self, mut f: &mut fmt::Formatter) -> fmt::Result {
        // If a single error occurred, pretend that this is just that.
        if self.inner.len() == 1 {
            return write!(f, "{}", self.inner[0]);
        }

        // Otherwise, list all errors.
        writeln!(
            f,
            "{} errors occurred {}:",
            self.inner.len(),
            self.description,
        )?;
        for error in &self.inner {
            let mut indent = IndentWriter::new_skip_initial("  ", f);
            writeln!(indent, "* {}", DisplayErrorChain::new(error))?;
            f = indent.into_inner();
        }
        Ok(())
    }
}

impl<T: std::error::Error> std::error::Error for ErrorList<T> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        if self.inner.len() == 1 {
            self.inner[0].source()
        } else {
            // More than one error occurred, so we can't return a single error here. Instead, we
            // return `None` and display the chain of causes in `fmt::Display`.
            None
        }
    }
}

/// A wrapper type to display a chain of errors with internal indentation.
///
/// This is similar to the display-error-chain crate, but uses IndentWriter
/// internally to ensure that subsequent lines are also nested.
pub(crate) struct DisplayErrorChain<E> {
    error: E,
    initial_indent: &'static str,
}

impl<E: std::error::Error> DisplayErrorChain<E> {
    pub(crate) fn new(error: E) -> Self {
        Self {
            error,
            initial_indent: "",
        }
    }

    pub(crate) fn new_with_initial_indent(initial_indent: &'static str, error: E) -> Self {
        Self {
            error,
            initial_indent,
        }
    }
}

impl<E> fmt::Display for DisplayErrorChain<E>
where
    E: std::error::Error,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut writer = IndentWriter::new(self.initial_indent, f);
        write!(writer, "{}", self.error)?;

        let Some(mut cause) = self.error.source() else {
            return Ok(());
        };

        write!(writer, "\n  caused by:")?;

        loop {
            writeln!(writer)?;
            let mut indent = IndentWriter::new_skip_initial("    ", writer);
            write!(indent, "  - {}", cause)?;

            let Some(next_cause) = cause.source() else {
                break Ok(());
            };

            cause = next_cause;
            writer = indent.into_inner();
        }
    }
}

/// An error was returned while managing a child process or reading its output.
#[derive(Clone, Debug, Error)]
pub enum ChildError {
    /// An error occurred while reading from a child file descriptor.
    #[error(transparent)]
    Fd(#[from] ChildFdError),

    /// An error occurred while reading the output of a setup script.
    #[error(transparent)]
    SetupScriptOutput(#[from] SetupScriptOutputError),
}

/// An error was returned while reading from child a file descriptor.
#[derive(Clone, Debug, Error)]
pub enum ChildFdError {
    /// An error occurred while reading standard output.
    #[error("error reading standard output")]
    ReadStdout(#[source] Arc<std::io::Error>),

    /// An error occurred while reading standard error.
    #[error("error reading standard error")]
    ReadStderr(#[source] Arc<std::io::Error>),

    /// An error occurred while reading a combined stream.
    #[error("error reading combined stream")]
    ReadCombined(#[source] Arc<std::io::Error>),

    /// An error occurred while waiting for the child process to exit.
    #[error("error waiting for child process to exit")]
    Wait(#[source] Arc<std::io::Error>),
}

/// An unknown test group was specified in the config.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct UnknownTestGroupError {
    /// The name of the profile under which the unknown test group was found.
    pub profile_name: String,

    /// The name of the unknown test group.
    pub name: TestGroup,
}

/// An unknown script was specified in the config.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct UnknownConfigScriptError {
    /// The name of the profile under which the unknown script was found.
    pub profile_name: String,

    /// The name of the unknown script.
    pub name: ScriptId,
}

/// An error which indicates that a profile was requested but not known to nextest.
#[derive(Clone, Debug, Error)]
#[error("profile `{profile} not found (known profiles: {})`", .all_profiles.join(", "))]
pub struct ProfileNotFound {
    profile: String,
    all_profiles: Vec<String>,
}

impl ProfileNotFound {
    pub(crate) fn new(
        profile: impl Into<String>,
        all_profiles: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        let mut all_profiles: Vec<_> = all_profiles.into_iter().map(|s| s.into()).collect();
        all_profiles.sort_unstable();
        Self {
            profile: profile.into(),
            all_profiles,
        }
    }
}

/// An identifier is invalid.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum InvalidIdentifier {
    /// The identifier is empty.
    #[error("identifier is empty")]
    Empty,

    /// The identifier is not in the correct Unicode format.
    #[error("invalid identifier `{0}`")]
    InvalidXid(SmolStr),

    /// This tool identifier doesn't match the expected pattern.
    #[error("tool identifier not of the form \"@tool:tool-name:identifier\": `{0}`")]
    ToolIdentifierInvalidFormat(SmolStr),

    /// One of the components of this tool identifier is empty.
    #[error("tool identifier has empty component: `{0}`")]
    ToolComponentEmpty(SmolStr),

    /// The tool identifier is not in the correct Unicode format.
    #[error("invalid tool identifier `{0}`")]
    ToolIdentifierInvalidXid(SmolStr),
}

/// The name of a test group is invalid (not a valid identifier).
#[derive(Clone, Debug, Error)]
#[error("invalid custom test group name: {0}")]
pub struct InvalidCustomTestGroupName(pub InvalidIdentifier);

/// The name of a configuration script is invalid (not a valid identifier).
#[derive(Clone, Debug, Error)]
#[error("invalid configuration script name: {0}")]
pub struct InvalidConfigScriptName(pub InvalidIdentifier);

/// Error returned while parsing a [`ToolConfigFile`](crate::config::ToolConfigFile) value.
#[derive(Clone, Debug, Error)]
pub enum ToolConfigFileParseError {
    #[error(
        "tool-config-file has invalid format: {input}\n(hint: tool configs must be in the format <tool-name>:<path>)"
    )]
    /// The input was not in the format "tool:path".
    InvalidFormat {
        /// The input that failed to parse.
        input: String,
    },

    /// The tool name was empty.
    #[error("tool-config-file has empty tool name: {input}")]
    EmptyToolName {
        /// The input that failed to parse.
        input: String,
    },

    /// The config file path was empty.
    #[error("tool-config-file has empty config file path: {input}")]
    EmptyConfigFile {
        /// The input that failed to parse.
        input: String,
    },

    /// The config file was not an absolute path.
    #[error("tool-config-file is not an absolute path: {config_file}")]
    ConfigFileNotAbsolute {
        /// The file name that wasn't absolute.
        config_file: Utf8PathBuf,
    },
}

/// Error returned while parsing a [`MaxFail`](crate::config::MaxFail) input.
#[derive(Clone, Debug, Error)]
#[error("unrecognized value for max-fail: {input}\n(hint: expected either a positive integer or \"all\")")]
pub struct MaxFailParseError {
    /// The input that failed to parse.
    pub input: String,
}

impl MaxFailParseError {
    pub(crate) fn new(input: impl Into<String>) -> Self {
        Self {
            input: input.into(),
        }
    }
}

/// Error returned while parsing a [`TestThreads`](crate::config::TestThreads) value.
#[derive(Clone, Debug, Error)]
#[error(
    "unrecognized value for test-threads: {input}\n(hint: expected either an integer or \"num-cpus\")"
)]
pub struct TestThreadsParseError {
    /// The input that failed to parse.
    pub input: String,
}

impl TestThreadsParseError {
    pub(crate) fn new(input: impl Into<String>) -> Self {
        Self {
            input: input.into(),
        }
    }
}

/// An error that occurs while parsing a
/// [`PartitionerBuilder`](crate::partition::PartitionerBuilder) input.
#[derive(Clone, Debug, Error)]
pub struct PartitionerBuilderParseError {
    expected_format: Option<&'static str>,
    message: Cow<'static, str>,
}

impl PartitionerBuilderParseError {
    pub(crate) fn new(
        expected_format: Option<&'static str>,
        message: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            expected_format,
            message: message.into(),
        }
    }
}

impl fmt::Display for PartitionerBuilderParseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.expected_format {
            Some(format) => {
                write!(
                    f,
                    "partition must be in the format \"{}\":\n{}",
                    format, self.message
                )
            }
            None => write!(f, "{}", self.message),
        }
    }
}

/// An error that occurs while operating on a
/// [`TestFilterBuilder`](crate::test_filter::TestFilterBuilder).
#[derive(Clone, Debug, Error)]
pub enum TestFilterBuilderError {
    /// An error that occurred while constructing test filters.
    #[error("error constructing test filters")]
    Construct {
        /// The underlying error.
        #[from]
        error: aho_corasick::BuildError,
    },
}

/// An error occurred in [`PathMapper::new`](crate::reuse_build::PathMapper::new).
#[derive(Debug, Error)]
pub enum PathMapperConstructError {
    /// An error occurred while canonicalizing a directory.
    #[error("{kind} `{input}` failed to canonicalize")]
    Canonicalization {
        /// The directory that failed to be canonicalized.
        kind: PathMapperConstructKind,

        /// The input provided.
        input: Utf8PathBuf,

        /// The error that occurred.
        #[source]
        err: std::io::Error,
    },
    /// The canonicalized path isn't valid UTF-8.
    #[error("{kind} `{input}` canonicalized to a non-UTF-8 path")]
    NonUtf8Path {
        /// The directory that failed to be canonicalized.
        kind: PathMapperConstructKind,

        /// The input provided.
        input: Utf8PathBuf,

        /// The underlying error.
        #[source]
        err: FromPathBufError,
    },
    /// A provided input is not a directory.
    #[error("{kind} `{canonicalized_path}` is not a directory")]
    NotADirectory {
        /// The directory that failed to be canonicalized.
        kind: PathMapperConstructKind,

        /// The input provided.
        input: Utf8PathBuf,

        /// The canonicalized path that wasn't a directory.
        canonicalized_path: Utf8PathBuf,
    },
}

impl PathMapperConstructError {
    /// The kind of directory.
    pub fn kind(&self) -> PathMapperConstructKind {
        match self {
            Self::Canonicalization { kind, .. }
            | Self::NonUtf8Path { kind, .. }
            | Self::NotADirectory { kind, .. } => *kind,
        }
    }

    /// The input path that failed.
    pub fn input(&self) -> &Utf8Path {
        match self {
            Self::Canonicalization { input, .. }
            | Self::NonUtf8Path { input, .. }
            | Self::NotADirectory { input, .. } => input,
        }
    }
}

/// The kind of directory that failed to be read in
/// [`PathMapper::new`](crate::reuse_build::PathMapper::new).
///
/// Returned as part of [`PathMapperConstructError`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PathMapperConstructKind {
    /// The workspace root.
    WorkspaceRoot,

    /// The target directory.
    TargetDir,
}

impl fmt::Display for PathMapperConstructKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::WorkspaceRoot => write!(f, "remapped workspace root"),
            Self::TargetDir => write!(f, "remapped target directory"),
        }
    }
}

/// An error that occurs while parsing Rust build metadata from a summary.
#[derive(Debug, Error)]
pub enum RustBuildMetaParseError {
    /// An error occurred while deserializing the platform.
    #[error("error deserializing platform from build metadata")]
    PlatformDeserializeError(#[from] target_spec::Error),

    /// The host platform could not be determined.
    #[error("the host platform could not be determined")]
    DetectBuildTargetError(#[source] target_spec::Error),

    /// The build metadata includes features unsupported.
    #[error("unsupported features in the build metadata: {message}")]
    Unsupported {
        /// The detailed error message.
        message: String,
    },
}

/// Error returned when a user-supplied format version fails to be parsed to a
/// valid and supported version.
#[derive(Clone, Debug, thiserror::Error)]
#[error("invalid format version: {input}")]
pub struct FormatVersionError {
    /// The input that failed to parse.
    pub input: String,
    /// The underlying error.
    #[source]
    pub error: FormatVersionErrorInner,
}

/// The different errors that can occur when parsing and validating a format version.
#[derive(Clone, Debug, thiserror::Error)]
pub enum FormatVersionErrorInner {
    /// The input did not have a valid syntax.
    #[error("expected format version in form of `{expected}`")]
    InvalidFormat {
        /// The expected pseudo format.
        expected: &'static str,
    },
    /// A decimal integer was expected but could not be parsed.
    #[error("version component `{which}` could not be parsed as an integer")]
    InvalidInteger {
        /// Which component was invalid.
        which: &'static str,
        /// The parse failure.
        #[source]
        err: std::num::ParseIntError,
    },
    /// The version component was not within the expected range.
    #[error("version component `{which}` value {value} is out of range {range:?}")]
    InvalidValue {
        /// The component which was out of range.
        which: &'static str,
        /// The value that was parsed.
        value: u8,
        /// The range of valid values for the component.
        range: std::ops::Range<u8>,
    },
}

/// An error that occurs in [`BinaryList::from_messages`](crate::list::BinaryList::from_messages) or
/// [`RustTestArtifact::from_binary_list`](crate::list::RustTestArtifact::from_binary_list).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum FromMessagesError {
    /// An error occurred while reading Cargo's JSON messages.
    #[error("error reading Cargo JSON messages")]
    ReadMessages(#[source] std::io::Error),

    /// An error occurred while querying the package graph.
    #[error("error querying package graph")]
    PackageGraph(#[source] guppy::Error),

    /// A target in the package graph was missing `kind` information.
    #[error("missing kind for target {binary_name} in package {package_name}")]
    MissingTargetKind {
        /// The name of the malformed package.
        package_name: String,
        /// The name of the malformed target within the package.
        binary_name: String,
    },
}

/// An error that occurs while parsing test list output.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CreateTestListError {
    /// The proposed cwd for a process is not a directory.
    #[error(
        "for `{binary_id}`, current directory `{cwd}` is not a directory\n\
         (hint: ensure project source is available at this location)"
    )]
    CwdIsNotDir {
        /// The binary ID for which the current directory wasn't found.
        binary_id: RustBinaryId,

        /// The current directory that wasn't found.
        cwd: Utf8PathBuf,
    },

    /// Running a command to gather the list of tests failed to execute.
    #[error(
        "for `{binary_id}`, running command `{}` failed to execute",
        shell_words::join(command)
    )]
    CommandExecFail {
        /// The binary ID for which gathering the list of tests failed.
        binary_id: RustBinaryId,

        /// The command that was run.
        command: Vec<String>,

        /// The underlying error.
        #[source]
        error: std::io::Error,
    },

    /// Running a command to gather the list of tests failed failed with a non-zero exit code.
    #[error(
        "for `{binary_id}`, command `{}` {}\n--- stdout:\n{}\n--- stderr:\n{}\n---",
        shell_words::join(command),
        display_exited_with(*exit_status),
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr),
    )]
    CommandFail {
        /// The binary ID for which gathering the list of tests failed.
        binary_id: RustBinaryId,

        /// The command that was run.
        command: Vec<String>,

        /// The exit status with which the command failed.
        exit_status: ExitStatus,

        /// Standard output for the command.
        stdout: Vec<u8>,

        /// Standard error for the command.
        stderr: Vec<u8>,
    },

    /// Running a command to gather the list of tests produced a non-UTF-8 standard output.
    #[error(
        "for `{binary_id}`, command `{}` produced non-UTF-8 output:\n--- stdout:\n{}\n--- stderr:\n{}\n---",
        shell_words::join(command),
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr),
    )]
    CommandNonUtf8 {
        /// The binary ID for which gathering the list of tests failed.
        binary_id: RustBinaryId,

        /// The command that was run.
        command: Vec<String>,

        /// Standard output for the command.
        stdout: Vec<u8>,

        /// Standard error for the command.
        stderr: Vec<u8>,
    },

    /// An error occurred while parsing a line in the test output.
    #[error("for `{binary_id}`, {message}\nfull output:\n{full_output}")]
    ParseLine {
        /// The binary ID for which parsing the list of tests failed.
        binary_id: RustBinaryId,

        /// A descriptive message.
        message: Cow<'static, str>,

        /// The full output.
        full_output: String,
    },

    /// An error occurred while joining paths for dynamic libraries.
    #[error(
        "error joining dynamic library paths for {}: [{}]",
        dylib_path_envvar(),
        itertools::join(.new_paths, ", ")
    )]
    DylibJoinPaths {
        /// New paths attempted to be added to the dynamic library environment variable.
        new_paths: Vec<Utf8PathBuf>,

        /// The underlying error.
        #[source]
        error: JoinPathsError,
    },

    /// Creating a Tokio runtime failed.
    #[error("error creating Tokio runtime")]
    TokioRuntimeCreate(#[source] std::io::Error),
}

impl CreateTestListError {
    pub(crate) fn parse_line(
        binary_id: RustBinaryId,
        message: impl Into<Cow<'static, str>>,
        full_output: impl Into<String>,
    ) -> Self {
        Self::ParseLine {
            binary_id,
            message: message.into(),
            full_output: full_output.into(),
        }
    }

    pub(crate) fn dylib_join_paths(new_paths: Vec<Utf8PathBuf>, error: JoinPathsError) -> Self {
        Self::DylibJoinPaths { new_paths, error }
    }
}

/// An error that occurs while writing list output.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WriteTestListError {
    /// An error occurred while writing the list to the provided output.
    #[error("error writing to output")]
    Io(#[source] std::io::Error),

    /// An error occurred while serializing JSON, or while writing it to the provided output.
    #[error("error serializing to JSON")]
    Json(#[source] serde_json::Error),
}

/// An error occurred while configuring handles.
///
/// Only relevant on Windows.
#[derive(Debug, Error)]
pub enum ConfigureHandleInheritanceError {
    /// An error occurred. This can only happen on Windows.
    #[cfg(windows)]
    #[error("error configuring handle inheritance")]
    WindowsError(#[from] std::io::Error),
}

/// An error that occurs while building the test runner.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TestRunnerBuildError {
    /// An error occurred while creating the Tokio runtime.
    #[error("error creating Tokio runtime")]
    TokioRuntimeCreate(#[source] std::io::Error),

    /// An error occurred while setting up signals.
    #[error("error setting up signals")]
    SignalHandlerSetupError(#[from] SignalHandlerSetupError),
}

/// Errors that occurred while managing test runner Tokio tasks.
#[derive(Debug, Error)]
pub struct TestRunnerExecuteErrors<E> {
    /// An error that occurred while reporting results to the reporter callback.
    pub report_error: Option<E>,

    /// Join errors (typically panics) that occurred while running the test
    /// runner.
    pub join_errors: Vec<tokio::task::JoinError>,
}

impl<E: std::error::Error> fmt::Display for TestRunnerExecuteErrors<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(report_error) = &self.report_error {
            write!(f, "error reporting results: {}", report_error)?;
        }

        if !self.join_errors.is_empty() {
            if self.report_error.is_some() {
                write!(f, "; ")?;
            }

            write!(f, "errors joining tasks: ")?;

            for (i, join_error) in self.join_errors.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }

                write!(f, "{}", join_error)?;
            }
        }

        Ok(())
    }
}

/// Represents an unknown archive format.
///
/// Returned by [`ArchiveFormat::autodetect`].
#[derive(Debug, Error)]
#[error(
    "could not detect archive format from file name `{file_name}` (supported extensions: {})",
    supported_extensions()
)]
pub struct UnknownArchiveFormat {
    /// The name of the archive file without any leading components.
    pub file_name: String,
}

fn supported_extensions() -> String {
    ArchiveFormat::SUPPORTED_FORMATS
        .iter()
        .map(|(extension, _)| *extension)
        .join(", ")
}

/// An error that occurs while archiving data.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ArchiveCreateError {
    /// An error occurred while creating the binary list to be written.
    #[error("error creating binary list")]
    CreateBinaryList(#[source] WriteTestListError),

    /// An extra path was missing.
    #[error("extra path `{}` not found", .redactor.redact_path(path))]
    MissingExtraPath {
        /// The path that was missing.
        path: Utf8PathBuf,

        /// A redactor for the path.
        ///
        /// (This should eventually move to being a field for a wrapper struct, but it's okay for
        /// now.)
        redactor: Redactor,
    },

    /// An error occurred while reading data from a file on disk.
    #[error("while archiving {step}, error writing {} `{path}` to archive", kind_str(*.is_dir))]
    InputFileRead {
        /// The step that the archive errored at.
        step: ArchiveStep,

        /// The name of the file that could not be read.
        path: Utf8PathBuf,

        /// Whether this is a directory. `None` means the status was unknown.
        is_dir: Option<bool>,

        /// The error that occurred.
        #[source]
        error: std::io::Error,
    },

    /// An error occurred while reading entries from a directory on disk.
    #[error("error reading directory entry from `{path}")]
    DirEntryRead {
        /// The name of the directory from which entries couldn't be read.
        path: Utf8PathBuf,

        /// The error that occurred.
        #[source]
        error: std::io::Error,
    },

    /// An error occurred while writing data to the output file.
    #[error("error writing to archive")]
    OutputArchiveIo(#[source] std::io::Error),

    /// An error occurred in the reporter.
    #[error("error reporting archive status")]
    ReporterIo(#[source] std::io::Error),
}

fn kind_str(is_dir: Option<bool>) -> &'static str {
    match is_dir {
        Some(true) => "directory",
        Some(false) => "file",
        None => "path",
    }
}

/// An error occurred while materializing a metadata path.
#[derive(Debug, Error)]
pub enum MetadataMaterializeError {
    /// An I/O error occurred while reading the metadata file.
    #[error("I/O error reading metadata file `{path}`")]
    Read {
        /// The path that was being read.
        path: Utf8PathBuf,

        /// The error that occurred.
        #[source]
        error: std::io::Error,
    },

    /// A JSON deserialization error occurred while reading the metadata file.
    #[error("error deserializing metadata file `{path}`")]
    Deserialize {
        /// The path that was being read.
        path: Utf8PathBuf,

        /// The error that occurred.
        #[source]
        error: serde_json::Error,
    },

    /// An error occurred while parsing Rust build metadata.
    #[error("error parsing Rust build metadata from `{path}`")]
    RustBuildMeta {
        /// The path that was deserialized.
        path: Utf8PathBuf,

        /// The error that occurred.
        #[source]
        error: RustBuildMetaParseError,
    },

    /// An error occurred converting data into a `PackageGraph`.
    #[error("error building package graph from `{path}`")]
    PackageGraphConstruct {
        /// The path that was deserialized.
        path: Utf8PathBuf,

        /// The error that occurred.
        #[source]
        error: guppy::Error,
    },
}

/// An error occurred while reading a file.
///
/// Returned as part of both [`ArchiveCreateError`] and [`ArchiveExtractError`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ArchiveReadError {
    /// An I/O error occurred while reading the archive.
    #[error("I/O error reading archive")]
    Io(#[source] std::io::Error),

    /// A path wasn't valid UTF-8.
    #[error("path in archive `{}` wasn't valid UTF-8", String::from_utf8_lossy(.0))]
    NonUtf8Path(Vec<u8>),

    /// A file path within the archive didn't begin with "target/".
    #[error("path in archive `{0}` doesn't start with `target/`")]
    NoTargetPrefix(Utf8PathBuf),

    /// A file path within the archive had an invalid component within it.
    #[error("path in archive `{path}` contains an invalid component `{component}`")]
    InvalidComponent {
        /// The path that had an invalid component.
        path: Utf8PathBuf,

        /// The invalid component.
        component: String,
    },

    /// An error occurred while reading a checksum.
    #[error("corrupted archive: checksum read error for path `{path}`")]
    ChecksumRead {
        /// The path for which there was a checksum read error.
        path: Utf8PathBuf,

        /// The error that occurred.
        #[source]
        error: std::io::Error,
    },

    /// An entry had an invalid checksum.
    #[error("corrupted archive: invalid checksum for path `{path}`")]
    InvalidChecksum {
        /// The path that had an invalid checksum.
        path: Utf8PathBuf,

        /// The expected checksum.
        expected: u32,

        /// The actual checksum.
        actual: u32,
    },

    /// A metadata file wasn't found.
    #[error("metadata file `{0}` not found in archive")]
    MetadataFileNotFound(&'static Utf8Path),

    /// An error occurred while deserializing a metadata file.
    #[error("error deserializing metadata file `{path}` in archive")]
    MetadataDeserializeError {
        /// The name of the metadata file.
        path: &'static Utf8Path,

        /// The deserialize error.
        #[source]
        error: serde_json::Error,
    },

    /// An error occurred while building a `PackageGraph`.
    #[error("error building package graph from `{path}` in archive")]
    PackageGraphConstructError {
        /// The name of the metadata file.
        path: &'static Utf8Path,

        /// The error.
        #[source]
        error: guppy::Error,
    },
}

/// An error occurred while extracting a file.
///
/// Returned by [`extract_archive`](crate::reuse_build::ReuseBuildInfo::extract_archive).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ArchiveExtractError {
    /// An error occurred while creating a temporary directory.
    #[error("error creating temporary directory")]
    TempDirCreate(#[source] std::io::Error),

    /// An error occurred while canonicalizing the destination directory.
    #[error("error canonicalizing destination directory `{dir}`")]
    DestDirCanonicalization {
        /// The directory that failed to canonicalize.
        dir: Utf8PathBuf,

        /// The error that occurred.
        #[source]
        error: std::io::Error,
    },

    /// The destination already exists and `--overwrite` was not passed in.
    #[error("destination `{0}` already exists")]
    DestinationExists(Utf8PathBuf),

    /// An error occurred while reading the archive.
    #[error("error reading archive")]
    Read(#[source] ArchiveReadError),

    /// An error occurred while deserializing Rust build metadata.
    #[error("error deserializing Rust build metadata")]
    RustBuildMeta(#[from] RustBuildMetaParseError),

    /// An error occurred while writing out a file to the destination directory.
    #[error("error writing file `{path}` to disk")]
    WriteFile {
        /// The path that we couldn't write out.
        path: Utf8PathBuf,

        /// The error that occurred.
        #[source]
        error: std::io::Error,
    },

    /// An error occurred while reporting the extraction status.
    #[error("error reporting extract status")]
    ReporterIo(std::io::Error),
}

/// An error occurred while setting up the JUnit reporter.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum JunitSetupError {
    /// An error occurred while creating the store directory.
    #[error("error creating store directory `{path}`")]
    CreateStoreDir {
        /// The path that we couldn't create.
        path: Utf8PathBuf,

        /// The error that occurred.
        #[source]
        error: std::io::Error,
    },
}

/// An error that occurs while writing an event.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WriteEventError {
    /// An error occurred while writing the event to the provided output.
    #[error("error writing to output")]
    Io(#[source] std::io::Error),

    /// An error occurred while operating on the file system.
    #[error("error operating on path {file}")]
    Fs {
        /// The file being operated on.
        file: Utf8PathBuf,

        /// The underlying IO error.
        #[source]
        error: std::io::Error,
    },

    /// An error occurred while producing JUnit XML.
    #[error("error writing JUnit output to {file}")]
    Junit {
        /// The output file.
        file: Utf8PathBuf,

        /// The underlying error.
        #[source]
        error: quick_junit::SerializeError,
    },
}

/// An error occurred while constructing a [`CargoConfigs`](crate::cargo_config::CargoConfigs)
/// instance.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CargoConfigError {
    /// Failed to retrieve the current directory.
    #[error("failed to retrieve current directory")]
    GetCurrentDir(#[source] std::io::Error),

    /// The current directory was invalid UTF-8.
    #[error("current directory is invalid UTF-8")]
    CurrentDirInvalidUtf8(#[source] FromPathBufError),

    /// Parsing a CLI config option failed.
    #[error("failed to parse --config argument `{config_str}` as TOML")]
    CliConfigParseError {
        /// The CLI config option.
        config_str: String,

        /// The error that occurred trying to parse the config.
        #[source]
        error: toml_edit::TomlError,
    },

    /// Deserializing a CLI config option into domain types failed.
    #[error("failed to deserialize --config argument `{config_str}` as TOML")]
    CliConfigDeError {
        /// The CLI config option.
        config_str: String,

        /// The error that occurred trying to deserialize the config.
        #[source]
        error: toml_edit::de::Error,
    },

    /// A CLI config option is not in the dotted key format.
    #[error(
        "invalid format for --config argument `{config_str}` (should be a dotted key expression)"
    )]
    InvalidCliConfig {
        /// The CLI config option.
        config_str: String,

        /// The reason why this Cargo CLI config is invalid.
        #[source]
        reason: InvalidCargoCliConfigReason,
    },

    /// A non-UTF-8 path was encountered.
    #[error("non-UTF-8 path encountered")]
    NonUtf8Path(#[source] FromPathBufError),

    /// Failed to retrieve the Cargo home directory.
    #[error("failed to retrieve the Cargo home directory")]
    GetCargoHome(#[source] std::io::Error),

    /// Failed to canonicalize a path
    #[error("failed to canonicalize path `{path}")]
    FailedPathCanonicalization {
        /// The path that failed to canonicalize
        path: Utf8PathBuf,

        /// The error the occurred during canonicalization
        #[source]
        error: std::io::Error,
    },

    /// Failed to read config file
    #[error("failed to read config at `{path}`")]
    ConfigReadError {
        /// The path of the config file
        path: Utf8PathBuf,

        /// The error that occurred trying to read the config file
        #[source]
        error: std::io::Error,
    },

    /// Failed to deserialize config file
    #[error(transparent)]
    ConfigParseError(#[from] Box<CargoConfigParseError>),
}

/// Failed to deserialize config file
///
/// We introduce this extra indirection, because of the `clippy::result_large_err` rule on Windows.
#[derive(Debug, Error)]
#[error("failed to parse config at `{path}`")]
pub struct CargoConfigParseError {
    /// The path of the config file
    pub path: Utf8PathBuf,

    /// The error that occurred trying to deserialize the config file
    #[source]
    pub error: toml::de::Error,
}

/// The reason an invalid CLI config failed.
///
/// Part of [`CargoConfigError::InvalidCliConfig`].
#[derive(Copy, Clone, Debug, Error, Eq, PartialEq)]
#[non_exhaustive]
pub enum InvalidCargoCliConfigReason {
    /// The argument is not a TOML dotted key expression.
    #[error("was not a TOML dotted key expression (such as `build.jobs = 2`)")]
    NotDottedKv,

    /// The argument includes non-whitespace decoration.
    #[error("includes non-whitespace decoration")]
    IncludesNonWhitespaceDecoration,

    /// The argument sets a value to an inline table.
    #[error("sets a value to an inline table, which is not accepted")]
    SetsValueToInlineTable,

    /// The argument sets a value to an array of tables.
    #[error("sets a value to an array of tables, which is not accepted")]
    SetsValueToArrayOfTables,

    /// The argument doesn't provide a value.
    #[error("doesn't provide a value")]
    DoesntProvideValue,
}

/// The host platform couldn't be detected.
#[derive(Debug, Error)]
pub enum HostPlatformDetectError {
    /// Spawning `rustc -vV` failed, and detecting the build target failed as
    /// well.
    #[error(
        "error spawning `rustc -vV`, and detecting the build \
         target failed as well\n\
         - rustc spawn error: {}\n\
         - build target error: {}\n",
        DisplayErrorChain::new_with_initial_indent("  ", error),
        DisplayErrorChain::new_with_initial_indent("  ", build_target_error)
    )]
    RustcVvSpawnError {
        /// The error.
        error: std::io::Error,

        /// The error that occurred while detecting the build target.
        build_target_error: Box<target_spec::Error>,
    },

    /// `rustc -vV` exited with a non-zero code, and detecting the build target
    /// failed as well.
    #[error(
        "`rustc -vV` failed with {}, and detecting the \
         build target failed as well\n\
         - `rustc -vV` stdout:\n{}\n\
         - `rustc -vV` stderr:\n{}\n\
         - build target error:\n{}\n",
        status,
        Indented { item: String::from_utf8_lossy(stdout), indent: "  " },
        Indented { item: String::from_utf8_lossy(stderr), indent: "  " },
        DisplayErrorChain::new_with_initial_indent("  ", build_target_error)
    )]
    RustcVvFailed {
        /// The status.
        status: ExitStatus,

        /// The standard output from `rustc -vV`.
        stdout: Vec<u8>,

        /// The standard error from `rustc -vV`.
        stderr: Vec<u8>,

        /// The error that occurred while detecting the build target.
        build_target_error: Box<target_spec::Error>,
    },

    /// Parsing the host platform failed, and detecting the build target failed
    /// as well.
    #[error(
        "parsing `rustc -vV` output failed, and detecting the build target \
         failed as well\n\
         - host platform error:\n{}\n\
         - build target error:\n{}\n",
        DisplayErrorChain::new_with_initial_indent("  ", host_platform_error),
        DisplayErrorChain::new_with_initial_indent("  ", build_target_error)
    )]
    HostPlatformParseError {
        /// The error that occurred while parsing the host platform.
        host_platform_error: Box<target_spec::Error>,

        /// The error that occurred while detecting the build target.
        build_target_error: Box<target_spec::Error>,
    },

    /// Test-only code: `rustc -vV` was not queried, and detecting the build
    /// target failed as well.
    #[error("test-only code, so `rustc -vV` was not called; failed to detect build target")]
    BuildTargetError {
        /// The error that occurred while detecting the build target.
        #[source]
        build_target_error: Box<target_spec::Error>,
    },
}

/// An error occurred while determining the cross-compiling target triple.
#[derive(Debug, Error)]
pub enum TargetTripleError {
    /// The environment variable contained non-utf8 content
    #[error(
        "environment variable '{}' contained non-UTF-8 data",
        TargetTriple::CARGO_BUILD_TARGET_ENV
    )]
    InvalidEnvironmentVar,

    /// An error occurred while deserializing the platform.
    #[error("error deserializing target triple from {source}")]
    TargetSpecError {
        /// The source from which the triple couldn't be parsed.
        source: TargetTripleSource,

        /// The error that occurred parsing the triple.
        #[source]
        error: target_spec::Error,
    },

    /// For a custom platform, reading the target path failed.
    #[error("target path `{path}` is not a valid file")]
    TargetPathReadError {
        /// The source from which the triple couldn't be parsed.
        source: TargetTripleSource,

        /// The path that we tried to read.
        path: Utf8PathBuf,

        /// The error that occurred parsing the triple.
        #[source]
        error: std::io::Error,
    },

    /// Failed to create a temporary directory for a custom platform.
    #[error(
        "for custom platform obtained from {source}, \
         failed to create temporary directory for custom platform"
    )]
    CustomPlatformTempDirError {
        /// The source of the target triple.
        source: TargetTripleSource,

        /// The error that occurred during the create.
        #[source]
        error: std::io::Error,
    },

    /// Failed to write a custom platform to disk.
    #[error(
        "for custom platform obtained from {source}, \
         failed to write JSON to temporary path `{path}`"
    )]
    CustomPlatformWriteError {
        /// The source of the target triple.
        source: TargetTripleSource,

        /// The path that we tried to write to.
        path: Utf8PathBuf,

        /// The error that occurred during the write.
        #[source]
        error: std::io::Error,
    },

    /// Failed to close a temporary directory for an extracted custom platform.
    #[error(
        "for custom platform obtained from {source}, \
         failed to close temporary directory `{dir_path}`"
    )]
    CustomPlatformCloseError {
        /// The source of the target triple.
        source: TargetTripleSource,

        /// The directory that we tried to delete.
        dir_path: Utf8PathBuf,

        /// The error that occurred during the close.
        #[source]
        error: std::io::Error,
    },
}

impl TargetTripleError {
    /// Returns a [`miette::Report`] for the source, if available.
    ///
    /// This should be preferred over [`std::error::Error::source`] if
    /// available.
    pub fn source_report(&self) -> Option<miette::Report> {
        match self {
            Self::TargetSpecError { error, .. } => {
                Some(miette::Report::new_boxed(error.clone().into_diagnostic()))
            }
            // The remaining types are covered via the error source path.
            TargetTripleError::InvalidEnvironmentVar
            | TargetTripleError::TargetPathReadError { .. }
            | TargetTripleError::CustomPlatformTempDirError { .. }
            | TargetTripleError::CustomPlatformWriteError { .. }
            | TargetTripleError::CustomPlatformCloseError { .. } => None,
        }
    }
}

/// An error occurred determining the target runner
#[derive(Debug, Error)]
pub enum TargetRunnerError {
    /// An environment variable contained non-utf8 content
    #[error("environment variable '{0}' contained non-UTF-8 data")]
    InvalidEnvironmentVar(String),

    /// An environment variable or config key was found that matches the target
    /// triple, but it didn't actually contain a binary
    #[error("runner '{key}' = '{value}' did not contain a runner binary")]
    BinaryNotSpecified {
        /// The source under consideration.
        key: PlatformRunnerSource,

        /// The value that was read from the key
        value: String,
    },
}

/// An error that occurred while setting up the signal handler.
#[derive(Debug, Error)]
#[error("error setting up signal handler")]
pub struct SignalHandlerSetupError(#[from] std::io::Error);

/// An error occurred while showing test groups.
#[derive(Debug, Error)]
pub enum ShowTestGroupsError {
    /// Unknown test groups were specified.
    #[error(
        "unknown test groups specified: {}\n(known groups: {})",
        unknown_groups.iter().join(", "),
        known_groups.iter().join(", "),
    )]
    UnknownGroups {
        /// The unknown test groups.
        unknown_groups: BTreeSet<TestGroup>,

        /// All known test groups.
        known_groups: BTreeSet<TestGroup>,
    },
}

#[cfg(feature = "self-update")]
mod self_update_errors {
    use super::*;
    use mukti_metadata::ReleaseStatus;
    use semver::{Version, VersionReq};

    /// An error that occurs while performing a self-update.
    ///
    /// Returned by methods in the [`update`](crate::update) module.
    #[cfg(feature = "self-update")]
    #[derive(Debug, Error)]
    #[non_exhaustive]
    pub enum UpdateError {
        /// Failed to read release metadata from a local path on disk.
        #[error("failed to read release metadata from `{path}`")]
        ReadLocalMetadata {
            /// The path that was read.
            path: Utf8PathBuf,

            /// The error that occurred.
            #[source]
            error: std::io::Error,
        },

        /// An error was generated by `self_update`.
        #[error("self-update failed")]
        SelfUpdate(#[source] self_update::errors::Error),

        /// Deserializing release metadata failed.
        #[error("deserializing release metadata failed")]
        ReleaseMetadataDe(#[source] serde_json::Error),

        /// This version was not found.
        #[error("version `{version}` not found (known versions: {})", known_versions(.known))]
        VersionNotFound {
            /// The version that wasn't found.
            version: Version,

            /// A list of all known versions.
            known: Vec<(Version, ReleaseStatus)>,
        },

        /// No version was found matching a requirement.
        #[error("no version found matching requirement `{req}`")]
        NoMatchForVersionReq {
            /// The version requirement that had no matches.
            req: VersionReq,
        },

        /// The specified mukti project was not found.
        #[error("project {not_found} not found in release metadata (known projects: {})", known.join(", "))]
        MuktiProjectNotFound {
            /// The project that was not found.
            not_found: String,

            /// Known projects.
            known: Vec<String>,
        },

        /// No release information was found for the given target triple.
        #[error(
            "for version {version}, no release information found for target `{triple}` \
            (known targets: {})",
            known_triples.iter().join(", ")
        )]
        NoTargetData {
            /// The version that was fetched.
            version: Version,

            /// The target triple.
            triple: String,

            /// The triples that were found.
            known_triples: BTreeSet<String>,
        },

        /// The current executable could not be determined.
        #[error("the current executable's path could not be determined")]
        CurrentExe(#[source] std::io::Error),

        /// A temporary directory could not be created.
        #[error("temporary directory could not be created at `{location}`")]
        TempDirCreate {
            /// The location where the temporary directory could not be created.
            location: Utf8PathBuf,

            /// The error that occurred.
            #[source]
            error: std::io::Error,
        },

        /// The temporary archive could not be created.
        #[error("temporary archive could not be created at `{archive_path}`")]
        TempArchiveCreate {
            /// The archive file that couldn't be created.
            archive_path: Utf8PathBuf,

            /// The error that occurred.
            #[source]
            error: std::io::Error,
        },

        /// An error occurred while writing to a temporary archive.
        #[error("error writing to temporary archive at `{archive_path}`")]
        TempArchiveWrite {
            /// The archive path for which there was an error.
            archive_path: Utf8PathBuf,

            /// The error that occurred.
            #[source]
            error: std::io::Error,
        },

        /// An error occurred while reading from a temporary archive.
        #[error("error reading from temporary archive at `{archive_path}`")]
        TempArchiveRead {
            /// The archive path for which there was an error.
            archive_path: Utf8PathBuf,

            /// The error that occurred.
            #[source]
            error: std::io::Error,
        },

        /// A checksum mismatch occurred. (Currently, the SHA-256 checksum is checked.)
        #[error("SHA-256 checksum mismatch: expected: {expected}, actual: {actual}")]
        ChecksumMismatch {
            /// The expected checksum.
            expected: String,

            /// The actual checksum.
            actual: String,
        },

        /// An error occurred while renaming a file.
        #[error("error renaming `{source}` to `{dest}`")]
        FsRename {
            /// The rename source.
            source: Utf8PathBuf,

            /// The rename destination.
            dest: Utf8PathBuf,

            /// The error that occurred.
            #[source]
            error: std::io::Error,
        },

        /// An error occurred while running `cargo nextest self setup`.
        #[error("cargo-nextest binary updated, but error running `cargo nextest self setup`")]
        SelfSetup(#[source] std::io::Error),
    }

    fn known_versions(versions: &[(Version, ReleaseStatus)]) -> String {
        use std::fmt::Write;

        // Take the first few versions here.
        const DISPLAY_COUNT: usize = 4;

        let display_versions: Vec<_> = versions
            .iter()
            .filter(|(v, status)| v.pre.is_empty() && *status == ReleaseStatus::Active)
            .map(|(v, _)| v.to_string())
            .take(DISPLAY_COUNT)
            .collect();
        let mut display_str = display_versions.join(", ");
        if versions.len() > display_versions.len() {
            write!(
                display_str,
                " and {} others",
                versions.len() - display_versions.len()
            )
            .unwrap();
        }

        display_str
    }

    #[cfg(feature = "self-update")]
    /// An error occurred while parsing an [`UpdateVersion`](crate::update::UpdateVersion).
    #[derive(Debug, Error)]
    pub enum UpdateVersionParseError {
        /// The version string is empty.
        #[error("version string is empty")]
        EmptyString,

        /// The input is not a valid version requirement.
        #[error("`{input}` is not a valid semver requirement\n\
                (hint: see https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html for the correct format)"
                )]
        InvalidVersionReq {
            /// The input that was provided.
            input: String,

            /// The error.
            #[source]
            error: semver::Error,
        },

        /// The version is not a valid semver.
        #[error("`{input}` is not a valid semver{}", extra_semver_output(.input))]
        InvalidVersion {
            /// The input that was provided.
            input: String,

            /// The error.
            #[source]
            error: semver::Error,
        },
    }

    fn extra_semver_output(input: &str) -> String {
        // If it is not a valid version but it is a valid version
        // requirement, add a note to the warning
        if input.parse::<VersionReq>().is_ok() {
            format!(
                "\n(if you want to specify a semver range, add an explicit qualifier, like ^{input})"
            )
        } else {
            "".to_owned()
        }
    }
}

#[cfg(feature = "self-update")]
pub use self_update_errors::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_error_chain() {
        let err1 = StringError::new("err1", None);

        insta::assert_snapshot!(format!("{}", DisplayErrorChain::new(&err1)), @"err1");

        let err2 = StringError::new("err2", Some(err1));
        let err3 = StringError::new("err3\nerr3 line 2", Some(err2));

        insta::assert_snapshot!(format!("{}", DisplayErrorChain::new(&err3)), @r"
        err3
        err3 line 2
          caused by:
          - err2
          - err1
        ");
    }

    #[test]
    fn display_error_list() {
        let err1 = StringError::new("err1", None);

        let error_list =
            ErrorList::<StringError>::new("waiting on the water to boil", vec![err1.clone()])
                .expect(">= 1 error");
        insta::assert_snapshot!(format!("{}", error_list), @"err1");
        insta::assert_snapshot!(format!("{}", DisplayErrorChain::new(&error_list)), @"err1");

        let err2 = StringError::new("err2", Some(err1));
        let err3 = StringError::new("err3", Some(err2));

        let error_list =
            ErrorList::<StringError>::new("waiting on flowers to bloom", vec![err3.clone()])
                .expect(">= 1 error");
        insta::assert_snapshot!(format!("{}", error_list), @"err3");
        insta::assert_snapshot!(format!("{}", DisplayErrorChain::new(&error_list)), @r"
        err3
          caused by:
          - err2
          - err1
        ");

        let err4 = StringError::new("err4", None);
        let err5 = StringError::new("err5", Some(err4));
        let err6 = StringError::new("err6\nerr6 line 2", Some(err5));

        let error_list = ErrorList::<StringError>::new(
            "waiting for the heat death of the universe",
            vec![err3, err6],
        )
        .expect(">= 1 error");

        insta::assert_snapshot!(format!("{}", error_list), @r"
        2 errors occurred waiting for the heat death of the universe:
        * err3
            caused by:
            - err2
            - err1
        * err6
          err6 line 2
            caused by:
            - err5
            - err4
        ");
        insta::assert_snapshot!(format!("{}", DisplayErrorChain::new(&error_list)), @r"
        2 errors occurred waiting for the heat death of the universe:
        * err3
            caused by:
            - err2
            - err1
        * err6
          err6 line 2
            caused by:
            - err5
            - err4
        ");
    }

    #[derive(Clone, Debug, Error)]
    struct StringError {
        message: String,
        #[source]
        source: Option<Box<StringError>>,
    }

    impl StringError {
        fn new(message: impl Into<String>, source: Option<StringError>) -> Self {
            Self {
                message: message.into(),
                source: source.map(Box::new),
            }
        }
    }

    impl fmt::Display for StringError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.message)
        }
    }
}
