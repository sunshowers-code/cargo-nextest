// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::errors::MaxFailParseError;
use serde::Deserialize;
use std::{fmt, str::FromStr};

/// Type for the max-fail flag and fail-fast configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MaxFail {
    /// Allow a specific number of tests to fail before exiting.
    Count(usize),

    /// Run all tests. Equivalent to --no-fast-fail.
    All,
}

impl MaxFail {
    /// Returns the max-fail corresponding to the fail-fast.
    pub fn from_fail_fast(fail_fast: bool) -> Self {
        if fail_fast {
            Self::Count(1)
        } else {
            Self::All
        }
    }

    /// Returns true if the max-fail has been exceeded.
    pub fn is_exceeded(&self, failed: usize) -> bool {
        match self {
            Self::Count(n) => failed >= *n,
            Self::All => false,
        }
    }
}

impl FromStr for MaxFail {
    type Err = MaxFailParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.to_lowercase() == "all" {
            return Ok(Self::All);
        }

        match s.parse::<isize>() {
            Err(e) => Err(MaxFailParseError::new(format!("Error: {e} parsing {s}"))),
            Ok(j) if j <= 0 => Err(MaxFailParseError::new("max-fail may not be <= 0")),
            Ok(j) => Ok(MaxFail::Count(j as usize)),
        }
    }
}

impl fmt::Display for MaxFail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::All => write!(f, "all"),
            Self::Count(n) => write!(f, "{n}"),
        }
    }
}

/// Deserializes a fail-fast configuration.
pub(super) fn deserialize_fail_fast<'de, D>(deserializer: D) -> Result<Option<MaxFail>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct V;

    impl<'de2> serde::de::Visitor<'de2> for V {
        type Value = Option<MaxFail>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            write!(formatter, "a boolean or {{ max-fail = ... }}")
        }

        fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(Some(MaxFail::from_fail_fast(v)))
        }

        fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de2>,
        {
            let de = serde::de::value::MapAccessDeserializer::new(map);
            FailFastMap::deserialize(de).map(|helper| Some(helper.max_fail))
        }
    }

    deserializer.deserialize_any(V)
}

/// A deserializer for `{ max-fail = xyz }`.
#[derive(Deserialize)]
struct FailFastMap {
    #[serde(rename = "max-fail", deserialize_with = "deserialize_max_fail")]
    max_fail: MaxFail,
}

fn deserialize_max_fail<'de, D>(deserializer: D) -> Result<MaxFail, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct V;

    impl serde::de::Visitor<'_> for V {
        type Value = MaxFail;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            write!(formatter, "a positive integer or the string \"all\"")
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            if v == "all" {
                return Ok(MaxFail::All);
            }

            // If v is a string that represents a number, suggest using the
            // integer form.
            if let Ok(val) = v.parse::<i64>() {
                if val > 0 {
                    return Err(serde::de::Error::invalid_value(
                        serde::de::Unexpected::Str(v),
                        &"the string \"all\" (numbers must be specified without quotes)",
                    ));
                } else {
                    return Err(serde::de::Error::invalid_value(
                        serde::de::Unexpected::Str(v),
                        &"the string \"all\" (numbers must be positive and without quotes)",
                    ));
                }
            }

            Err(serde::de::Error::invalid_value(
                serde::de::Unexpected::Str(v),
                &"the string \"all\" or a positive integer",
            ))
        }

        fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            if v > 0 {
                Ok(MaxFail::Count(v as usize))
            } else {
                Err(serde::de::Error::invalid_value(
                    serde::de::Unexpected::Signed(v),
                    &"a positive integer or the string \"all\"",
                ))
            }
        }
    }

    deserializer.deserialize_any(V)
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
    use camino_tempfile::tempdir;
    use indoc::indoc;
    use nextest_filtering::ParseContext;
    use test_case::test_case;

    #[test]
    fn maxfail_builder_from_str() {
        let successes = vec![
            ("all", MaxFail::All),
            ("ALL", MaxFail::All),
            ("1", MaxFail::Count(1)),
        ];

        let failures = vec!["-1", "0", "foo"];

        for (input, output) in successes {
            assert_eq!(
                MaxFail::from_str(input).unwrap_or_else(|err| panic!(
                    "expected input '{input}' to succeed, failed with: {err}"
                )),
                output,
                "success case '{input}' matches",
            );
        }

        for input in failures {
            MaxFail::from_str(input).expect_err(&format!("expected input '{input}' to fail"));
        }
    }

    #[test_case(
        indoc! {r#"
            [profile.custom]
            fail-fast = true
        "#},
        MaxFail::Count(1)
        ; "boolean true"
    )]
    #[test_case(
        indoc! {r#"
            [profile.custom]
            fail-fast = false
        "#},
        MaxFail::All
        ; "boolean false"
    )]
    #[test_case(
        indoc! {r#"
            [profile.custom]
            fail-fast = { max-fail = 1 }
        "#},
        MaxFail::Count(1)
        ; "max-fail 1"
    )]
    #[test_case(
        indoc! {r#"
            [profile.custom]
            fail-fast = { max-fail = 2 }
        "#},
        MaxFail::Count(2)
        ; "max-fail 2"
    )]
    #[test_case(
        indoc! {r#"
            [profile.custom]
            fail-fast = { max-fail = "all" }
        "#},
        MaxFail::All
        ; "max-fail all"
    )]
    fn parse_fail_fast(config_contents: &str, expected: MaxFail) {
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
        .expect("expected parsing to succeed");

        let profile = config
            .profile("custom")
            .unwrap()
            .into_evaluatable(&build_platforms());

        assert_eq!(profile.max_fail(), expected);
    }

    #[test_case(
        indoc! {r#"
            [profile.custom]
            fail-fast = { max-fail = 0 }
        "#},
        "profile.custom.fail-fast.max-fail: invalid value: integer `0`, expected a positive integer or the string \"all\""
        ; "invalid zero max-fail"
    )]
    #[test_case(
        indoc! {r#"
            [profile.custom]
            fail-fast = { max-fail = -1 }
        "#},
        "profile.custom.fail-fast.max-fail: invalid value: integer `-1`, expected a positive integer or the string \"all\""
        ; "invalid negative max-fail"
    )]
    #[test_case(
        indoc! {r#"
            [profile.custom]
            fail-fast = { max-fail = "" }
        "#},
        "profile.custom.fail-fast.max-fail: invalid value: string \"\", expected the string \"all\" or a positive integer"
        ; "empty string max-fail"
    )]
    #[test_case(
        indoc! {r#"
            [profile.custom]
            fail-fast = { max-fail = "1" }
        "#},
        "profile.custom.fail-fast.max-fail: invalid value: string \"1\", expected the string \"all\" (numbers must be specified without quotes)"
        ; "string as positive integer"
    )]
    #[test_case(
        indoc! {r#"
            [profile.custom]
            fail-fast = { max-fail = "0" }
        "#},
        "profile.custom.fail-fast.max-fail: invalid value: string \"0\", expected the string \"all\" (numbers must be positive and without quotes)"
        ; "zero string"
    )]
    #[test_case(
        indoc! {r#"
            [profile.custom]
            fail-fast = { max-fail = "invalid" }
        "#},
        "profile.custom.fail-fast.max-fail: invalid value: string \"invalid\", expected the string \"all\" or a positive integer"
        ; "invalid string max-fail"
    )]
    #[test_case(
        indoc! {r#"
            [profile.custom]
            fail-fast = { max-fail = true }
        "#},
        "profile.custom.fail-fast.max-fail: invalid type: boolean `true`, expected a positive integer or the string \"all\""
        ; "invalid max-fail type"
    )]
    #[test_case(
        indoc! {r#"
            [profile.custom]
            fail-fast = { invalid-key = 1 }
        "#},
        "profile.custom.fail-fast: missing field `max-fail`"
        ; "invalid map key"
    )]
    #[test_case(
        indoc! {r#"
            [profile.custom]
            fail-fast = "true"
        "#},
        "profile.custom.fail-fast: invalid type: string \"true\", expected a boolean or { max-fail = ... }"
        ; "string boolean not allowed"
    )]
    fn invalid_fail_fast(config_contents: &str, error_str: &str) {
        let workspace_dir = tempdir().unwrap();
        let graph = temp_workspace(workspace_dir.path(), config_contents);
        let pcx = ParseContext::new(&graph);

        let error = NextestConfig::from_sources(
            graph.workspace().root(),
            &pcx,
            None,
            [],
            &Default::default(),
        )
        .expect_err("expected parsing to fail");

        let error = match error.kind() {
            ConfigParseErrorKind::DeserializeError(d) => d,
            _ => panic!("expected deserialize error, found {error:?}"),
        };

        assert_eq!(
            error.to_string(),
            error_str,
            "actual error matches expected"
        );
    }
}
