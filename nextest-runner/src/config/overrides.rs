// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::{
    CompiledProfileScripts, DeserializedProfileScriptConfig, EvaluatableProfile, NextestConfig,
    NextestConfigImpl, TestPriority,
};
use crate::{
    config::{FinalConfig, PreBuildPlatform, RetryPolicy, SlowTimeout, TestGroup, ThreadsRequired},
    errors::{
        ConfigCompileError, ConfigCompileErrorKind, ConfigCompileSection, ConfigParseErrorKind,
    },
    platform::BuildPlatforms,
    reporter::TestOutputDisplay,
};
use guppy::graph::cargo::BuildPlatform;
use nextest_filtering::{CompiledExpr, Filterset, FiltersetKind, ParseContext, TestQuery};
use owo_colors::{OwoColorize, Style};
use serde::{Deserialize, Deserializer};
use smol_str::SmolStr;
use std::{collections::HashMap, time::Duration};
use target_spec::{Platform, TargetSpec};

/// Settings for individual tests.
///
/// Returned by [`EvaluatableProfile::settings_for`].
///
/// The `Source` parameter tracks an optional source; this isn't used by any public APIs at the
/// moment.
#[derive(Clone, Debug)]
pub struct TestSettings<'p, Source = ()> {
    priority: (TestPriority, Source),
    threads_required: (ThreadsRequired, Source),
    run_extra_args: (&'p [String], Source),
    retries: (RetryPolicy, Source),
    slow_timeout: (SlowTimeout, Source),
    leak_timeout: (Duration, Source),
    test_group: (TestGroup, Source),
    success_output: (TestOutputDisplay, Source),
    failure_output: (TestOutputDisplay, Source),
    junit_store_success_output: (bool, Source),
    junit_store_failure_output: (bool, Source),
}

pub(crate) trait TrackSource<'p>: Sized {
    fn track_default<T>(value: T) -> (T, Self);
    fn track_profile<T>(value: T) -> (T, Self);
    fn track_override<T>(value: T, source: &'p CompiledOverride<FinalConfig>) -> (T, Self);
}

impl<'p> TrackSource<'p> for () {
    fn track_default<T>(value: T) -> (T, Self) {
        (value, ())
    }

    fn track_profile<T>(value: T) -> (T, Self) {
        (value, ())
    }

    fn track_override<T>(value: T, _source: &'p CompiledOverride<FinalConfig>) -> (T, Self) {
        (value, ())
    }
}

#[derive(Copy, Clone, Debug)]
pub(crate) enum SettingSource<'p> {
    /// A default configuration not specified in, or possible to override from,
    /// a profile.
    Default,

    /// A configuration specified in a profile.
    Profile,

    /// An override specified in a profile.
    Override(&'p CompiledOverride<FinalConfig>),
}

impl<'p> TrackSource<'p> for SettingSource<'p> {
    fn track_default<T>(value: T) -> (T, Self) {
        (value, SettingSource::Default)
    }

    fn track_profile<T>(value: T) -> (T, Self) {
        (value, SettingSource::Profile)
    }

    fn track_override<T>(value: T, source: &'p CompiledOverride<FinalConfig>) -> (T, Self) {
        (value, SettingSource::Override(source))
    }
}

impl<'p> TestSettings<'p> {
    /// Returns the test's priority.
    pub fn priority(&self) -> TestPriority {
        self.priority.0
    }

    /// Returns the number of threads required for this test.
    pub fn threads_required(&self) -> ThreadsRequired {
        self.threads_required.0
    }

    /// Returns extra arguments to pass at runtime for this test.
    pub fn run_extra_args(&self) -> &'p [String] {
        self.run_extra_args.0
    }

    /// Returns the number of retries for this test.
    pub fn retries(&self) -> RetryPolicy {
        self.retries.0
    }

    /// Returns the slow timeout for this test.
    pub fn slow_timeout(&self) -> SlowTimeout {
        self.slow_timeout.0
    }

    /// Returns the leak timeout for this test.
    pub fn leak_timeout(&self) -> Duration {
        self.leak_timeout.0
    }

    /// Returns the test group for this test.
    pub fn test_group(&self) -> &TestGroup {
        &self.test_group.0
    }

    /// Returns the success output setting for this test.
    pub fn success_output(&self) -> TestOutputDisplay {
        self.success_output.0
    }

    /// Returns the failure output setting for this test.
    pub fn failure_output(&self) -> TestOutputDisplay {
        self.failure_output.0
    }

    /// Returns whether success output should be stored in JUnit.
    pub fn junit_store_success_output(&self) -> bool {
        self.junit_store_success_output.0
    }

    /// Returns whether failure output should be stored in JUnit.
    pub fn junit_store_failure_output(&self) -> bool {
        self.junit_store_failure_output.0
    }
}

#[expect(dead_code)]
impl<'p, Source: Copy> TestSettings<'p, Source> {
    pub(super) fn new(profile: &'p EvaluatableProfile<'_>, query: &TestQuery<'_>) -> Self
    where
        Source: TrackSource<'p>,
    {
        let ecx = profile.filterset_ecx();

        let mut priority = None;
        let mut threads_required = None;
        let mut run_extra_args = None;
        let mut retries = None;
        let mut slow_timeout = None;
        let mut leak_timeout = None;
        let mut test_group = None;
        let mut success_output = None;
        let mut failure_output = None;
        let mut junit_store_success_output = None;
        let mut junit_store_failure_output = None;

        for override_ in &profile.compiled_data.overrides {
            if !override_.state.host_eval {
                continue;
            }
            if query.binary_query.platform == BuildPlatform::Host && !override_.state.host_test_eval
            {
                continue;
            }
            if query.binary_query.platform == BuildPlatform::Target && !override_.state.target_eval
            {
                continue;
            }

            if let Some(expr) = &override_.filter() {
                if !expr.matches_test(query, &ecx) {
                    continue;
                }
                // If no expression is present, it's equivalent to "all()".
            }

            if priority.is_none() {
                if let Some(p) = override_.data.priority {
                    priority = Some(Source::track_override(p, override_));
                }
            }
            if threads_required.is_none() {
                if let Some(t) = override_.data.threads_required {
                    threads_required = Some(Source::track_override(t, override_));
                }
            }
            if run_extra_args.is_none() {
                if let Some(r) = override_.data.run_extra_args.as_deref() {
                    run_extra_args = Some(Source::track_override(r, override_));
                }
            }
            if retries.is_none() {
                if let Some(r) = override_.data.retries {
                    retries = Some(Source::track_override(r, override_));
                }
            }
            if slow_timeout.is_none() {
                if let Some(s) = override_.data.slow_timeout {
                    slow_timeout = Some(Source::track_override(s, override_));
                }
            }
            if leak_timeout.is_none() {
                if let Some(l) = override_.data.leak_timeout {
                    leak_timeout = Some(Source::track_override(l, override_));
                }
            }
            if test_group.is_none() {
                if let Some(t) = &override_.data.test_group {
                    test_group = Some(Source::track_override(t.clone(), override_));
                }
            }
            if success_output.is_none() {
                if let Some(s) = override_.data.success_output {
                    success_output = Some(Source::track_override(s, override_));
                }
            }
            if failure_output.is_none() {
                if let Some(f) = override_.data.failure_output {
                    failure_output = Some(Source::track_override(f, override_));
                }
            }
            if junit_store_success_output.is_none() {
                if let Some(s) = override_.data.junit.store_success_output {
                    junit_store_success_output = Some(Source::track_override(s, override_));
                }
            }
            if junit_store_failure_output.is_none() {
                if let Some(f) = override_.data.junit.store_failure_output {
                    junit_store_failure_output = Some(Source::track_override(f, override_));
                }
            }
        }

        // If no overrides were found, use the profile defaults.
        let priority = priority.unwrap_or_else(|| Source::track_default(TestPriority::default()));
        let threads_required =
            threads_required.unwrap_or_else(|| Source::track_profile(profile.threads_required()));
        let run_extra_args =
            run_extra_args.unwrap_or_else(|| Source::track_profile(profile.run_extra_args()));
        let retries = retries.unwrap_or_else(|| Source::track_profile(profile.retries()));
        let slow_timeout =
            slow_timeout.unwrap_or_else(|| Source::track_profile(profile.slow_timeout()));
        let leak_timeout =
            leak_timeout.unwrap_or_else(|| Source::track_profile(profile.leak_timeout()));
        let test_group = test_group.unwrap_or_else(|| Source::track_profile(TestGroup::Global));
        let success_output =
            success_output.unwrap_or_else(|| Source::track_profile(profile.success_output()));
        let failure_output =
            failure_output.unwrap_or_else(|| Source::track_profile(profile.failure_output()));
        let junit_store_success_output = junit_store_success_output.unwrap_or_else(|| {
            // If the profile doesn't have JUnit enabled, success output can just be false.
            Source::track_profile(profile.junit().is_some_and(|j| j.store_success_output()))
        });
        let junit_store_failure_output = junit_store_failure_output.unwrap_or_else(|| {
            // If the profile doesn't have JUnit enabled, failure output can just be false.
            Source::track_profile(profile.junit().is_some_and(|j| j.store_failure_output()))
        });

        TestSettings {
            threads_required,
            run_extra_args,
            retries,
            priority,
            slow_timeout,
            leak_timeout,
            test_group,
            success_output,
            failure_output,
            junit_store_success_output,
            junit_store_failure_output,
        }
    }

    /// Returns the number of threads required for this test, with the source attached.
    pub(crate) fn threads_required_with_source(&self) -> (ThreadsRequired, Source) {
        self.threads_required
    }

    /// Returns the number of retries for this test, with the source attached.
    pub(crate) fn retries_with_source(&self) -> (RetryPolicy, Source) {
        self.retries
    }

    /// Returns the slow timeout for this test, with the source attached.
    pub(crate) fn slow_timeout_with_source(&self) -> (SlowTimeout, Source) {
        self.slow_timeout
    }

    /// Returns the leak timeout for this test, with the source attached.
    pub(crate) fn leak_timeout_with_source(&self) -> (Duration, Source) {
        self.leak_timeout
    }

    /// Returns the test group for this test, with the source attached.
    pub(crate) fn test_group_with_source(&self) -> &(TestGroup, Source) {
        &self.test_group
    }
}

#[derive(Clone, Debug)]
pub(super) struct CompiledByProfile {
    pub(super) default: CompiledData<PreBuildPlatform>,
    pub(super) other: HashMap<String, CompiledData<PreBuildPlatform>>,
}

impl CompiledByProfile {
    pub(super) fn new(
        pcx: &ParseContext<'_>,
        config: &NextestConfigImpl,
    ) -> Result<Self, ConfigParseErrorKind> {
        let mut errors = vec![];
        let default = CompiledData::new(
            pcx,
            "default",
            Some(config.default_profile().default_filter()),
            config.default_profile().overrides(),
            config.default_profile().setup_scripts(),
            &mut errors,
        );
        let other: HashMap<_, _> = config
            .other_profiles()
            .map(|(profile_name, profile)| {
                (
                    profile_name.to_owned(),
                    CompiledData::new(
                        pcx,
                        profile_name,
                        profile.default_filter(),
                        profile.overrides(),
                        profile.scripts(),
                        &mut errors,
                    ),
                )
            })
            .collect();

        if errors.is_empty() {
            Ok(Self { default, other })
        } else {
            Err(ConfigParseErrorKind::CompileErrors(errors))
        }
    }

    /// Returns the compiled data for the default config.
    ///
    /// The default config does not depend on the package graph, so we create it separately here.
    /// But we don't implement `Default` to make sure that the value is for the default _config_,
    /// not the default _profile_ (which repo config can customize).
    pub(super) fn for_default_config() -> Self {
        Self {
            default: CompiledData {
                profile_default_filter: Some(CompiledDefaultFilter::for_default_config()),
                overrides: vec![],
                scripts: vec![],
            },
            other: HashMap::new(),
        }
    }
}

/// A compiled form of the default filter for a profile.
///
/// Returned by [`EvaluatableProfile::default_filter`].
#[derive(Clone, Debug)]
pub struct CompiledDefaultFilter {
    /// The compiled expression.
    ///
    /// This is a bit tricky -- in some cases, the default config is constructed without a
    /// `PackageGraph` being available. But parsing filtersets requires a `PackageGraph`. So we hack
    /// around it by only storing the compiled expression here, and by setting it to `all()` (which
    /// matches the config).
    ///
    /// This does make the default-filter defined in default-config.toml a bit
    /// of a lie (since we don't use it directly, but instead replicate it in
    /// code). But it's not too bad.
    pub expr: CompiledExpr,

    /// The profile name the default filter originates from.
    pub profile: String,

    /// The section of the config that the default filter comes from.
    pub section: CompiledDefaultFilterSection,
}

impl CompiledDefaultFilter {
    pub(crate) fn for_default_config() -> Self {
        Self {
            expr: CompiledExpr::ALL,
            profile: NextestConfig::DEFAULT_PROFILE.to_owned(),
            section: CompiledDefaultFilterSection::Profile,
        }
    }

    /// Displays a configuration string for the default filter.
    pub fn display_config(&self, bold_style: Style) -> String {
        match &self.section {
            CompiledDefaultFilterSection::Profile => {
                format!("profile.{}.default-filter", self.profile)
                    .style(bold_style)
                    .to_string()
            }
            CompiledDefaultFilterSection::Override(_) => {
                format!(
                    "default-filter in {}",
                    format!("profile.{}.overrides", self.profile).style(bold_style)
                )
            }
        }
    }
}

/// Within [`CompiledDefaultFilter`], the part of the config that the default
/// filter comes from.
#[derive(Clone, Copy, Debug)]
pub enum CompiledDefaultFilterSection {
    /// The config comes from the top-level `profile.<profile-name>.default-filter`.
    Profile,

    /// The config comes from the override at the given index.
    Override(usize),
}

#[derive(Clone, Debug)]
pub(super) struct CompiledData<State> {
    // The default filter specified at the profile level.
    //
    // Overrides might also specify their own filters, and in that case the
    // overrides take priority.
    pub(super) profile_default_filter: Option<CompiledDefaultFilter>,
    pub(super) overrides: Vec<CompiledOverride<State>>,
    pub(super) scripts: Vec<CompiledProfileScripts<State>>,
}

impl CompiledData<PreBuildPlatform> {
    fn new(
        pcx: &ParseContext<'_>,
        profile_name: &str,
        profile_default_filter: Option<&str>,
        overrides: &[DeserializedOverride],
        scripts: &[DeserializedProfileScriptConfig],
        errors: &mut Vec<ConfigCompileError>,
    ) -> Self {
        let profile_default_filter =
            profile_default_filter.and_then(|filter| {
                match Filterset::parse(filter.to_owned(), pcx, FiltersetKind::DefaultFilter) {
                    Ok(expr) => Some(CompiledDefaultFilter {
                        expr: expr.compiled,
                        profile: profile_name.to_owned(),
                        section: CompiledDefaultFilterSection::Profile,
                    }),
                    Err(err) => {
                        errors.push(ConfigCompileError {
                            profile_name: profile_name.to_owned(),
                            section: ConfigCompileSection::DefaultFilter,
                            kind: ConfigCompileErrorKind::Parse {
                                host_parse_error: None,
                                target_parse_error: None,
                                filter_parse_errors: vec![err],
                            },
                        });
                        None
                    }
                }
            });

        let overrides = overrides
            .iter()
            .enumerate()
            .filter_map(|(index, source)| {
                CompiledOverride::new(pcx, profile_name, index, source, errors)
            })
            .collect();
        let scripts = scripts
            .iter()
            .enumerate()
            .filter_map(|(index, source)| {
                CompiledProfileScripts::new(pcx, profile_name, index, source, errors)
            })
            .collect();
        Self {
            profile_default_filter,
            overrides,
            scripts,
        }
    }

    pub(super) fn extend_reverse(&mut self, other: Self) {
        // For the default filter, other wins (it is last, and after reversing, it will be first).
        if other.profile_default_filter.is_some() {
            self.profile_default_filter = other.profile_default_filter;
        }
        self.overrides.extend(other.overrides.into_iter().rev());
        self.scripts.extend(other.scripts.into_iter().rev());
    }

    pub(super) fn reverse(&mut self) {
        self.overrides.reverse();
        self.scripts.reverse();
    }

    /// Chains this data with another set of data, treating `other` as lower-priority than `self`.
    pub(super) fn chain(self, other: Self) -> Self {
        let profile_default_filter = self.profile_default_filter.or(other.profile_default_filter);
        let mut overrides = self.overrides;
        let mut setup_scripts = self.scripts;
        overrides.extend(other.overrides);
        setup_scripts.extend(other.scripts);
        Self {
            profile_default_filter,
            overrides,
            scripts: setup_scripts,
        }
    }

    pub(super) fn apply_build_platforms(
        self,
        build_platforms: &BuildPlatforms,
    ) -> CompiledData<FinalConfig> {
        let profile_default_filter = self.profile_default_filter;
        let overrides = self
            .overrides
            .into_iter()
            .map(|override_| override_.apply_build_platforms(build_platforms))
            .collect();
        let setup_scripts = self
            .scripts
            .into_iter()
            .map(|setup_script| setup_script.apply_build_platforms(build_platforms))
            .collect();
        CompiledData {
            profile_default_filter,
            overrides,
            scripts: setup_scripts,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CompiledOverride<State> {
    id: OverrideId,
    state: State,
    pub(super) data: ProfileOverrideData,
}

impl<State> CompiledOverride<State> {
    pub(crate) fn id(&self) -> &OverrideId {
        &self.id
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct OverrideId {
    pub(crate) profile_name: SmolStr,
    index: usize,
}

#[derive(Clone, Debug)]
pub(super) struct ProfileOverrideData {
    host_spec: MaybeTargetSpec,
    target_spec: MaybeTargetSpec,
    filter: Option<FilterOrDefaultFilter>,
    priority: Option<TestPriority>,
    threads_required: Option<ThreadsRequired>,
    run_extra_args: Option<Vec<String>>,
    retries: Option<RetryPolicy>,
    slow_timeout: Option<SlowTimeout>,
    leak_timeout: Option<Duration>,
    pub(super) test_group: Option<TestGroup>,
    success_output: Option<TestOutputDisplay>,
    failure_output: Option<TestOutputDisplay>,
    junit: DeserializedJunitOutput,
}

impl CompiledOverride<PreBuildPlatform> {
    fn new(
        pcx: &ParseContext<'_>,
        profile_name: &str,
        index: usize,
        source: &DeserializedOverride,
        errors: &mut Vec<ConfigCompileError>,
    ) -> Option<Self> {
        if source.platform.host.is_none()
            && source.platform.target.is_none()
            && source.filter.is_none()
        {
            errors.push(ConfigCompileError {
                profile_name: profile_name.to_owned(),
                section: ConfigCompileSection::Override(index),
                kind: ConfigCompileErrorKind::ConstraintsNotSpecified {
                    default_filter_specified: source.default_filter.is_some(),
                },
            });
            return None;
        }

        let host_spec = MaybeTargetSpec::new(source.platform.host.as_deref());
        let target_spec = MaybeTargetSpec::new(source.platform.target.as_deref());
        let filter = source.filter.as_ref().map_or(Ok(None), |filter| {
            Some(Filterset::parse(filter.clone(), pcx, FiltersetKind::Test)).transpose()
        });
        let default_filter = source.default_filter.as_ref().map_or(Ok(None), |filter| {
            Some(Filterset::parse(
                filter.clone(),
                pcx,
                FiltersetKind::DefaultFilter,
            ))
            .transpose()
        });

        match (host_spec, target_spec, filter, default_filter) {
            (Ok(host_spec), Ok(target_spec), Ok(filter), Ok(default_filter)) => {
                // At most one of filter and default-filter can be specified.
                let filter = match (filter, default_filter) {
                    (Some(_), Some(_)) => {
                        errors.push(ConfigCompileError {
                            profile_name: profile_name.to_owned(),
                            section: ConfigCompileSection::Override(index),
                            kind: ConfigCompileErrorKind::FilterAndDefaultFilterSpecified,
                        });
                        return None;
                    }
                    (Some(filter), None) => Some(FilterOrDefaultFilter::Filter(filter)),
                    (None, Some(default_filter)) => {
                        let compiled = CompiledDefaultFilter {
                            expr: default_filter.compiled,
                            profile: profile_name.to_owned(),
                            section: CompiledDefaultFilterSection::Override(index),
                        };
                        Some(FilterOrDefaultFilter::DefaultFilter(compiled))
                    }
                    (None, None) => None,
                };

                Some(Self {
                    id: OverrideId {
                        profile_name: profile_name.into(),
                        index,
                    },
                    state: PreBuildPlatform {},
                    data: ProfileOverrideData {
                        host_spec,
                        target_spec,
                        filter,
                        priority: source.priority,
                        threads_required: source.threads_required,
                        run_extra_args: source.run_extra_args.clone(),
                        retries: source.retries,
                        slow_timeout: source.slow_timeout,
                        leak_timeout: source.leak_timeout,
                        test_group: source.test_group.clone(),
                        success_output: source.success_output,
                        failure_output: source.failure_output,
                        junit: source.junit,
                    },
                })
            }
            (maybe_host_err, maybe_target_err, maybe_filter_err, maybe_default_filter_err) => {
                let host_parse_error = maybe_host_err.err();
                let target_parse_error = maybe_target_err.err();
                let filter_parse_errors = maybe_filter_err
                    .err()
                    .into_iter()
                    .chain(maybe_default_filter_err.err())
                    .collect();

                errors.push(ConfigCompileError {
                    profile_name: profile_name.to_owned(),
                    section: ConfigCompileSection::Override(index),
                    kind: ConfigCompileErrorKind::Parse {
                        host_parse_error,
                        target_parse_error,
                        filter_parse_errors,
                    },
                });
                None
            }
        }
    }

    pub(super) fn apply_build_platforms(
        self,
        build_platforms: &BuildPlatforms,
    ) -> CompiledOverride<FinalConfig> {
        let host_eval = self.data.host_spec.eval(&build_platforms.host.platform);
        let host_test_eval = self.data.target_spec.eval(&build_platforms.host.platform);
        let target_eval = build_platforms
            .target
            .as_ref()
            .map_or(host_test_eval, |target| {
                self.data.target_spec.eval(&target.triple.platform)
            });

        CompiledOverride {
            id: self.id,
            state: FinalConfig {
                host_eval,
                host_test_eval,
                target_eval,
            },
            data: self.data,
        }
    }
}

impl CompiledOverride<FinalConfig> {
    /// Returns the target spec.
    pub(crate) fn target_spec(&self) -> &MaybeTargetSpec {
        &self.data.target_spec
    }

    /// Returns the filter to apply to overrides, if any.
    pub(crate) fn filter(&self) -> Option<&Filterset> {
        match self.data.filter.as_ref() {
            Some(FilterOrDefaultFilter::Filter(filter)) => Some(filter),
            _ => None,
        }
    }

    /// Returns the default filter if it matches the platform.
    pub(crate) fn default_filter_if_matches_platform(&self) -> Option<&CompiledDefaultFilter> {
        match self.data.filter.as_ref() {
            Some(FilterOrDefaultFilter::DefaultFilter(filter)) => {
                // Which kind of evaluation to assume: matching the *target*
                // filter against the *target* platform (host_eval +
                // target_eval), or matching the *target* filter against the
                // *host* platform (host_eval + host_test_eval)? The former
                // makes much more sense, since in a cross-compile scenario you
                // want to match a (host, target) pair.
                (self.state.host_eval && self.state.target_eval).then_some(filter)
            }
            _ => None,
        }
    }
}

/// Represents a [`TargetSpec`] that might have been provided.
#[derive(Clone, Debug, Default)]
pub(crate) enum MaybeTargetSpec {
    Provided(TargetSpec),
    #[default]
    Any,
}

impl MaybeTargetSpec {
    pub(super) fn new(platform_str: Option<&str>) -> Result<Self, target_spec::Error> {
        Ok(match platform_str {
            Some(platform_str) => {
                MaybeTargetSpec::Provided(TargetSpec::new(platform_str.to_owned())?)
            }
            None => MaybeTargetSpec::Any,
        })
    }

    pub(super) fn eval(&self, platform: &Platform) -> bool {
        match self {
            MaybeTargetSpec::Provided(spec) => spec
                .eval(platform)
                .unwrap_or(/* unknown results are mapped to true */ true),
            MaybeTargetSpec::Any => true,
        }
    }
}

/// Either a filter override or a default filter specified for a platform.
///
/// At most one of these can be specified.
#[derive(Clone, Debug)]
pub(crate) enum FilterOrDefaultFilter {
    Filter(Filterset),
    DefaultFilter(CompiledDefaultFilter),
}

/// Deserialized form of profile overrides before compilation.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) struct DeserializedOverride {
    /// The host and/or target platforms to match against.
    #[serde(default)]
    platform: PlatformStrings,
    /// The filterset to match against.
    #[serde(default)]
    filter: Option<String>,
    /// Overrides. (This used to use serde(flatten) but that has issues:
    /// https://github.com/serde-rs/serde/issues/2312.)
    #[serde(default)]
    priority: Option<TestPriority>,
    #[serde(default)]
    default_filter: Option<String>,
    #[serde(default)]
    threads_required: Option<ThreadsRequired>,
    #[serde(default)]
    run_extra_args: Option<Vec<String>>,
    #[serde(default, deserialize_with = "super::deserialize_retry_policy")]
    retries: Option<RetryPolicy>,
    #[serde(default, deserialize_with = "super::deserialize_slow_timeout")]
    slow_timeout: Option<SlowTimeout>,
    #[serde(default, with = "humantime_serde::option")]
    leak_timeout: Option<Duration>,
    #[serde(default)]
    test_group: Option<TestGroup>,
    #[serde(default)]
    success_output: Option<TestOutputDisplay>,
    #[serde(default)]
    failure_output: Option<TestOutputDisplay>,
    #[serde(default)]
    junit: DeserializedJunitOutput,
}

#[derive(Copy, Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) struct DeserializedJunitOutput {
    store_success_output: Option<bool>,
    store_failure_output: Option<bool>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct PlatformStrings {
    pub(super) host: Option<String>,
    pub(super) target: Option<String>,
}

impl<'de> Deserialize<'de> for PlatformStrings {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;

        impl<'de2> serde::de::Visitor<'de2> for V {
            type Value = PlatformStrings;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str(
                    "a table ({ host = \"x86_64-apple-darwin\", \
                        target = \"cfg(windows)\" }) \
                        or a string (\"x86_64-unknown-gnu-linux\")",
                )
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(PlatformStrings {
                    host: None,
                    target: Some(v.to_owned()),
                })
            }

            fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de2>,
            {
                #[derive(Deserialize)]
                struct PlatformStringsInner {
                    #[serde(default)]
                    host: Option<String>,
                    #[serde(default)]
                    target: Option<String>,
                }

                let inner = PlatformStringsInner::deserialize(
                    serde::de::value::MapAccessDeserializer::new(map),
                )?;
                Ok(PlatformStrings {
                    host: inner.host,
                    target: inner.target,
                })
            }
        }

        deserializer.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{test_helpers::*, NextestConfig};
    use camino::Utf8Path;
    use camino_tempfile::tempdir;
    use indoc::indoc;
    use std::num::NonZeroUsize;
    use test_case::test_case;

    /// Basic test to ensure overrides work. Add new override parameters to this test.
    #[test]
    fn test_overrides_basic() {
        let config_contents = indoc! {r#"
            # Override 1
            [[profile.default.overrides]]
            platform = 'aarch64-apple-darwin'  # this is the target platform
            filter = "test(test)"
            retries = { backoff = "exponential", count = 20, delay = "1s", max-delay = "20s" }
            slow-timeout = { period = "120s", terminate-after = 1, grace-period = "0s" }
            success-output = "immediate-final"
            junit = { store-success-output = true }

            # Override 2
            [[profile.default.overrides]]
            filter = "test(test)"
            threads-required = 8
            retries = 3
            slow-timeout = "60s"
            leak-timeout = "300ms"
            test-group = "my-group"
            failure-output = "final"
            junit = { store-failure-output = false }

            # Override 3
            [[profile.default.overrides]]
            platform = { host = "cfg(unix)" }
            filter = "test(override3)"
            retries = 5

            # Override 4 -- host not matched
            [[profile.default.overrides]]
            platform = { host = 'aarch64-apple-darwin' }
            retries = 10

            # Override 5 -- no filter provided, just platform
            [[profile.default.overrides]]
            platform = { host = 'cfg(target_os = "linux")', target = 'aarch64-apple-darwin' }
            filter = "test(override5)"
            retries = 8

            [profile.default.junit]
            path = "my-path.xml"

            [test-groups.my-group]
            max-threads = 20
        "#};

        let workspace_dir = tempdir().unwrap();

        let graph = temp_workspace(workspace_dir.path(), config_contents);
        let package_id = graph.workspace().iter().next().unwrap().id();

        let pcx = ParseContext::new(&graph);

        let nextest_config_result = NextestConfig::from_sources(
            graph.workspace().root(),
            &pcx,
            None,
            &[][..],
            &Default::default(),
        )
        .expect("config is valid");
        let profile = nextest_config_result
            .profile("default")
            .expect("valid profile name")
            .into_evaluatable(&build_platforms());

        // This query matches override 2.
        let host_binary_query =
            binary_query(&graph, package_id, "lib", "my-binary", BuildPlatform::Host);
        let query = TestQuery {
            binary_query: host_binary_query.to_query(),
            test_name: "test",
        };
        let overrides = profile.settings_for(&query);

        assert_eq!(overrides.threads_required(), ThreadsRequired::Count(8));
        assert_eq!(overrides.retries(), RetryPolicy::new_without_delay(3));
        assert_eq!(
            overrides.slow_timeout(),
            SlowTimeout {
                period: Duration::from_secs(60),
                terminate_after: None,
                grace_period: Duration::from_secs(10),
            }
        );
        assert_eq!(overrides.leak_timeout(), Duration::from_millis(300));
        assert_eq!(overrides.test_group(), &test_group("my-group"));
        assert_eq!(overrides.success_output(), TestOutputDisplay::Never);
        assert_eq!(overrides.failure_output(), TestOutputDisplay::Final);
        // For clarity.
        #[expect(clippy::bool_assert_comparison)]
        {
            assert_eq!(overrides.junit_store_success_output(), false);
            assert_eq!(overrides.junit_store_failure_output(), false);
        }

        // This query matches override 1 and 2.
        let target_binary_query = binary_query(
            &graph,
            package_id,
            "lib",
            "my-binary",
            BuildPlatform::Target,
        );
        let query = TestQuery {
            binary_query: target_binary_query.to_query(),
            test_name: "test",
        };
        let overrides = profile.settings_for(&query);

        assert_eq!(overrides.threads_required(), ThreadsRequired::Count(8));
        assert_eq!(
            overrides.retries(),
            RetryPolicy::Exponential {
                count: 20,
                delay: Duration::from_secs(1),
                jitter: false,
                max_delay: Some(Duration::from_secs(20)),
            }
        );
        assert_eq!(
            overrides.slow_timeout(),
            SlowTimeout {
                period: Duration::from_secs(120),
                terminate_after: Some(NonZeroUsize::new(1).unwrap()),
                grace_period: Duration::ZERO,
            }
        );
        assert_eq!(overrides.leak_timeout(), Duration::from_millis(300));
        assert_eq!(overrides.test_group(), &test_group("my-group"));
        assert_eq!(
            overrides.success_output(),
            TestOutputDisplay::ImmediateFinal
        );
        assert_eq!(overrides.failure_output(), TestOutputDisplay::Final);
        // For clarity.
        #[expect(clippy::bool_assert_comparison)]
        {
            assert_eq!(overrides.junit_store_success_output(), true);
            assert_eq!(overrides.junit_store_failure_output(), false);
        }

        // This query matches override 3.
        let query = TestQuery {
            binary_query: target_binary_query.to_query(),
            test_name: "override3",
        };
        let overrides = profile.settings_for(&query);
        assert_eq!(overrides.retries(), RetryPolicy::new_without_delay(5));

        // This query matches override 5.
        let query = TestQuery {
            binary_query: target_binary_query.to_query(),
            test_name: "override5",
        };
        let overrides = profile.settings_for(&query);
        assert_eq!(overrides.retries(), RetryPolicy::new_without_delay(8));

        // This query does not match any overrides.
        let query = TestQuery {
            binary_query: target_binary_query.to_query(),
            test_name: "no_match",
        };
        let overrides = profile.settings_for(&query);
        assert_eq!(overrides.retries(), RetryPolicy::new_without_delay(0));
    }

    #[test_case(
        indoc! {r#"
            [[profile.default.overrides]]
            retries = 2
        "#},
        "default",
        &[MietteJsonReport {
            message: "at least one of `platform` and `filter` must be specified".to_owned(),
            labels: vec![],
        }]

        ; "neither platform nor filter specified"
    )]
    #[test_case(
        indoc! {r#"
            [[profile.default.overrides]]
            default-filter = "test(test1)"
            retries = 2
        "#},
        "default",
        &[MietteJsonReport {
            message: "for override with `default-filter`, `platform` must also be specified".to_owned(),
            labels: vec![],
        }]

        ; "default-filter without platform"
    )]
    #[test_case(
        indoc! {r#"
            [[profile.default.overrides]]
            platform = 'cfg(unix)'
            default-filter = "not default()"
            retries = 2
        "#},
        "default",
        &[MietteJsonReport {
            message: "predicate not allowed in `default-filter` expressions".to_owned(),
            labels: vec![
                MietteJsonLabel {
                    label: "this predicate causes infinite recursion".to_owned(),
                    span: MietteJsonSpan { offset: 4, length: 9 },
                },
            ],
        }]

        ; "default filterset in default-filter"
    )]
    #[test_case(
        indoc! {r#"
            [[profile.default.overrides]]
            filter = 'test(test1)'
            default-filter = "test(test2)"
            retries = 2
        "#},
        "default",
        &[MietteJsonReport {
            message: "at most one of `filter` and `default-filter` must be specified".to_owned(),
            labels: vec![],
        }]

        ; "both filter and default-filter specified"
    )]
    #[test_case(
        indoc! {r#"
            [[profile.default.overrides]]
            filter = 'test(test1)'
            platform = 'cfg(unix)'
            default-filter = "test(test2)"
            retries = 2
        "#},
        "default",
        &[MietteJsonReport {
            message: "at most one of `filter` and `default-filter` must be specified".to_owned(),
            labels: vec![],
        }]

        ; "both filter and default-filter specified with platform"
    )]
    #[test_case(
        indoc! {r#"
            [[profile.default.overrides]]
            platform = {}
            retries = 2
        "#},
        "default",
        &[MietteJsonReport {
            message: "at least one of `platform` and `filter` must be specified".to_owned(),
            labels: vec![],
        }]

        ; "empty platform map"
    )]
    #[test_case(
        indoc! {r#"
            [[profile.ci.overrides]]
            platform = 'cfg(target_os = "macos)'
            retries = 2
        "#},
        "ci",
        &[MietteJsonReport {
            message: "error parsing cfg() expression".to_owned(),
            labels: vec![
                MietteJsonLabel { label: "unclosed quotes".to_owned(), span: MietteJsonSpan { offset: 16, length: 6 } }
            ]
        }]

        ; "invalid platform expression"
    )]
    #[test_case(
        indoc! {r#"
            [[profile.ci.overrides]]
            filter = 'test(/foo)'
            retries = 2
        "#},
        "ci",
        &[MietteJsonReport {
            message: "expected close regex".to_owned(),
            labels: vec![
                MietteJsonLabel { label: "missing `/`".to_owned(), span: MietteJsonSpan { offset: 9, length: 0 } }
            ]
        }]

        ; "invalid filterset"
    )]
    #[test_case(
        // Not strictly an override error, but convenient to put here.
        indoc! {r#"
            [profile.ci]
            default-filter = "test(foo) or default()"
        "#},
        "ci",
        &[MietteJsonReport {
            message: "predicate not allowed in `default-filter` expressions".to_owned(),
            labels: vec![
                MietteJsonLabel { label: "this predicate causes infinite recursion".to_owned(), span: MietteJsonSpan { offset: 13, length: 9 } }
            ]
        }]

        ; "default-filter with default"
    )]
    fn parse_overrides_invalid(
        config_contents: &str,
        faulty_profile: &str,
        expected_reports: &[MietteJsonReport],
    ) {
        let workspace_dir = tempdir().unwrap();
        let workspace_path: &Utf8Path = workspace_dir.path();

        let graph = temp_workspace(workspace_path, config_contents);
        let pcx = ParseContext::new(&graph);

        let err = NextestConfig::from_sources(
            graph.workspace().root(),
            &pcx,
            None,
            [],
            &Default::default(),
        )
        .expect_err("config is invalid");
        match err.kind() {
            ConfigParseErrorKind::CompileErrors(compile_errors) => {
                assert_eq!(
                    compile_errors.len(),
                    1,
                    "exactly one override error must be produced"
                );
                let error = compile_errors.first().unwrap();
                assert_eq!(
                    error.profile_name, faulty_profile,
                    "compile error profile matches"
                );
                let handler = miette::JSONReportHandler::new();
                let reports = error
                    .kind
                    .reports()
                    .map(|report| {
                        let mut out = String::new();
                        handler.render_report(&mut out, report.as_ref()).unwrap();

                        let json_report: MietteJsonReport = serde_json::from_str(&out)
                            .unwrap_or_else(|err| {
                                panic!(
                                    "failed to deserialize JSON message produced by miette: {err}"
                                )
                            });
                        json_report
                    })
                    .collect::<Vec<_>>();
                assert_eq!(&reports, expected_reports, "reports match");
            }
            other => {
                panic!("for config error {other:?}, expected ConfigParseErrorKind::FiltersetOrCfgParseError");
            }
        };
    }

    /// Test that `cfg(unix)` works with a custom platform.
    ///
    /// This was broken with older versions of target-spec.
    #[test]
    fn cfg_unix_with_custom_platform() {
        let config_contents = indoc! {r#"
            [[profile.default.overrides]]
            platform = { host = "cfg(unix)" }
            filter = "test(test)"
            retries = 5
        "#};

        let workspace_dir = tempdir().unwrap();

        let graph = temp_workspace(workspace_dir.path(), config_contents);
        let package_id = graph.workspace().iter().next().unwrap().id();
        let pcx = ParseContext::new(&graph);

        let nextest_config = NextestConfig::from_sources(
            graph.workspace().root(),
            &pcx,
            None,
            &[][..],
            &Default::default(),
        )
        .expect("config is valid");

        let build_platforms = custom_build_platforms(workspace_dir.path());

        let profile = nextest_config
            .profile("default")
            .expect("valid profile name")
            .into_evaluatable(&build_platforms);

        // Check that the override is correctly applied.
        let target_binary_query = binary_query(
            &graph,
            package_id,
            "lib",
            "my-binary",
            BuildPlatform::Target,
        );
        let query = TestQuery {
            binary_query: target_binary_query.to_query(),
            test_name: "test",
        };
        let overrides = profile.settings_for(&query);
        assert_eq!(
            overrides.retries(),
            RetryPolicy::new_without_delay(5),
            "retries applied to custom platform"
        );
    }
}
