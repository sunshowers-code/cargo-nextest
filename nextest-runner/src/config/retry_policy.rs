// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::Deserialize;
use std::{cmp::Ordering, fmt, time::Duration};

/// Type for the retry config key.
#[derive(Debug, Copy, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "backoff", rename_all = "kebab-case", deny_unknown_fields)]
pub enum RetryPolicy {
    /// Fixed backoff.
    #[serde(rename_all = "kebab-case")]
    Fixed {
        /// Maximum retry count.
        count: usize,

        /// Delay between retries.
        #[serde(default, with = "humantime_serde")]
        delay: Duration,

        /// If set to true, randomness will be added to the delay on each retry attempt.
        #[serde(default)]
        jitter: bool,
    },

    /// Exponential backoff.
    #[serde(rename_all = "kebab-case")]
    Exponential {
        /// Maximum retry count.
        count: usize,

        /// Delay between retries. Not optional for exponential backoff.
        #[serde(with = "humantime_serde")]
        delay: Duration,

        /// If set to true, randomness will be added to the delay on each retry attempt.
        #[serde(default)]
        jitter: bool,

        /// If set, limits the delay between retries.
        #[serde(default, with = "humantime_serde")]
        max_delay: Option<Duration>,
    },
}

impl Default for RetryPolicy {
    #[inline]
    fn default() -> Self {
        Self::new_without_delay(0)
    }
}

impl RetryPolicy {
    /// Create new policy with no delay between retries.
    pub fn new_without_delay(count: usize) -> Self {
        Self::Fixed {
            count,
            delay: Duration::ZERO,
            jitter: false,
        }
    }

    /// Returns the number of retries.
    pub fn count(&self) -> usize {
        match self {
            Self::Fixed { count, .. } | Self::Exponential { count, .. } => *count,
        }
    }
}

pub(super) fn deserialize_retry_policy<'de, D>(
    deserializer: D,
) -> Result<Option<RetryPolicy>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct V;

    impl<'de2> serde::de::Visitor<'de2> for V {
        type Value = Option<RetryPolicy>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            write!(
                formatter,
                "a table ({{ count = 5, backoff = \"exponential\", delay = \"1s\", max-delay = \"10s\", jitter = true }}) or a number (5)"
            )
        }

        // Note that TOML uses i64, not u64.
        fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            match v.cmp(&0) {
                Ordering::Greater | Ordering::Equal => {
                    Ok(Some(RetryPolicy::new_without_delay(v as usize)))
                }
                Ordering::Less => Err(serde::de::Error::invalid_value(
                    serde::de::Unexpected::Signed(v),
                    &self,
                )),
            }
        }

        fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de2>,
        {
            RetryPolicy::deserialize(serde::de::value::MapAccessDeserializer::new(map)).map(Some)
        }
    }

    // Post-deserialize validation of retry policy.
    let retry_policy = deserializer.deserialize_any(V)?;
    match &retry_policy {
        Some(RetryPolicy::Fixed {
            count: _,
            delay,
            jitter,
        }) => {
            // Jitter can't be specified if delay is 0.
            if delay.is_zero() && *jitter {
                return Err(serde::de::Error::custom(
                    "`jitter` cannot be true if `delay` isn't specified or is zero",
                ));
            }
        }
        Some(RetryPolicy::Exponential {
            count,
            delay,
            jitter: _,
            max_delay,
        }) => {
            // Count can't be zero.
            if *count == 0 {
                return Err(serde::de::Error::custom(
                    "`count` cannot be zero with exponential backoff",
                ));
            }
            // Delay can't be zero.
            if delay.is_zero() {
                return Err(serde::de::Error::custom(
                    "`delay` cannot be zero with exponential backoff",
                ));
            }
            // Max delay, if specified, can't be zero.
            if max_delay.is_some_and(|f| f.is_zero()) {
                return Err(serde::de::Error::custom(
                    "`max-delay` cannot be zero with exponential backoff",
                ));
            }
            // Max delay can't be less than delay.
            if max_delay.is_some_and(|max_delay| max_delay < *delay) {
                return Err(serde::de::Error::custom(
                    "`max-delay` cannot be less than delay with exponential backoff",
                ));
            }
        }
        None => {}
    }

    Ok(retry_policy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{
            test_helpers::{binary_query, build_platforms, temp_workspace},
            NextestConfig,
        },
        errors::ConfigParseErrorKind,
    };
    use camino::Utf8Path;
    use camino_tempfile::tempdir;
    use config::ConfigError;
    use guppy::graph::cargo::BuildPlatform;
    use indoc::indoc;
    use nextest_filtering::{ParseContext, TestQuery};
    use test_case::test_case;

    #[test]
    fn parse_retries_valid() {
        let config_contents = indoc! {r#"
            [profile.default]
            retries = { backoff = "fixed", count = 3 }

            [profile.no-retries]
            retries = 0

            [profile.fixed-with-delay]
            retries = { backoff = "fixed", count = 3, delay = "1s" }

            [profile.exp]
            retries = { backoff = "exponential", count = 4, delay = "2s" }

            [profile.exp-with-max-delay]
            retries = { backoff = "exponential", count = 5, delay = "3s", max-delay = "10s" }

            [profile.exp-with-max-delay-and-jitter]
            retries = { backoff = "exponential", count = 6, delay = "4s", max-delay = "1m", jitter = true }
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
        assert_eq!(
            config
                .profile("default")
                .expect("default profile exists")
                .into_evaluatable(&build_platforms())
                .retries(),
            RetryPolicy::Fixed {
                count: 3,
                delay: Duration::ZERO,
                jitter: false,
            },
            "default retries matches"
        );

        assert_eq!(
            config
                .profile("no-retries")
                .expect("profile exists")
                .into_evaluatable(&build_platforms())
                .retries(),
            RetryPolicy::new_without_delay(0),
            "no-retries retries matches"
        );

        assert_eq!(
            config
                .profile("fixed-with-delay")
                .expect("profile exists")
                .into_evaluatable(&build_platforms())
                .retries(),
            RetryPolicy::Fixed {
                count: 3,
                delay: Duration::from_secs(1),
                jitter: false,
            },
            "fixed-with-delay retries matches"
        );

        assert_eq!(
            config
                .profile("exp")
                .expect("profile exists")
                .into_evaluatable(&build_platforms())
                .retries(),
            RetryPolicy::Exponential {
                count: 4,
                delay: Duration::from_secs(2),
                jitter: false,
                max_delay: None,
            },
            "exp retries matches"
        );

        assert_eq!(
            config
                .profile("exp-with-max-delay")
                .expect("profile exists")
                .into_evaluatable(&build_platforms())
                .retries(),
            RetryPolicy::Exponential {
                count: 5,
                delay: Duration::from_secs(3),
                jitter: false,
                max_delay: Some(Duration::from_secs(10)),
            },
            "exp-with-max-delay retries matches"
        );

        assert_eq!(
            config
                .profile("exp-with-max-delay-and-jitter")
                .expect("profile exists")
                .into_evaluatable(&build_platforms())
                .retries(),
            RetryPolicy::Exponential {
                count: 6,
                delay: Duration::from_secs(4),
                jitter: true,
                max_delay: Some(Duration::from_secs(60)),
            },
            "exp-with-max-delay-and-jitter retries matches"
        );
    }

    #[test_case(
        indoc!{r#"
            [profile.default]
            retries = { backoff = "foo" }
        "#},
        "unknown variant `foo`, expected `fixed` or `exponential`"
        ; "invalid value for backoff")]
    #[test_case(
        indoc!{r#"
            [profile.default]
            retries = { backoff = "fixed" }
        "#},
        "missing field `count`"
        ; "fixed specified without count")]
    #[test_case(
        indoc!{r#"
            [profile.default]
            retries = { backoff = "fixed", count = 1, delay = "foobar" }
        "#},
        "invalid value: string \"foobar\", expected a duration"
        ; "delay is not a valid duration")]
    #[test_case(
        indoc!{r#"
            [profile.default]
            retries = { backoff = "fixed", count = 1, jitter = true }
        "#},
        "`jitter` cannot be true if `delay` isn't specified or is zero"
        ; "jitter specified without delay")]
    #[test_case(
        indoc!{r#"
            [profile.default]
            retries = { backoff = "fixed", count = 1, max-delay = "10s" }
        "#},
        "unknown field `max-delay`, expected one of `count`, `delay`, `jitter`"
        ; "max-delay is incompatible with fixed backoff")]
    #[test_case(
        indoc!{r#"
            [profile.default]
            retries = { backoff = "exponential", count = 1 }
        "#},
        "missing field `delay`"
        ; "exponential backoff must specify delay")]
    #[test_case(
        indoc!{r#"
            [profile.default]
            retries = { backoff = "exponential", delay = "1s" }
        "#},
        "missing field `count`"
        ; "exponential backoff must specify count")]
    #[test_case(
        indoc!{r#"
            [profile.default]
            retries = { backoff = "exponential", count = 0, delay = "1s" }
        "#},
        "`count` cannot be zero with exponential backoff"
        ; "exponential backoff must have a non-zero count")]
    #[test_case(
        indoc!{r#"
            [profile.default]
            retries = { backoff = "exponential", count = 1, delay = "0s" }
        "#},
        "`delay` cannot be zero with exponential backoff"
        ; "exponential backoff must have a non-zero delay")]
    #[test_case(
        indoc!{r#"
            [profile.default]
            retries = { backoff = "exponential", count = 1, delay = "1s", max-delay = "0s" }
        "#},
        "`max-delay` cannot be zero with exponential backoff"
        ; "exponential backoff must have a non-zero max delay")]
    #[test_case(
        indoc!{r#"
            [profile.default]
            retries = { backoff = "exponential", count = 1, delay = "4s", max-delay = "2s", jitter = true }
        "#},
        "`max-delay` cannot be less than delay"
        ; "max-delay greater than delay")]
    fn parse_retries_invalid(config_contents: &str, expected_message: &str) {
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
            "expected message \"{message}\" to contain \"{expected_message}\""
        );
    }

    #[test_case(
        indoc! {r#"
            [[profile.default.overrides]]
            filter = "test(=my_test)"
            retries = 2

            [profile.ci]
        "#},
        BuildPlatform::Target,
        RetryPolicy::new_without_delay(2)

        ; "my_test matches exactly"
    )]
    #[test_case(
        indoc! {r#"
            [[profile.default.overrides]]
            filter = "!test(=my_test)"
            retries = 2

            [profile.ci]
        "#},
        BuildPlatform::Target,
        RetryPolicy::new_without_delay(0)

        ; "not match"
    )]
    #[test_case(
        indoc! {r#"
            [[profile.default.overrides]]
            filter = "test(=my_test)"

            [profile.ci]
        "#},
        BuildPlatform::Target,
        RetryPolicy::new_without_delay(0)

        ; "no retries specified"
    )]
    #[test_case(
        indoc! {r#"
            [[profile.default.overrides]]
            filter = "test(test)"
            retries = 2

            [[profile.default.overrides]]
            filter = "test(=my_test)"
            retries = 3

            [profile.ci]
        "#},
        BuildPlatform::Target,
        RetryPolicy::new_without_delay(2)

        ; "earlier configs override later ones"
    )]
    #[test_case(
        indoc! {r#"
            [[profile.default.overrides]]
            filter = "test(test)"
            retries = 2

            [profile.ci]

            [[profile.ci.overrides]]
            filter = "test(=my_test)"
            retries = 3
        "#},
        BuildPlatform::Target,
        RetryPolicy::new_without_delay(3)

        ; "profile-specific configs override default ones"
    )]
    #[test_case(
        indoc! {r#"
            [[profile.default.overrides]]
            filter = "(!package(test-package)) and test(test)"
            retries = 2

            [profile.ci]

            [[profile.ci.overrides]]
            filter = "!test(=my_test_2)"
            retries = 3
        "#},
        BuildPlatform::Target,
        RetryPolicy::new_without_delay(3)

        ; "no overrides match my_test exactly"
    )]
    #[test_case(
        indoc! {r#"
            [[profile.default.overrides]]
            platform = "x86_64-unknown-linux-gnu"
            filter = "test(test)"
            retries = 2

            [[profile.default.overrides]]
            filter = "test(=my_test)"
            retries = 3

            [profile.ci]
        "#},
        BuildPlatform::Host,
        RetryPolicy::new_without_delay(2)

        ; "earlier config applied because it matches host triple"
    )]
    #[test_case(
        indoc! {r#"
            [[profile.default.overrides]]
            platform = "aarch64-apple-darwin"
            filter = "test(test)"
            retries = 2

            [[profile.default.overrides]]
            filter = "test(=my_test)"
            retries = 3

            [profile.ci]
        "#},
        BuildPlatform::Host,
        RetryPolicy::new_without_delay(3)

        ; "earlier config ignored because it doesn't match host triple"
    )]
    #[test_case(
        indoc! {r#"
            [[profile.default.overrides]]
            platform = "aarch64-apple-darwin"
            filter = "test(test)"
            retries = 2

            [[profile.default.overrides]]
            filter = "test(=my_test)"
            retries = 3

            [profile.ci]
        "#},
        BuildPlatform::Target,
        RetryPolicy::new_without_delay(2)

        ; "earlier config applied because it matches target triple"
    )]
    #[test_case(
        indoc! {r#"
            [[profile.default.overrides]]
            platform = "x86_64-unknown-linux-gnu"
            filter = "test(test)"
            retries = 2

            [[profile.default.overrides]]
            filter = "test(=my_test)"
            retries = 3

            [profile.ci]
        "#},
        BuildPlatform::Target,
        RetryPolicy::new_without_delay(3)

        ; "earlier config ignored because it doesn't match target triple"
    )]
    #[test_case(
        indoc! {r#"
            [[profile.default.overrides]]
            platform = 'cfg(target_os = "macos")'
            filter = "test(test)"
            retries = 2

            [[profile.default.overrides]]
            filter = "test(=my_test)"
            retries = 3

            [profile.ci]
        "#},
        BuildPlatform::Target,
        RetryPolicy::new_without_delay(2)

        ; "earlier config applied because it matches target cfg expr"
    )]
    #[test_case(
        indoc! {r#"
            [[profile.default.overrides]]
            platform = 'cfg(target_arch = "x86_64")'
            filter = "test(test)"
            retries = 2

            [[profile.default.overrides]]
            filter = "test(=my_test)"
            retries = 3

            [profile.ci]
        "#},
        BuildPlatform::Target,
        RetryPolicy::new_without_delay(3)

        ; "earlier config ignored because it doesn't match target cfg expr"
    )]
    fn overrides_retries(
        config_contents: &str,
        build_platform: BuildPlatform,
        retries: RetryPolicy,
    ) {
        let workspace_dir = tempdir().unwrap();
        let workspace_path: &Utf8Path = workspace_dir.path();

        let graph = temp_workspace(workspace_path, config_contents);
        let package_id = graph.workspace().iter().next().unwrap().id();
        let pcx = ParseContext::new(&graph);

        let config = NextestConfig::from_sources(
            graph.workspace().root(),
            &pcx,
            None,
            &[][..],
            &Default::default(),
        )
        .unwrap();
        let binary_query = binary_query(&graph, package_id, "lib", "my-binary", build_platform);
        let query = TestQuery {
            binary_query: binary_query.to_query(),
            test_name: "my_test",
        };
        let profile = config
            .profile("ci")
            .expect("ci profile is defined")
            .into_evaluatable(&build_platforms());
        let settings_for = profile.settings_for(&query);
        assert_eq!(
            settings_for.retries(),
            retries,
            "actual retries don't match expected retries"
        );
    }
}
