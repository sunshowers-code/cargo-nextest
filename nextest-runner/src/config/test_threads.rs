// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::get_num_cpus;
use crate::errors::TestThreadsParseError;
use serde::Deserialize;
use std::{cmp::Ordering, fmt, str::FromStr};

/// Type for the test-threads config key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TestThreads {
    /// Run tests with a specified number of threads.
    Count(usize),

    /// Run tests with a number of threads equal to the logical CPU count.
    NumCpus,
}

impl TestThreads {
    /// Gets the actual number of test threads computed at runtime.
    pub fn compute(self) -> usize {
        match self {
            Self::Count(threads) => threads,
            Self::NumCpus => get_num_cpus(),
        }
    }
}

impl FromStr for TestThreads {
    type Err = TestThreadsParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "num-cpus" {
            return Ok(Self::NumCpus);
        }

        match s.parse::<isize>() {
            Err(e) => Err(TestThreadsParseError::new(format!(
                "Error: {e} parsing {s}"
            ))),
            Ok(0) => Err(TestThreadsParseError::new("jobs may not be 0")),
            Ok(j) if j < 0 => Ok(TestThreads::Count(
                (get_num_cpus() as isize + j).max(1) as usize
            )),
            Ok(j) => Ok(TestThreads::Count(j as usize)),
        }
    }
}

impl fmt::Display for TestThreads {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Count(threads) => write!(f, "{threads}"),
            Self::NumCpus => write!(f, "num-cpus"),
        }
    }
}

impl<'de> Deserialize<'de> for TestThreads {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct V;

        impl serde::de::Visitor<'_> for V {
            type Value = TestThreads;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                write!(formatter, "an integer or the string \"num-cpus\"")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if v == "num-cpus" {
                    Ok(TestThreads::NumCpus)
                } else {
                    Err(serde::de::Error::invalid_value(
                        serde::de::Unexpected::Str(v),
                        &self,
                    ))
                }
            }

            // Note that TOML uses i64, not u64.
            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match v.cmp(&0) {
                    Ordering::Greater => Ok(TestThreads::Count(v as usize)),
                    Ordering::Less => Ok(TestThreads::Count(
                        (get_num_cpus() as i64 + v).max(1) as usize
                    )),
                    Ordering::Equal => Err(serde::de::Error::invalid_value(
                        serde::de::Unexpected::Signed(v),
                        &self,
                    )),
                }
            }
        }

        deserializer.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{test_helpers::*, NextestConfig};
    use camino_tempfile::tempdir;
    use indoc::indoc;
    use nextest_filtering::ParseContext;
    use test_case::test_case;

    #[test_case(
        indoc! {r#"
            [profile.custom]
            test-threads = -1
        "#},
        Some(get_num_cpus() - 1)

        ; "negative"
    )]
    #[test_case(
        indoc! {r#"
            [profile.custom]
            test-threads = 2
        "#},
        Some(2)

        ; "positive"
    )]
    #[test_case(
        indoc! {r#"
            [profile.custom]
            test-threads = 0
        "#},
        None

        ; "zero"
    )]
    #[test_case(
        indoc! {r#"
            [profile.custom]
            test-threads = "num-cpus"
        "#},
        Some(get_num_cpus())

        ; "num-cpus"
    )]
    fn parse_test_threads(config_contents: &str, n_threads: Option<usize>) {
        let workspace_dir = tempdir().unwrap();

        let graph = temp_workspace(workspace_dir.path(), config_contents);

        let pcx = ParseContext::new(&graph);
        let config = NextestConfig::from_sources(
            graph.workspace().root(),
            &pcx,
            None,
            [],
            &Default::default(),
        );
        match n_threads {
            None => assert!(config.is_err()),
            Some(n) => assert_eq!(
                config
                    .unwrap()
                    .profile("custom")
                    .unwrap()
                    .into_evaluatable(&build_platforms())
                    .custom_profile()
                    .unwrap()
                    .test_threads()
                    .unwrap()
                    .compute(),
                n,
            ),
        }
    }
}
