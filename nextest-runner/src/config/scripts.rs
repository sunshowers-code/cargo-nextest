// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Setup scripts.

use super::{
    ConfigIdentifier, EvaluatableProfile, FinalConfig, MaybeTargetSpec, PlatformStrings,
    PreBuildPlatform, SlowTimeout,
};
use crate::{
    double_spawn::{DoubleSpawnContext, DoubleSpawnInfo},
    errors::{
        ChildStartError, ConfigCompileError, ConfigCompileErrorKind, ConfigCompileSection,
        InvalidConfigScriptName,
    },
    list::TestList,
    platform::BuildPlatforms,
    reporter::events::SetupScriptEnvMap,
    test_command::{apply_ld_dyld_env, create_command},
};
use camino_tempfile::Utf8TempPath;
use guppy::graph::cargo::BuildPlatform;
use indexmap::IndexMap;
use nextest_filtering::{EvalContext, Filterset, FiltersetKind, ParseContext, TestQuery};
use serde::{de::Error, Deserialize};
use smol_str::SmolStr;
use std::{
    collections::{HashMap, HashSet},
    fmt,
    process::Command,
    sync::Arc,
    time::Duration,
};

/// Data about setup scripts, returned by an [`EvaluatableProfile`].
pub struct SetupScripts<'profile> {
    enabled_scripts: IndexMap<&'profile ScriptId, SetupScript<'profile>>,
}

impl<'profile> SetupScripts<'profile> {
    pub(super) fn new(profile: &'profile EvaluatableProfile<'_>, test_list: &TestList<'_>) -> Self {
        Self::new_with_queries(
            profile,
            test_list
                .iter_tests()
                .filter(|test| test.test_info.filter_match.is_match())
                .map(|test| test.to_test_query()),
        )
    }

    // Creates a new `SetupScripts` instance for the given profile and matching tests.
    fn new_with_queries<'a>(
        profile: &'profile EvaluatableProfile<'_>,
        matching_tests: impl IntoIterator<Item = TestQuery<'a>>,
    ) -> Self {
        let script_config = profile.script_config();
        let profile_scripts = &profile.compiled_data.scripts;
        if profile_scripts.is_empty() {
            return Self {
                enabled_scripts: IndexMap::new(),
            };
        }

        // Build a map of setup scripts to the test configurations that enable them.
        let mut by_script_id = HashMap::new();
        for profile_script in profile_scripts {
            for script_id in &profile_script.setup {
                by_script_id
                    .entry(script_id)
                    .or_insert_with(Vec::new)
                    .push(profile_script);
            }
        }

        let env = profile.filterset_ecx();

        // This is a map from enabled setup scripts to a list of configurations that enabled them.
        let mut enabled_ids = HashSet::new();
        for test in matching_tests {
            // Look at all the setup scripts activated by this test.
            for (&script_id, compiled) in &by_script_id {
                if enabled_ids.contains(script_id) {
                    // This script is already enabled.
                    continue;
                }
                if compiled.iter().any(|data| data.is_enabled(&test, &env)) {
                    enabled_ids.insert(script_id);
                }
            }
        }

        // Build up a map of enabled scripts along with their data, by script ID.
        let mut enabled_scripts = IndexMap::new();
        for (script_id, config) in script_config {
            if enabled_ids.contains(script_id) {
                let compiled = by_script_id
                    .remove(script_id)
                    .expect("script id must be present");
                enabled_scripts.insert(
                    script_id,
                    SetupScript {
                        id: script_id.clone(),
                        config,
                        compiled,
                    },
                );
            }
        }

        Self { enabled_scripts }
    }

    /// Returns the number of enabled setup scripts.
    #[inline]
    pub fn len(&self) -> usize {
        self.enabled_scripts.len()
    }

    /// Returns true if there are no enabled setup scripts.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.enabled_scripts.is_empty()
    }

    /// Returns enabled setup scripts in the order they should be run in.
    #[inline]
    pub(crate) fn into_iter(self) -> impl Iterator<Item = SetupScript<'profile>> {
        self.enabled_scripts.into_values()
    }
}

/// Data about an individual setup script.
///
/// Returned by [`SetupScripts::iter`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub(crate) struct SetupScript<'profile> {
    /// The script ID.
    pub(crate) id: ScriptId,

    /// The configuration for the script.
    pub(crate) config: &'profile ScriptConfig,

    /// The compiled filters to use to check which tests this script is enabled for.
    pub(crate) compiled: Vec<&'profile CompiledProfileScripts<FinalConfig>>,
}

impl SetupScript<'_> {
    pub(crate) fn is_enabled(&self, test: &TestQuery<'_>, cx: &EvalContext<'_>) -> bool {
        self.compiled
            .iter()
            .any(|compiled| compiled.is_enabled(test, cx))
    }
}

/// Represents a to-be-run setup script command with a certain set of arguments.
pub(crate) struct SetupScriptCommand {
    /// The command to be run.
    command: std::process::Command,
    /// The environment file.
    env_path: Utf8TempPath,
    /// Double-spawn context.
    double_spawn: Option<DoubleSpawnContext>,
}

impl SetupScriptCommand {
    /// Creates a new `SetupScriptCommand` for a setup script.
    pub(crate) fn new(
        config: &ScriptConfig,
        profile_name: &str,
        double_spawn: &DoubleSpawnInfo,
        test_list: &TestList<'_>,
    ) -> Result<Self, ChildStartError> {
        let mut cmd = create_command(config.program().to_owned(), config.args(), double_spawn);

        // NB: we will always override user-provided environment variables with the
        // `CARGO_*` and `NEXTEST_*` variables set directly on `cmd` below.
        test_list.cargo_env().apply_env(&mut cmd);

        let env_path = camino_tempfile::Builder::new()
            .prefix("nextest-env")
            .tempfile()
            .map_err(|error| ChildStartError::TempPath(Arc::new(error)))?
            .into_temp_path();

        cmd.current_dir(test_list.workspace_root())
            // This environment variable is set to indicate that tests are being run under nextest.
            .env("NEXTEST", "1")
            // Set the nextest profile.
            .env("NEXTEST_PROFILE", profile_name)
            // Setup scripts can define environment variables which are written out here.
            .env("NEXTEST_ENV", &env_path);

        apply_ld_dyld_env(&mut cmd, test_list.updated_dylib_path());

        let double_spawn = double_spawn.spawn_context();

        Ok(Self {
            command: cmd,
            env_path,
            double_spawn,
        })
    }

    /// Returns the command to be run.
    #[inline]
    pub(crate) fn command_mut(&mut self) -> &mut std::process::Command {
        &mut self.command
    }

    pub(crate) fn spawn(self) -> std::io::Result<(tokio::process::Child, Utf8TempPath)> {
        let mut command = tokio::process::Command::from(self.command);
        let res = command.spawn();
        if let Some(ctx) = self.double_spawn {
            ctx.finish();
        }
        let child = res?;
        Ok((child, self.env_path))
    }
}

/// Data obtained by executing setup scripts. This is used to set up the environment for tests.
#[derive(Clone, Debug, Default)]
pub(crate) struct SetupScriptExecuteData<'profile> {
    env_maps: Vec<(SetupScript<'profile>, SetupScriptEnvMap)>,
}

impl<'profile> SetupScriptExecuteData<'profile> {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn add_script(&mut self, script: SetupScript<'profile>, env_map: SetupScriptEnvMap) {
        self.env_maps.push((script, env_map));
    }

    /// Applies the data from setup scripts to the given test instance.
    pub(crate) fn apply(&self, test: &TestQuery<'_>, cx: &EvalContext<'_>, command: &mut Command) {
        for (script, env_map) in &self.env_maps {
            if script.is_enabled(test, cx) {
                for (key, value) in env_map.env_map.iter() {
                    command.env(key, value);
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CompiledProfileScripts<State> {
    pub(super) setup: Vec<ScriptId>,
    pub(super) data: ProfileScriptData,
    state: State,
}

impl CompiledProfileScripts<PreBuildPlatform> {
    pub(super) fn new(
        pcx: &ParseContext<'_>,
        profile_name: &str,
        index: usize,
        source: &DeserializedProfileScriptConfig,
        errors: &mut Vec<ConfigCompileError>,
    ) -> Option<Self> {
        if source.platform.host.is_none()
            && source.platform.target.is_none()
            && source.filter.is_none()
        {
            errors.push(ConfigCompileError {
                profile_name: profile_name.to_owned(),
                section: ConfigCompileSection::Script(index),
                kind: ConfigCompileErrorKind::ConstraintsNotSpecified {
                    // The default filter is not relevant for scripts -- it is a
                    // configuration value, not a constraint.
                    default_filter_specified: false,
                },
            });
            return None;
        }

        let host_spec = MaybeTargetSpec::new(source.platform.host.as_deref());
        let target_spec = MaybeTargetSpec::new(source.platform.target.as_deref());

        let filter_expr = source.filter.as_ref().map_or(Ok(None), |filter| {
            // TODO: probably want to restrict the set of expressions here via
            // the `kind` parameter.
            Some(Filterset::parse(
                filter.clone(),
                pcx,
                FiltersetKind::DefaultFilter,
            ))
            .transpose()
        });

        match (host_spec, target_spec, filter_expr) {
            (Ok(host_spec), Ok(target_spec), Ok(expr)) => Some(Self {
                setup: source.setup.clone(),
                data: ProfileScriptData {
                    host_spec,
                    target_spec,
                    expr,
                },
                state: PreBuildPlatform {},
            }),
            (maybe_host_err, maybe_platform_err, maybe_parse_err) => {
                let host_platform_parse_error = maybe_host_err.err();
                let platform_parse_error = maybe_platform_err.err();
                let parse_errors = maybe_parse_err.err();

                errors.push(ConfigCompileError {
                    profile_name: profile_name.to_owned(),
                    section: ConfigCompileSection::Script(index),
                    kind: ConfigCompileErrorKind::Parse {
                        host_parse_error: host_platform_parse_error,
                        target_parse_error: platform_parse_error,
                        filter_parse_errors: parse_errors.into_iter().collect(),
                    },
                });
                None
            }
        }
    }

    pub(super) fn apply_build_platforms(
        self,
        build_platforms: &BuildPlatforms,
    ) -> CompiledProfileScripts<FinalConfig> {
        let host_eval = self.data.host_spec.eval(&build_platforms.host.platform);
        let host_test_eval = self.data.target_spec.eval(&build_platforms.host.platform);
        let target_eval = build_platforms
            .target
            .as_ref()
            .map_or(host_test_eval, |target| {
                self.data.target_spec.eval(&target.triple.platform)
            });

        CompiledProfileScripts {
            setup: self.setup,
            data: self.data,
            state: FinalConfig {
                host_eval,
                host_test_eval,
                target_eval,
            },
        }
    }
}

impl CompiledProfileScripts<FinalConfig> {
    pub(super) fn is_enabled(&self, query: &TestQuery<'_>, cx: &EvalContext<'_>) -> bool {
        if !self.state.host_eval {
            return false;
        }
        if query.binary_query.platform == BuildPlatform::Host && !self.state.host_test_eval {
            return false;
        }
        if query.binary_query.platform == BuildPlatform::Target && !self.state.target_eval {
            return false;
        }

        if let Some(expr) = &self.data.expr {
            expr.matches_test(query, cx)
        } else {
            true
        }
    }
}

/// The name of a configuration script.
#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct ScriptId(pub ConfigIdentifier);

impl ScriptId {
    /// Creates a new script identifier.
    pub fn new(identifier: SmolStr) -> Result<Self, InvalidConfigScriptName> {
        let identifier = ConfigIdentifier::new(identifier).map_err(InvalidConfigScriptName)?;
        Ok(Self(identifier))
    }

    /// Returns the name of the script as a [`ConfigIdentifier`].
    pub fn as_identifier(&self) -> &ConfigIdentifier {
        &self.0
    }

    #[cfg(test)]
    pub(super) fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl<'de> Deserialize<'de> for ScriptId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Try and deserialize as a string.
        let identifier = SmolStr::deserialize(deserializer)?;
        Self::new(identifier).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for ScriptId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Debug)]
pub(super) struct ProfileScriptData {
    host_spec: MaybeTargetSpec,
    target_spec: MaybeTargetSpec,
    expr: Option<Filterset>,
}

/// Deserialized form of profile-specific script configuration before compilation.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) struct DeserializedProfileScriptConfig {
    /// The host and/or target platforms to match against.
    #[serde(default)]
    pub(super) platform: PlatformStrings,

    /// The filterset to match against.
    #[serde(default)]
    filter: Option<String>,

    /// The setup script or scripts to run.
    #[serde(deserialize_with = "deserialize_script_ids")]
    setup: Vec<ScriptId>,
}

/// Deserialized form of script configuration before compilation.
///
/// This is defined as a top-level element.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ScriptConfig {
    /// The command to run. The first element is the program and the second element is a list
    /// of arguments.
    #[serde(deserialize_with = "deserialize_command")]
    pub command: (String, Vec<String>),

    /// An optional slow timeout for this command.
    #[serde(default, deserialize_with = "super::deserialize_slow_timeout")]
    pub slow_timeout: Option<SlowTimeout>,

    /// An optional leak timeout for this command.
    #[serde(default, with = "humantime_serde::option")]
    pub leak_timeout: Option<Duration>,

    /// Whether to capture standard output for this command.
    #[serde(default)]
    pub capture_stdout: bool,

    /// Whether to capture standard error for this command.
    #[serde(default)]
    pub capture_stderr: bool,

    /// JUnit configuration for this script.
    #[serde(default)]
    pub junit: ScriptJunitConfig,
}

impl ScriptConfig {
    /// Returns the name of the program.
    #[inline]
    pub fn program(&self) -> &str {
        &self.command.0
    }

    /// Returns the arguments to the command.
    #[inline]
    pub fn args(&self) -> &[String] {
        &self.command.1
    }

    /// Returns true if at least some output isn't being captured.
    #[inline]
    pub fn no_capture(&self) -> bool {
        !(self.capture_stdout && self.capture_stderr)
    }
}

/// A JUnit override configuration.
#[derive(Copy, Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ScriptJunitConfig {
    /// Whether to store successful output.
    ///
    /// Defaults to true.
    #[serde(default = "default_true")]
    pub store_success_output: bool,

    /// Whether to store failing output.
    ///
    /// Defaults to true.
    #[serde(default = "default_true")]
    pub store_failure_output: bool,
}

impl Default for ScriptJunitConfig {
    fn default() -> Self {
        Self {
            store_success_output: true,
            store_failure_output: true,
        }
    }
}

fn default_true() -> bool {
    true
}

fn deserialize_script_ids<'de, D>(deserializer: D) -> Result<Vec<ScriptId>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct ScriptIdVisitor;

    impl<'de> serde::de::Visitor<'de> for ScriptIdVisitor {
        type Value = Vec<ScriptId>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a script ID (string) or a list of script IDs")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(vec![ScriptId::new(value.into()).map_err(E::custom)?])
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut ids = Vec::new();
            while let Some(value) = seq.next_element::<String>()? {
                ids.push(ScriptId::new(value.into()).map_err(A::Error::custom)?);
            }
            Ok(ids)
        }
    }

    deserializer.deserialize_any(ScriptIdVisitor)
}

fn deserialize_command<'de, D>(deserializer: D) -> Result<(String, Vec<String>), D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct CommandVisitor;

    impl<'de> serde::de::Visitor<'de> for CommandVisitor {
        type Value = (String, Vec<String>);

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a Unix shell command or a list of arguments")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            let mut args = shell_words::split(value).map_err(E::custom)?;
            if args.is_empty() {
                return Err(E::invalid_value(serde::de::Unexpected::Str(value), &self));
            }
            let program = args.remove(0);
            Ok((program, args))
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let Some(program) = seq.next_element::<String>()? else {
                return Err(A::Error::invalid_length(0, &self));
            };
            let mut args = Vec::new();
            while let Some(value) = seq.next_element::<String>()? {
                args.push(value);
            }
            Ok((program, args))
        }
    }

    deserializer.deserialize_any(CommandVisitor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{test_helpers::*, ConfigExperimental, NextestConfig, ToolConfigFile},
        errors::{ConfigParseErrorKind, DisplayErrorChain, UnknownConfigScriptError},
    };
    use camino_tempfile::tempdir;
    use indoc::indoc;
    use maplit::btreeset;
    use test_case::test_case;

    #[test]
    fn test_scripts_basic() {
        let config_contents = indoc! {r#"
            [[profile.default.scripts]]
            platform = { host = "x86_64-unknown-linux-gnu" }
            filter = "test(script1)"
            setup = ["foo", "bar"]

            [[profile.default.scripts]]
            platform = { target = "aarch64-apple-darwin" }
            filter = "test(script2)"
            setup = "baz"

            [[profile.default.scripts]]
            filter = "test(script3)"
            # No matter which order scripts are specified here, they must always be run in the
            # order defined below.
            setup = ["baz", "foo", "@tool:my-tool:toolscript"]

            [script.foo]
            command = "command foo"

            [script.bar]
            command = ["cargo", "run", "-p", "bar"]
            slow-timeout = { period = "60s", terminate-after = 2 }

            [script.baz]
            command = "baz"
            slow-timeout = "1s"
            leak-timeout = "1s"
            capture-stdout = true
            capture-stderr = true
        "#
        };

        let tool_config_contents = indoc! {r#"
            [script.'@tool:my-tool:toolscript']
            command = "tool-command"
            "#
        };

        let workspace_dir = tempdir().unwrap();

        let graph = temp_workspace(workspace_dir.path(), config_contents);
        let package_id = graph.workspace().iter().next().unwrap().id();
        let tool_path = workspace_dir.path().join(".config/my-tool.toml");
        std::fs::write(&tool_path, tool_config_contents).unwrap();

        let pcx = ParseContext::new(&graph);

        let tool_config_files = [ToolConfigFile {
            tool: "my-tool".to_owned(),
            config_file: tool_path,
        }];

        // First, check that if the experimental feature isn't enabled, we get an error.
        let nextest_config_error = NextestConfig::from_sources(
            graph.workspace().root(),
            &pcx,
            None,
            &tool_config_files,
            &Default::default(),
        )
        .unwrap_err();
        match nextest_config_error.kind() {
            ConfigParseErrorKind::ExperimentalFeatureNotEnabled { feature } => {
                assert_eq!(*feature, ConfigExperimental::SetupScripts);
            }
            other => panic!("unexpected error kind: {other:?}"),
        }

        // Now, check with the experimental feature enabled.
        let nextest_config_result = NextestConfig::from_sources(
            graph.workspace().root(),
            &pcx,
            None,
            &tool_config_files,
            &btreeset! { ConfigExperimental::SetupScripts },
        )
        .expect("config is valid");
        let profile = nextest_config_result
            .profile("default")
            .expect("valid profile name")
            .into_evaluatable(&build_platforms());

        // This query matches the foo and bar scripts.
        let host_binary_query =
            binary_query(&graph, package_id, "lib", "my-binary", BuildPlatform::Host);
        let query = TestQuery {
            binary_query: host_binary_query.to_query(),
            test_name: "script1",
        };
        let scripts = SetupScripts::new_with_queries(&profile, std::iter::once(query));
        assert_eq!(scripts.len(), 2, "two scripts should be enabled");
        assert_eq!(
            scripts.enabled_scripts.get_index(0).unwrap().0.as_str(),
            "foo",
            "first script should be foo"
        );
        assert_eq!(
            scripts.enabled_scripts.get_index(1).unwrap().0.as_str(),
            "bar",
            "second script should be bar"
        );

        let target_binary_query = binary_query(
            &graph,
            package_id,
            "lib",
            "my-binary",
            BuildPlatform::Target,
        );

        // This query matches the baz script.
        let query = TestQuery {
            binary_query: target_binary_query.to_query(),
            test_name: "script2",
        };
        let scripts = SetupScripts::new_with_queries(&profile, std::iter::once(query));
        assert_eq!(scripts.len(), 1, "one script should be enabled");
        assert_eq!(
            scripts.enabled_scripts.get_index(0).unwrap().0.as_str(),
            "baz",
            "first script should be baz"
        );

        // This query matches the baz, foo and tool scripts (but note the order).
        let query = TestQuery {
            binary_query: target_binary_query.to_query(),
            test_name: "script3",
        };
        let scripts = SetupScripts::new_with_queries(&profile, std::iter::once(query));
        assert_eq!(scripts.len(), 3, "three scripts should be enabled");
        assert_eq!(
            scripts.enabled_scripts.get_index(0).unwrap().0.as_str(),
            "@tool:my-tool:toolscript",
            "first script should be toolscript"
        );
        assert_eq!(
            scripts.enabled_scripts.get_index(1).unwrap().0.as_str(),
            "foo",
            "second script should be foo"
        );
        assert_eq!(
            scripts.enabled_scripts.get_index(2).unwrap().0.as_str(),
            "baz",
            "third script should be baz"
        );
    }

    #[test_case(
        indoc! {r#"
            [script.foo]
            command = ""
        "#},
        "invalid value: string \"\", expected a Unix shell command or a list of arguments"

        ; "empty command"
    )]
    #[test_case(
        indoc! {r#"
            [script.foo]
            command = []
        "#},
        "invalid length 0, expected a Unix shell command or a list of arguments"

        ; "empty command list"
    )]
    #[test_case(
        indoc! {r#"
            [script.foo]
        "#},
        "script.foo: missing field `command`"

        ; "missing command"
    )]
    #[test_case(
        indoc! {r#"
            [script.foo]
            command = "my-command"
            slow-timeout = 34
        "#},
        r#"invalid type: integer `34`, expected a table ({ period = "60s", terminate-after = 2 }) or a string ("60s")"#

        ; "slow timeout is not a duration"
    )]
    #[test_case(
        indoc! {r#"
            [script.'@tool:foo']
            command = "my-command"
        "#},
        r#"invalid configuration script name: tool identifier not of the form "@tool:tool-name:identifier": `@tool:foo`"#

        ; "invalid tool script name"
    )]
    #[test_case(
        indoc! {r#"
            [script.'#foo']
            command = "my-command"
        "#},
        r"invalid configuration script name: invalid identifier `#foo`"

        ; "invalid script name"
    )]
    fn parse_scripts_invalid_deserialize(config_contents: &str, message: &str) {
        let workspace_dir = tempdir().unwrap();

        let graph = temp_workspace(workspace_dir.path(), config_contents);
        let pcx = ParseContext::new(&graph);

        let nextest_config_error = NextestConfig::from_sources(
            graph.workspace().root(),
            &pcx,
            None,
            &[][..],
            &btreeset! { ConfigExperimental::SetupScripts },
        )
        .expect_err("config is invalid");
        let actual_message = DisplayErrorChain::new(nextest_config_error).to_string();

        assert!(
            actual_message.contains(message),
            "nextest config error `{actual_message}` contains message `{message}`"
        );
    }

    #[test_case(
        indoc! {r#"
            [script.foo]
            command = "my-command"

            [[profile.default.scripts]]
            setup = ["foo"]
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
            [script.foo]
            command = "my-command"

            [[profile.default.scripts]]
            platform = {}
            setup = ["foo"]
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
            [script.foo]
            command = "my-command"

            [[profile.default.scripts]]
            platform = { host = 'cfg(target_os = "linux' }
            setup = ["foo"]
        "#},
        "default",
        &[MietteJsonReport {
            message: "error parsing cfg() expression".to_owned(),
            labels: vec![
                MietteJsonLabel { label: "expected one of `=`, `,`, `)` here".to_owned(), span: MietteJsonSpan { offset: 3, length: 1 } }
            ]
        }]

        ; "invalid platform expression"
    )]
    #[test_case(
        indoc! {r#"
            [script.foo]
            command = "my-command"

            [[profile.ci.overrides]]
            filter = 'test(/foo)'
            setup = ["foo"]
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
    fn parse_scripts_invalid_compile(
        config_contents: &str,
        faulty_profile: &str,
        expected_reports: &[MietteJsonReport],
    ) {
        let workspace_dir = tempdir().unwrap();

        let graph = temp_workspace(workspace_dir.path(), config_contents);

        let pcx = ParseContext::new(&graph);

        let error = NextestConfig::from_sources(
            graph.workspace().root(),
            &pcx,
            None,
            &[][..],
            &btreeset! { ConfigExperimental::SetupScripts },
        )
        .expect_err("config is invalid");
        match error.kind() {
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
                panic!("for config error {other:?}, expected ConfigParseErrorKind::CompiledDataParseError");
            }
        }
    }

    #[test_case(
        indoc! {r#"
            [script.'@tool:foo:bar']
            command = "my-command"

            [[profile.ci.overrides]]
            setup = ["@tool:foo:bar"]
        "#},
        &["@tool:foo:bar"]

        ; "tool config in main program")]
    fn parse_scripts_invalid_defined(config_contents: &str, expected_invalid_scripts: &[&str]) {
        let workspace_dir = tempdir().unwrap();

        let graph = temp_workspace(workspace_dir.path(), config_contents);

        let pcx = ParseContext::new(&graph);

        let error = NextestConfig::from_sources(
            graph.workspace().root(),
            &pcx,
            None,
            &[][..],
            &btreeset! { ConfigExperimental::SetupScripts },
        )
        .expect_err("config is invalid");
        match error.kind() {
            ConfigParseErrorKind::InvalidConfigScriptsDefined(scripts) => {
                assert_eq!(
                    scripts.len(),
                    expected_invalid_scripts.len(),
                    "correct number of scripts defined"
                );
                for (script, expected_script) in scripts.iter().zip(expected_invalid_scripts) {
                    assert_eq!(script.as_str(), *expected_script, "script name matches");
                }
            }
            other => {
                panic!("for config error {other:?}, expected ConfigParseErrorKind::InvalidConfigScriptsDefined");
            }
        }
    }

    #[test_case(
        indoc! {r#"
            [script.'blarg']
            command = "my-command"

            [[profile.ci.overrides]]
            setup = ["blarg"]
        "#},
        &["blarg"]

        ; "non-tool config in tool")]
    fn parse_scripts_invalid_defined_by_tool(
        tool_config_contents: &str,
        expected_invalid_scripts: &[&str],
    ) {
        let workspace_dir = tempdir().unwrap();

        let graph = temp_workspace(workspace_dir.path(), "");
        let tool_path = workspace_dir.path().join(".config/my-tool.toml");
        std::fs::write(&tool_path, tool_config_contents).unwrap();
        let tool_config_files = [ToolConfigFile {
            tool: "my-tool".to_owned(),
            config_file: tool_path,
        }];

        let pcx = ParseContext::new(&graph);

        let error = NextestConfig::from_sources(
            graph.workspace().root(),
            &pcx,
            None,
            &tool_config_files,
            &btreeset! { ConfigExperimental::SetupScripts },
        )
        .expect_err("config is invalid");
        match error.kind() {
            ConfigParseErrorKind::InvalidConfigScriptsDefinedByTool(scripts) => {
                assert_eq!(
                    scripts.len(),
                    expected_invalid_scripts.len(),
                    "exactly one script must be defined"
                );
                for (script, expected_script) in scripts.iter().zip(expected_invalid_scripts) {
                    assert_eq!(script.as_str(), *expected_script, "script name matches");
                }
            }
            other => {
                panic!("for config error {other:?}, expected ConfigParseErrorKind::InvalidConfigScriptsDefinedByTool");
            }
        }
    }

    #[test_case(
        indoc! {r#"
            [script.foo]
            command = "my-command"

            [[profile.default.scripts]]
            filter = "test(script1)"
            setup = "bar"

            [[profile.ci.scripts]]
            filter = "test(script2)"
            setup = ["baz"]
        "#},
        vec![
            UnknownConfigScriptError {
                profile_name: "default".to_owned(),
                name: ScriptId::new("bar".into()).unwrap(),
            },
            UnknownConfigScriptError {
                profile_name: "ci".to_owned(),
                name: ScriptId::new("baz".into()).unwrap(),
            },
        ],
        &["foo"]

        ; "unknown scripts"
    )]
    fn parse_scripts_invalid_unknown(
        config_contents: &str,
        expected_errors: Vec<UnknownConfigScriptError>,
        expected_known_scripts: &[&str],
    ) {
        let workspace_dir = tempdir().unwrap();

        let graph = temp_workspace(workspace_dir.path(), config_contents);

        let pcx = ParseContext::new(&graph);

        let error = NextestConfig::from_sources(
            graph.workspace().root(),
            &pcx,
            None,
            &[][..],
            &btreeset! { ConfigExperimental::SetupScripts },
        )
        .expect_err("config is invalid");
        match error.kind() {
            ConfigParseErrorKind::UnknownConfigScripts {
                errors,
                known_scripts,
            } => {
                assert_eq!(
                    errors.len(),
                    expected_errors.len(),
                    "correct number of errors"
                );
                for (error, expected_error) in errors.iter().zip(expected_errors) {
                    assert_eq!(
                        error.profile_name, expected_error.profile_name,
                        "profile name matches"
                    );
                    assert_eq!(
                        error.name, expected_error.name,
                        "unknown script name matches"
                    );
                }
                assert_eq!(
                    known_scripts.len(),
                    expected_known_scripts.len(),
                    "correct number of known scripts"
                );
                for (script, expected_script) in known_scripts.iter().zip(expected_known_scripts) {
                    assert_eq!(
                        script.as_str(),
                        *expected_script,
                        "known script name matches"
                    );
                }
            }
            other => {
                panic!("for config error {other:?}, expected ConfigParseErrorKind::UnknownConfigScripts");
            }
        }
    }
}
