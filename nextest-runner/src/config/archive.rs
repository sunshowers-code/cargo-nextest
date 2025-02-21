// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::TrackDefault;
use crate::config::helpers::deserialize_relative_path;
use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use serde::{de::Unexpected, Deserialize};
use std::fmt;

/// Configuration for archives.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct ArchiveConfig {
    /// Files to include in the archive.
    pub include: Vec<ArchiveInclude>,
}

/// Type for the archive-include key.
///
/// # Notes
///
/// This is `deny_unknown_fields` because if we take additional arguments in the future, they're
/// likely to change semantics in an incompatible way.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ArchiveInclude {
    // We only allow well-formed relative paths within the target directory here. It's possible we
    // can relax this in the future, but better safe than sorry for now.
    #[serde(deserialize_with = "deserialize_relative_path")]
    path: Utf8PathBuf,
    relative_to: ArchiveRelativeTo,
    #[serde(default = "default_depth")]
    depth: TrackDefault<RecursionDepth>,
    #[serde(default = "default_on_missing")]
    on_missing: ArchiveIncludeOnMissing,
}

impl ArchiveInclude {
    /// The maximum depth of recursion.
    pub fn depth(&self) -> RecursionDepth {
        self.depth.value
    }

    /// Whether the depth was deserialized. If false, the default value was used.
    pub fn is_depth_deserialized(&self) -> bool {
        self.depth.is_deserialized
    }

    /// Join the path with the given target dir.
    pub fn join_path(&self, target_dir: &Utf8Path) -> Utf8PathBuf {
        match self.relative_to {
            ArchiveRelativeTo::Target => join_rel_path(target_dir, &self.path),
        }
    }

    /// What to do when the path is missing.
    pub fn on_missing(&self) -> ArchiveIncludeOnMissing {
        self.on_missing
    }
}

fn default_depth() -> TrackDefault<RecursionDepth> {
    // We use a high-but-not-infinite depth.
    TrackDefault::with_default_value(RecursionDepth::Finite(16))
}

fn default_on_missing() -> ArchiveIncludeOnMissing {
    ArchiveIncludeOnMissing::Warn
}

fn join_rel_path(a: &Utf8Path, rel: &Utf8Path) -> Utf8PathBuf {
    // This joins the subset of components that deserialize_relative_path
    // allows. We also always use "/" to ensure consistency across platforms.
    let mut out = String::from(a.to_owned());

    for component in rel.components() {
        match component {
            Utf8Component::CurDir => {}
            Utf8Component::Normal(p) => {
                out.push('/');
                out.push_str(p);
            }
            other => unreachable!(
                "found invalid component {other:?}, deserialize_relative_path should have errored"
            ),
        }
    }

    out.into()
}

/// What to do when an archive-include path is missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveIncludeOnMissing {
    /// Ignore and continue.
    Ignore,

    /// Warn and continue.
    Warn,

    /// Produce an error.
    Error,
}

impl<'de> Deserialize<'de> for ArchiveIncludeOnMissing {
    fn deserialize<D>(deserializer: D) -> Result<ArchiveIncludeOnMissing, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ArchiveIncludeOnMissingVisitor;

        impl serde::de::Visitor<'_> for ArchiveIncludeOnMissingVisitor {
            type Value = ArchiveIncludeOnMissing;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a string: \"ignore\", \"warn\", or \"error\"")
            }

            fn visit_str<E>(self, value: &str) -> Result<ArchiveIncludeOnMissing, E>
            where
                E: serde::de::Error,
            {
                match value {
                    "ignore" => Ok(ArchiveIncludeOnMissing::Ignore),
                    "warn" => Ok(ArchiveIncludeOnMissing::Warn),
                    "error" => Ok(ArchiveIncludeOnMissing::Error),
                    _ => Err(serde::de::Error::invalid_value(
                        Unexpected::Str(value),
                        &self,
                    )),
                }
            }
        }

        deserializer.deserialize_any(ArchiveIncludeOnMissingVisitor)
    }
}

/// Defines the base of the path
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ArchiveRelativeTo {
    /// Path starts at the target directory
    Target,
    // TODO: add support for profile relative
    //TargetProfile,
}

/// Recursion depth.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RecursionDepth {
    /// A specific depth.
    Finite(usize),

    /// Infinite recursion.
    Infinite,
}

impl RecursionDepth {
    pub(crate) const ZERO: RecursionDepth = RecursionDepth::Finite(0);

    pub(crate) fn is_zero(self) -> bool {
        self == Self::ZERO
    }

    pub(crate) fn decrement(self) -> Self {
        match self {
            Self::ZERO => panic!("attempted to decrement zero"),
            Self::Finite(n) => Self::Finite(n - 1),
            Self::Infinite => Self::Infinite,
        }
    }

    pub(crate) fn unwrap_finite(self) -> usize {
        match self {
            Self::Finite(n) => n,
            Self::Infinite => panic!("expected finite recursion depth"),
        }
    }
}

impl fmt::Display for RecursionDepth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Finite(n) => write!(f, "{n}"),
            Self::Infinite => write!(f, "infinite"),
        }
    }
}

impl<'de> Deserialize<'de> for RecursionDepth {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct RecursionDepthVisitor;

        impl serde::de::Visitor<'_> for RecursionDepthVisitor {
            type Value = RecursionDepth;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a non-negative integer or \"infinite\"")
            }

            // TOML uses i64, not u64
            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value < 0 {
                    return Err(serde::de::Error::invalid_value(
                        Unexpected::Signed(value),
                        &self,
                    ));
                }
                Ok(RecursionDepth::Finite(value as usize))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match value {
                    "infinite" => Ok(RecursionDepth::Infinite),
                    _ => Err(serde::de::Error::invalid_value(
                        Unexpected::Str(value),
                        &self,
                    )),
                }
            }
        }

        deserializer.deserialize_any(RecursionDepthVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{
            test_helpers::{build_platforms, temp_workspace},
            NextestConfig,
        },
        errors::ConfigParseErrorKind,
    };
    use camino::Utf8Path;
    use camino_tempfile::tempdir;
    use config::ConfigError;
    use indoc::indoc;
    use nextest_filtering::ParseContext;
    use test_case::test_case;

    #[test]
    fn parse_valid() {
        let config_contents = indoc! {r#"
            [profile.default.archive]
            include = [
                { path = "foo", relative-to = "target" },
                { path = "bar", relative-to = "target", depth = 1, on-missing = "error" },
            ]

            [profile.profile1]
            archive.include = [
                { path = "baz", relative-to = "target", depth = 0, on-missing = "ignore" },
            ]

            [profile.profile2]
            archive.include = []

            [profile.profile3]
        "#};

        let workspace_dir = tempdir().unwrap();

        let graph = temp_workspace(workspace_dir.path(), config_contents);

        let pcx = ParseContext::new(&graph);

        let config = NextestConfig::from_sources(
            graph.workspace().root(),
            &pcx,
            None,
            [],
            &Default::default(),
        )
        .expect("config is valid");

        let default_config = ArchiveConfig {
            include: vec![
                ArchiveInclude {
                    path: "foo".into(),
                    relative_to: ArchiveRelativeTo::Target,
                    depth: default_depth(),
                    on_missing: ArchiveIncludeOnMissing::Warn,
                },
                ArchiveInclude {
                    path: "bar".into(),
                    relative_to: ArchiveRelativeTo::Target,
                    depth: TrackDefault::with_deserialized_value(RecursionDepth::Finite(1)),
                    on_missing: ArchiveIncludeOnMissing::Error,
                },
            ],
        };

        assert_eq!(
            config
                .profile("default")
                .expect("default profile exists")
                .into_evaluatable(&build_platforms())
                .archive_config(),
            &default_config,
            "default matches"
        );

        assert_eq!(
            config
                .profile("profile1")
                .expect("profile exists")
                .into_evaluatable(&build_platforms())
                .archive_config(),
            &ArchiveConfig {
                include: vec![ArchiveInclude {
                    path: "baz".into(),
                    relative_to: ArchiveRelativeTo::Target,
                    depth: TrackDefault::with_deserialized_value(RecursionDepth::ZERO),
                    on_missing: ArchiveIncludeOnMissing::Ignore,
                }],
            },
            "profile1 matches"
        );

        assert_eq!(
            config
                .profile("profile2")
                .expect("default profile exists")
                .into_evaluatable(&build_platforms())
                .archive_config(),
            &ArchiveConfig { include: vec![] },
            "profile2 matches"
        );

        assert_eq!(
            config
                .profile("profile3")
                .expect("default profile exists")
                .into_evaluatable(&build_platforms())
                .archive_config(),
            &default_config,
            "profile3 matches"
        );
    }

    #[test_case(
        indoc!{r#"
            [profile.default]
            archive.include = { path = "foo", relative-to = "target" }
        "#},
        r"invalid type: map, expected a sequence"
        ; "missing list")]
    #[test_case(
        indoc!{r#"
            [profile.default]
            archive.include = [
                { path = "foo" }
            ]
        "#},
        r"missing field `relative-to`"
        ; "missing relative-to")]
    #[test_case(
        indoc!{r#"
            [profile.default]
            archive.include = [
                { path = "bar", relative-to = "unknown" }
            ]
        "#},
        r"enum ArchiveRelativeTo does not have variant constructor unknown"
        ; "invalid relative-to")]
    #[test_case(
        indoc!{r#"
            [profile.default]
            archive.include = [
                { path = "bar", relative-to = "target", depth = -1 }
            ]
        "#},
        r#"invalid value: integer `-1`, expected a non-negative integer or "infinite""#
        ; "negative depth")]
    #[test_case(
        indoc!{r#"
            [profile.default]
            archive.include = [
                { path = "foo/../bar", relative-to = "target" }
            ]
        "#},
        r#"invalid value: string "foo/../bar", expected a relative path with no parent components"#
        ; "parent component")]
    #[test_case(
        indoc!{r#"
            [profile.default]
            archive.include = [
                { path = "/foo/bar", relative-to = "target" }
            ]
        "#},
        r#"invalid value: string "/foo/bar", expected a relative path with no parent components"#
        ; "absolute path")]
    #[test_case(
        indoc!{r#"
            [profile.default]
            archive.include = [
                { path = "foo", relative-to = "target", on-missing = "unknown" }
            ]
        "#},
        r#"invalid value: string "unknown", expected a string: "ignore", "warn", or "error""#
        ; "invalid on-missing")]
    #[test_case(
        indoc!{r#"
            [profile.default]
            archive.include = [
                { path = "foo", relative-to = "target", on-missing = 42 }
            ]
        "#},
        r#"invalid type: integer `42`, expected a string: "ignore", "warn", or "error""#
        ; "invalid on-missing type")]
    fn parse_invalid(config_contents: &str, expected_message: &str) {
        let workspace_dir = tempdir().unwrap();
        let workspace_path: &Utf8Path = workspace_dir.path();

        let graph = temp_workspace(workspace_path, config_contents);

        let pcx = ParseContext::new(&graph);

        let config_err = NextestConfig::from_sources(
            graph.workspace().root(),
            &pcx,
            None,
            [],
            &Default::default(),
        )
        .expect_err("config expected to be invalid");

        let message = match config_err.kind() {
            ConfigParseErrorKind::DeserializeError(path_error) => match path_error.inner() {
                ConfigError::Message(message) => message,
                other => {
                    panic!("for config error {config_err:?}, expected ConfigError::Message for inner error {other:?}");
                }
            },
            other => {
                panic!(
                    "for config error {other:?}, expected ConfigParseErrorKind::DeserializeError"
                );
            }
        };

        assert!(
            message.contains(expected_message),
            "expected message: {expected_message}\nactual message: {message}"
        );
    }

    #[test]
    fn test_join_rel_path() {
        let inputs = [
            ("a", "b", "a/b"),
            ("a", "b/c", "a/b/c"),
            ("a", "", "a"),
            ("a", ".", "a"),
        ];

        for (base, rel, expected) in inputs {
            assert_eq!(
                join_rel_path(Utf8Path::new(base), Utf8Path::new(rel)),
                Utf8Path::new(expected),
                "actual matches expected -- base: {base}, rel: {rel}"
            );
        }
    }
}
