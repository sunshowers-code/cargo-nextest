// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::errors::ToolConfigFileParseError;
use camino::{Utf8Path, Utf8PathBuf};
use std::str::FromStr;

/// A tool-specific config file.
///
/// Tool-specific config files are lower priority than repository configs, but higher priority than
/// the default config shipped with nextest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolConfigFile {
    /// The name of the tool.
    pub tool: String,

    /// The path to the config file.
    pub config_file: Utf8PathBuf,
}

impl FromStr for ToolConfigFile {
    type Err = ToolConfigFileParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input.split_once(':') {
            Some((tool, config_file)) => {
                if tool.is_empty() {
                    Err(ToolConfigFileParseError::EmptyToolName {
                        input: input.to_owned(),
                    })
                } else if config_file.is_empty() {
                    Err(ToolConfigFileParseError::EmptyConfigFile {
                        input: input.to_owned(),
                    })
                } else {
                    let config_file = Utf8Path::new(config_file);
                    if config_file.is_absolute() {
                        Ok(Self {
                            tool: tool.to_owned(),
                            config_file: Utf8PathBuf::from(config_file),
                        })
                    } else {
                        Err(ToolConfigFileParseError::ConfigFileNotAbsolute {
                            config_file: config_file.to_owned(),
                        })
                    }
                }
            }
            None => Err(ToolConfigFileParseError::InvalidFormat {
                input: input.to_owned(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        test_helpers::*, NextestConfig, NextestVersionConfig, NextestVersionReq, RetryPolicy,
        TestGroup, VersionOnlyConfig,
    };
    use camino_tempfile::tempdir;
    use guppy::graph::cargo::BuildPlatform;
    use nextest_filtering::{ParseContext, TestQuery};

    #[test]
    fn parse_tool_config_file() {
        cfg_if::cfg_if! {
            if #[cfg(windows)] {
                let valid = ["tool:C:\\foo\\bar", "tool:\\\\?\\C:\\foo\\bar"];
                let invalid = ["C:\\foo\\bar", "tool:\\foo\\bar", "tool:", ":/foo/bar"];
            } else {
                let valid = ["tool:/foo/bar"];
                let invalid = ["/foo/bar", "tool:", ":/foo/bar", "tool:foo/bar"];
            }
        }

        for valid_input in valid {
            valid_input.parse::<ToolConfigFile>().unwrap_or_else(|err| {
                panic!("valid input {valid_input} should parse correctly: {err}")
            });
        }

        for invalid_input in invalid {
            invalid_input
                .parse::<ToolConfigFile>()
                .expect_err(&format!("invalid input {invalid_input} should error out"));
        }
    }

    #[test]
    fn tool_config_basic() {
        let config_contents = r#"
        nextest-version = "0.9.50"

        [profile.default]
        retries = 3

        [[profile.default.overrides]]
        filter = 'test(test_foo)'
        retries = 20
        test-group = 'foo'

        [[profile.default.overrides]]
        filter = 'test(test_quux)'
        test-group = '@tool:tool1:group1'

        [test-groups.foo]
        max-threads = 2
        "#;

        let tool1_config_contents = r#"
        nextest-version = { required = "0.9.51", recommended = "0.9.52" }

        [profile.default]
        retries = 4

        [[profile.default.overrides]]
        filter = 'test(test_bar)'
        retries = 21

        [profile.tool]
        retries = 12

        [[profile.tool.overrides]]
        filter = 'test(test_baz)'
        retries = 22
        test-group = '@tool:tool1:group1'

        [[profile.tool.overrides]]
        filter = 'test(test_quux)'
        retries = 22
        test-group = '@tool:tool2:group2'

        [test-groups.'@tool:tool1:group1']
        max-threads = 2
        "#;

        let tool2_config_contents = r#"
        nextest-version = { recommended = "0.9.49" }

        [profile.default]
        retries = 5

        [[profile.default.overrides]]
        filter = 'test(test_)'
        retries = 23

        [profile.tool]
        retries = 16

        [[profile.tool.overrides]]
        filter = 'test(test_ba)'
        retries = 24
        test-group = '@tool:tool2:group2'

        [[profile.tool.overrides]]
        filter = 'test(test_)'
        retries = 25
        test-group = '@global'

        [profile.tool2]
        retries = 18

        [[profile.tool2.overrides]]
        filter = 'all()'
        retries = 26

        [test-groups.'@tool:tool2:group2']
        max-threads = 4
        "#;

        let workspace_dir = tempdir().unwrap();

        let graph = temp_workspace(workspace_dir.path(), config_contents);
        let workspace_root = graph.workspace().root();
        let tool1_path = workspace_root.join(".config/tool1.toml");
        let tool2_path = workspace_root.join(".config/tool2.toml");
        std::fs::write(&tool1_path, tool1_config_contents).unwrap();
        std::fs::write(&tool2_path, tool2_config_contents).unwrap();

        let tool_config_files = [
            ToolConfigFile {
                tool: "tool1".to_owned(),
                config_file: tool1_path,
            },
            ToolConfigFile {
                tool: "tool2".to_owned(),
                config_file: tool2_path,
            },
        ];

        let version_only_config =
            VersionOnlyConfig::from_sources(workspace_root, None, &tool_config_files).unwrap();
        let nextest_version = version_only_config.nextest_version();
        assert_eq!(
            nextest_version,
            &NextestVersionConfig {
                required: NextestVersionReq::Version {
                    version: "0.9.51".parse().unwrap(),
                    tool: Some("tool1".to_owned())
                },
                recommended: NextestVersionReq::Version {
                    version: "0.9.52".parse().unwrap(),
                    tool: Some("tool1".to_owned())
                }
            },
        );

        let pcx = ParseContext::new(&graph);
        let config = NextestConfig::from_sources(
            workspace_root,
            &pcx,
            None,
            &tool_config_files,
            &Default::default(),
        )
        .expect("config is valid");

        let default_profile = config
            .profile(NextestConfig::DEFAULT_PROFILE)
            .expect("default profile is present")
            .into_evaluatable(&build_platforms());
        // This is present in .config/nextest.toml and is the highest priority
        assert_eq!(default_profile.retries(), RetryPolicy::new_without_delay(3));

        let package_id = graph.workspace().iter().next().unwrap().id();

        let binary_query = binary_query(
            &graph,
            package_id,
            "lib",
            "my-binary",
            BuildPlatform::Target,
        );
        let test_foo_query = TestQuery {
            binary_query: binary_query.to_query(),
            test_name: "test_foo",
        };
        let test_bar_query = TestQuery {
            binary_query: binary_query.to_query(),
            test_name: "test_bar",
        };
        let test_baz_query = TestQuery {
            binary_query: binary_query.to_query(),
            test_name: "test_baz",
        };
        let test_quux_query = TestQuery {
            binary_query: binary_query.to_query(),
            test_name: "test_quux",
        };

        assert_eq!(
            default_profile.settings_for(&test_foo_query).retries(),
            RetryPolicy::new_without_delay(20),
            "retries for test_foo/default profile"
        );
        assert_eq!(
            default_profile.settings_for(&test_foo_query).test_group(),
            &test_group("foo"),
            "test_group for test_foo/default profile"
        );
        assert_eq!(
            default_profile.settings_for(&test_bar_query).retries(),
            RetryPolicy::new_without_delay(21),
            "retries for test_bar/default profile"
        );
        assert_eq!(
            default_profile.settings_for(&test_bar_query).test_group(),
            &TestGroup::Global,
            "test_group for test_bar/default profile"
        );
        assert_eq!(
            default_profile.settings_for(&test_baz_query).retries(),
            RetryPolicy::new_without_delay(23),
            "retries for test_baz/default profile"
        );
        assert_eq!(
            default_profile.settings_for(&test_quux_query).test_group(),
            &test_group("@tool:tool1:group1"),
            "test group for test_quux/default profile"
        );

        let tool_profile = config
            .profile("tool")
            .expect("tool profile is present")
            .into_evaluatable(&build_platforms());
        assert_eq!(tool_profile.retries(), RetryPolicy::new_without_delay(12));
        assert_eq!(
            tool_profile.settings_for(&test_foo_query).retries(),
            RetryPolicy::new_without_delay(25),
            "retries for test_foo/default profile"
        );
        assert_eq!(
            tool_profile.settings_for(&test_bar_query).retries(),
            RetryPolicy::new_without_delay(24),
            "retries for test_bar/default profile"
        );
        assert_eq!(
            tool_profile.settings_for(&test_baz_query).retries(),
            RetryPolicy::new_without_delay(22),
            "retries for test_baz/default profile"
        );

        let tool2_profile = config
            .profile("tool2")
            .expect("tool2 profile is present")
            .into_evaluatable(&build_platforms());
        assert_eq!(tool2_profile.retries(), RetryPolicy::new_without_delay(18));
        assert_eq!(
            tool2_profile.settings_for(&test_foo_query).retries(),
            RetryPolicy::new_without_delay(26),
            "retries for test_foo/default profile"
        );
        assert_eq!(
            tool2_profile.settings_for(&test_bar_query).retries(),
            RetryPolicy::new_without_delay(26),
            "retries for test_bar/default profile"
        );
        assert_eq!(
            tool2_profile.settings_for(&test_baz_query).retries(),
            RetryPolicy::new_without_delay(26),
            "retries for test_baz/default profile"
        );
    }
}
