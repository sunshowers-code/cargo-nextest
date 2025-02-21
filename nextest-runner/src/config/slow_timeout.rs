// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{de::IntoDeserializer, Deserialize};
use std::{fmt, num::NonZeroUsize, time::Duration};

/// Type for the slow-timeout config key.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct SlowTimeout {
    #[serde(with = "humantime_serde")]
    pub(crate) period: Duration,
    #[serde(default)]
    pub(crate) terminate_after: Option<NonZeroUsize>,
    #[serde(with = "humantime_serde", default = "default_grace_period")]
    pub(crate) grace_period: Duration,
}

impl SlowTimeout {
    /// A reasonable value for "maximum slow timeout".
    pub(crate) const VERY_LARGE: Self = Self {
        // See far_future() in pausable_sleep.rs for why this is roughly 30 years.
        period: Duration::from_secs(86400 * 365 * 30),
        terminate_after: None,
        grace_period: Duration::from_secs(10),
    };
}

fn default_grace_period() -> Duration {
    Duration::from_secs(10)
}

pub(super) fn deserialize_slow_timeout<'de, D>(
    deserializer: D,
) -> Result<Option<SlowTimeout>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct V;

    impl<'de2> serde::de::Visitor<'de2> for V {
        type Value = Option<SlowTimeout>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            write!(
                formatter,
                "a table ({{ period = \"60s\", terminate-after = 2 }}) or a string (\"60s\")"
            )
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            if v.is_empty() {
                Ok(None)
            } else {
                let period = humantime_serde::deserialize(v.into_deserializer())?;
                Ok(Some(SlowTimeout {
                    period,
                    terminate_after: None,
                    grace_period: default_grace_period(),
                }))
            }
        }

        fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de2>,
        {
            SlowTimeout::deserialize(serde::de::value::MapAccessDeserializer::new(map)).map(Some)
        }
    }

    deserializer.deserialize_any(V)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        test_helpers::{build_platforms, temp_workspace},
        NextestConfig,
    };
    use camino_tempfile::tempdir;
    use indoc::indoc;
    use nextest_filtering::ParseContext;
    use test_case::test_case;

    #[test_case(
        "",
        Ok(SlowTimeout { period: Duration::from_secs(60), terminate_after: None, grace_period: Duration::from_secs(10) }),
        None

        ; "empty config is expected to use the hardcoded values"
    )]
    #[test_case(
        indoc! {r#"
            [profile.default]
            slow-timeout = "30s"
        "#},
        Ok(SlowTimeout { period: Duration::from_secs(30), terminate_after: None, grace_period: Duration::from_secs(10) }),
        None

        ; "overrides the default profile"
    )]
    #[test_case(
        indoc! {r#"
            [profile.default]
            slow-timeout = "30s"

            [profile.ci]
            slow-timeout = { period = "60s", terminate-after = 3 }
        "#},
        Ok(SlowTimeout { period: Duration::from_secs(30), terminate_after: None, grace_period: Duration::from_secs(10) }),
        Some(SlowTimeout { period: Duration::from_secs(60), terminate_after: Some(NonZeroUsize::new(3).unwrap()), grace_period: Duration::from_secs(10) })

        ; "adds a custom profile 'ci'"
    )]
    #[test_case(
        indoc! {r#"
            [profile.default]
            slow-timeout = { period = "60s", terminate-after = 3 }

            [profile.ci]
            slow-timeout = "30s"
        "#},
        Ok(SlowTimeout { period: Duration::from_secs(60), terminate_after: Some(NonZeroUsize::new(3).unwrap()), grace_period: Duration::from_secs(10) }),
        Some(SlowTimeout { period: Duration::from_secs(30), terminate_after: None, grace_period: Duration::from_secs(10) })

        ; "ci profile uses string notation"
    )]
    #[test_case(
        indoc! {r#"
            [profile.default]
            slow-timeout = { period = "60s", terminate-after = 3, grace-period = "1s" }

            [profile.ci]
            slow-timeout = "30s"
        "#},
        Ok(SlowTimeout { period: Duration::from_secs(60), terminate_after: Some(NonZeroUsize::new(3).unwrap()), grace_period: Duration::from_secs(1) }),
        Some(SlowTimeout { period: Duration::from_secs(30), terminate_after: None, grace_period: Duration::from_secs(10) })

        ; "timeout grace period"
    )]
    #[test_case(
        indoc! {r#"
            [profile.default]
            slow-timeout = { period = "60s" }
        "#},
        Ok(SlowTimeout { period: Duration::from_secs(60), terminate_after: None, grace_period: Duration::from_secs(10) }),
        None

        ; "partial table"
    )]
    #[test_case(
        indoc! {r#"
            [profile.default]
            slow-timeout = { period = "60s", terminate-after = 0 }
        "#},
        Err("original: invalid value: integer `0`, expected a nonzero usize"),
        None

        ; "zero terminate-after should fail"
    )]
    #[test_case(
        indoc! {r#"
            [profile.default]
            slow-timeout = "60s"

            [profile.ci]
            slow-timeout = { terminate-after = 3 }
        "#},
        Err("original: missing field `period`"),
        None

        ; "partial slow-timeout table should error"
    )]
    fn slowtimeout_adheres_to_hierarchy(
        config_contents: &str,
        expected_default: Result<SlowTimeout, &str>,
        maybe_expected_ci: Option<SlowTimeout>,
    ) {
        let workspace_dir = tempdir().unwrap();

        let graph = temp_workspace(workspace_dir.path(), config_contents);

        let pcx = ParseContext::new(&graph);

        let nextest_config_result = NextestConfig::from_sources(
            graph.workspace().root(),
            &pcx,
            None,
            &[][..],
            &Default::default(),
        );

        match expected_default {
            Ok(expected_default) => {
                let nextest_config = nextest_config_result.expect("config file should parse");

                assert_eq!(
                    nextest_config
                        .profile("default")
                        .expect("default profile should exist")
                        .into_evaluatable(&build_platforms())
                        .slow_timeout(),
                    expected_default,
                );

                if let Some(expected_ci) = maybe_expected_ci {
                    assert_eq!(
                        nextest_config
                            .profile("ci")
                            .expect("ci profile should exist")
                            .into_evaluatable(&build_platforms())
                            .slow_timeout(),
                        expected_ci,
                    );
                }
            }

            Err(expected_err_str) => {
                let err_str = format!("{:?}", nextest_config_result.unwrap_err());

                assert!(
                    err_str.contains(expected_err_str),
                    "expected error string not found: {err_str}",
                )
            }
        }
    }
}
