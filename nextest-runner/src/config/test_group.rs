// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::{ConfigIdentifier, TestThreads};
use crate::errors::InvalidCustomTestGroupName;
use serde::Deserialize;
use smol_str::SmolStr;
use std::{fmt, str::FromStr};

/// Represents the test group a test is in.
#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum TestGroup {
    /// This test is in the named custom group.
    Custom(CustomTestGroup),

    /// This test is not in a group.
    Global,
}

impl TestGroup {
    /// The string `"@global"`, indicating the global test group.
    pub const GLOBAL_STR: &'static str = "@global";

    pub(crate) fn make_all_groups(
        custom_groups: impl IntoIterator<Item = CustomTestGroup>,
    ) -> impl Iterator<Item = Self> {
        custom_groups
            .into_iter()
            .map(TestGroup::Custom)
            .chain(std::iter::once(TestGroup::Global))
    }
}

impl<'de> Deserialize<'de> for TestGroup {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Try and deserialize the group as a string. (Note: we don't deserialize a
        // `CustomTestGroup` directly because that errors out on None.
        let group = SmolStr::deserialize(deserializer)?;
        if group == Self::GLOBAL_STR {
            Ok(TestGroup::Global)
        } else {
            Ok(TestGroup::Custom(
                CustomTestGroup::new(group).map_err(serde::de::Error::custom)?,
            ))
        }
    }
}

impl FromStr for TestGroup {
    type Err = InvalidCustomTestGroupName;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == Self::GLOBAL_STR {
            Ok(TestGroup::Global)
        } else {
            Ok(TestGroup::Custom(CustomTestGroup::new(s.into())?))
        }
    }
}

impl fmt::Display for TestGroup {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            TestGroup::Global => write!(f, "@global"),
            TestGroup::Custom(group) => write!(f, "{}", group.as_str()),
        }
    }
}

/// Represents a custom test group.
#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct CustomTestGroup(ConfigIdentifier);

impl CustomTestGroup {
    /// Creates a new custom test group, returning an error if it is invalid.
    pub fn new(name: SmolStr) -> Result<Self, InvalidCustomTestGroupName> {
        let identifier = ConfigIdentifier::new(name).map_err(InvalidCustomTestGroupName)?;
        Ok(Self(identifier))
    }

    /// Creates a new custom test group from an identifier.
    pub fn from_identifier(identifier: ConfigIdentifier) -> Self {
        Self(identifier)
    }

    /// Returns the test group as a [`ConfigIdentifier`].
    pub fn as_identifier(&self) -> &ConfigIdentifier {
        &self.0
    }

    /// Returns the test group as a string.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl<'de> Deserialize<'de> for CustomTestGroup {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Try and deserialize as a string.
        let identifier = SmolStr::deserialize(deserializer)?;
        Self::new(identifier).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for CustomTestGroup {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Configuration for a test group.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TestGroupConfig {
    /// The maximum number of threads allowed for this test group.
    pub max_threads: TestThreads,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{test_helpers::*, NextestConfig, ToolConfigFile},
        errors::{ConfigParseErrorKind, UnknownTestGroupError},
    };
    use camino::Utf8Path;
    use camino_tempfile::tempdir;
    use indoc::indoc;
    use maplit::btreeset;
    use nextest_filtering::ParseContext;
    use std::collections::BTreeSet;
    use test_case::test_case;

    #[derive(Debug)]
    enum GroupExpectedError {
        DeserializeError(&'static str),
        InvalidTestGroups(BTreeSet<CustomTestGroup>),
    }

    #[test_case(
        indoc!{r#"
            [test-groups."@tool:my-tool:foo"]
            max-threads = 1
        "#},
        Ok(btreeset! {custom_test_group("user-group"), custom_test_group("@tool:my-tool:foo")})
        ; "group name valid")]
    #[test_case(
        indoc!{r#"
            [test-groups.foo]
            max-threads = 1
        "#},
        Err(GroupExpectedError::InvalidTestGroups(btreeset! {custom_test_group("foo")}))
        ; "group name doesn't start with @tool:")]
    #[test_case(
        indoc!{r#"
            [test-groups."@tool:moo:test"]
            max-threads = 1
        "#},
        Err(GroupExpectedError::InvalidTestGroups(btreeset! {custom_test_group("@tool:moo:test")}))
        ; "group name doesn't start with tool name")]
    #[test_case(
        indoc!{r#"
            [test-groups."@tool:my-tool"]
            max-threads = 1
        "#},
        Err(GroupExpectedError::DeserializeError("test-groups.@tool:my-tool: invalid custom test group name: tool identifier not of the form \"@tool:tool-name:identifier\": `@tool:my-tool`"))
        ; "group name missing suffix colon")]
    #[test_case(
        indoc!{r#"
            [test-groups.'@global']
            max-threads = 1
        "#},
        Err(GroupExpectedError::DeserializeError("test-groups.@global: invalid custom test group name: invalid identifier `@global`"))
        ; "group name is @global")]
    #[test_case(
        indoc!{r#"
            [test-groups.'@foo']
            max-threads = 1
        "#},
        Err(GroupExpectedError::DeserializeError("test-groups.@foo: invalid custom test group name: invalid identifier `@foo`"))
        ; "group name starts with @")]
    fn tool_config_define_groups(
        input: &str,
        expected: Result<BTreeSet<CustomTestGroup>, GroupExpectedError>,
    ) {
        let config_contents = indoc! {r#"
            [profile.default]
            test-group = "user-group"

            [test-groups.user-group]
            max-threads = 1
        "#};
        let workspace_dir = tempdir().unwrap();

        let graph = temp_workspace(workspace_dir.path(), config_contents);
        let workspace_root = graph.workspace().root();
        let tool_path = workspace_root.join(".config/tool.toml");
        std::fs::write(&tool_path, input).unwrap();

        let pcx = ParseContext::new(&graph);
        let config_res = NextestConfig::from_sources(
            workspace_root,
            &pcx,
            None,
            &[ToolConfigFile {
                tool: "my-tool".to_owned(),
                config_file: tool_path.clone(),
            }][..],
            &Default::default(),
        );
        match expected {
            Ok(expected_groups) => {
                let config = config_res.expect("config is valid");
                let profile = config.profile("default").expect("default profile is known");
                let profile = profile.into_evaluatable(&build_platforms());
                assert_eq!(
                    profile
                        .test_group_config()
                        .keys()
                        .cloned()
                        .collect::<BTreeSet<_>>(),
                    expected_groups
                );
            }
            Err(expected_error) => {
                let error = config_res.expect_err("config is invalid");
                assert_eq!(error.config_file(), &tool_path);
                assert_eq!(error.tool(), Some("my-tool"));
                match &expected_error {
                    GroupExpectedError::InvalidTestGroups(expected_groups) => {
                        assert!(
                            matches!(
                                error.kind(),
                                ConfigParseErrorKind::InvalidTestGroupsDefinedByTool(groups)
                                    if groups == expected_groups
                            ),
                            "expected config.kind ({}) to be {:?}",
                            error.kind(),
                            expected_error,
                        );
                    }
                    GroupExpectedError::DeserializeError(error_str) => {
                        assert!(
                            matches!(
                                error.kind(),
                                ConfigParseErrorKind::DeserializeError(error)
                                    if error.to_string() == *error_str
                            ),
                            "expected config.kind ({}) to be {:?}",
                            error.kind(),
                            expected_error,
                        );
                    }
                }
            }
        }
    }

    #[test_case(
        indoc!{r#"
            [test-groups."my-group"]
            max-threads = 1
        "#},
        Ok(btreeset! {custom_test_group("my-group")})
        ; "group name valid")]
    #[test_case(
        indoc!{r#"
            [test-groups."@tool:"]
            max-threads = 1
        "#},
        Err(GroupExpectedError::DeserializeError("test-groups.@tool:: invalid custom test group name: tool identifier not of the form \"@tool:tool-name:identifier\": `@tool:`"))
        ; "group name starts with @tool:")]
    #[test_case(
        indoc!{r#"
            [test-groups.'@global']
            max-threads = 1
        "#},
        Err(GroupExpectedError::DeserializeError("test-groups.@global: invalid custom test group name: invalid identifier `@global`"))
        ; "group name is @global")]
    #[test_case(
        indoc!{r#"
            [test-groups.'@foo']
            max-threads = 1
        "#},
        Err(GroupExpectedError::DeserializeError("test-groups.@foo: invalid custom test group name: invalid identifier `@foo`"))
        ; "group name starts with @")]
    fn user_config_define_groups(
        config_contents: &str,
        expected: Result<BTreeSet<CustomTestGroup>, GroupExpectedError>,
    ) {
        let workspace_dir = tempdir().unwrap();
        let workspace_path: &Utf8Path = workspace_dir.path();

        let graph = temp_workspace(workspace_path, config_contents);
        let workspace_root = graph.workspace().root();

        let pcx = ParseContext::new(&graph);
        let config_res =
            NextestConfig::from_sources(workspace_root, &pcx, None, &[][..], &Default::default());
        match expected {
            Ok(expected_groups) => {
                let config = config_res.expect("config is valid");
                let profile = config.profile("default").expect("default profile is known");
                let profile = profile.into_evaluatable(&build_platforms());
                assert_eq!(
                    profile
                        .test_group_config()
                        .keys()
                        .cloned()
                        .collect::<BTreeSet<_>>(),
                    expected_groups
                );
            }
            Err(expected_error) => {
                let error = config_res.expect_err("config is invalid");
                assert_eq!(error.tool(), None);
                match &expected_error {
                    GroupExpectedError::InvalidTestGroups(expected_groups) => {
                        assert!(
                            matches!(
                                error.kind(),
                                ConfigParseErrorKind::InvalidTestGroupsDefined(groups)
                                    if groups == expected_groups
                            ),
                            "expected config.kind ({}) to be {:?}",
                            error.kind(),
                            expected_error,
                        );
                    }
                    GroupExpectedError::DeserializeError(error_str) => {
                        assert!(
                            matches!(
                                error.kind(),
                                ConfigParseErrorKind::DeserializeError(error)
                                    if error.to_string() == *error_str
                            ),
                            "expected config.kind ({}) to be {:?}",
                            error.kind(),
                            expected_error,
                        );
                    }
                }
            }
        }
    }

    #[test_case(
        indoc!{r#"
            [[profile.default.overrides]]
            filter = 'all()'
            test-group = "foo"
        "#},
        "",
        "",
        Some("tool1"),
        vec![UnknownTestGroupError {
            profile_name: "default".to_owned(),
            name: test_group("foo"),
        }],
        btreeset! { TestGroup::Global }
        ; "unknown group in tool config")]
    #[test_case(
        "",
        "",
        indoc!{r#"
            [[profile.default.overrides]]
            filter = 'all()'
            test-group = "foo"
        "#},
        None,
        vec![UnknownTestGroupError {
            profile_name: "default".to_owned(),
            name: test_group("foo"),
        }],
        btreeset! { TestGroup::Global }
        ; "unknown group in user config")]
    #[test_case(
        indoc!{r#"
            [[profile.default.overrides]]
            filter = 'all()'
            test-group = "@tool:tool1:foo"

            [test-groups."@tool:tool1:foo"]
            max-threads = 1
        "#},
        indoc!{r#"
            [[profile.default.overrides]]
            filter = 'all()'
            test-group = "@tool:tool1:foo"
        "#},
        indoc!{r#"
            [[profile.default.overrides]]
            filter = 'all()'
            test-group = "foo"
        "#},
        Some("tool2"),
        vec![UnknownTestGroupError {
            profile_name: "default".to_owned(),
            name: test_group("@tool:tool1:foo"),
        }],
        btreeset! { TestGroup::Global }
        ; "depends on downstream tool config")]
    #[test_case(
        indoc!{r#"
            [[profile.default.overrides]]
            filter = 'all()'
            test-group = "foo"
        "#},
        "",
        indoc!{r#"
            [[profile.default.overrides]]
            filter = 'all()'
            test-group = "foo"

            [test-groups.foo]
            max-threads = 1
        "#},
        Some("tool1"),
        vec![UnknownTestGroupError {
            profile_name: "default".to_owned(),
            name: test_group("foo"),
        }],
        btreeset! { TestGroup::Global }
        ; "depends on user config")]
    fn unknown_groups(
        tool1_config: &str,
        tool2_config: &str,
        user_config: &str,
        tool: Option<&str>,
        expected_errors: Vec<UnknownTestGroupError>,
        expected_known_groups: BTreeSet<TestGroup>,
    ) {
        let workspace_dir = tempdir().unwrap();
        let workspace_path: &Utf8Path = workspace_dir.path();

        let graph = temp_workspace(workspace_path, user_config);
        let workspace_root = graph.workspace().root();
        let tool1_path = workspace_root.join(".config/tool1.toml");
        std::fs::write(&tool1_path, tool1_config).unwrap();
        let tool2_path = workspace_root.join(".config/tool2.toml");
        std::fs::write(&tool2_path, tool2_config).unwrap();

        let pcx = ParseContext::new(&graph);
        let config = NextestConfig::from_sources(
            workspace_root,
            &pcx,
            None,
            &[
                ToolConfigFile {
                    tool: "tool1".to_owned(),
                    config_file: tool1_path,
                },
                ToolConfigFile {
                    tool: "tool2".to_owned(),
                    config_file: tool2_path,
                },
            ][..],
            &Default::default(),
        )
        .expect_err("config is invalid");
        assert_eq!(config.tool(), tool);
        match config.kind() {
            ConfigParseErrorKind::UnknownTestGroups {
                errors,
                known_groups,
            } => {
                assert_eq!(errors, &expected_errors, "expected errors match");
                assert_eq!(known_groups, &expected_known_groups, "known groups match");
            }
            other => {
                panic!("expected ConfigParseErrorKind::UnknownTestGroups, got {other}");
            }
        }
    }
}
