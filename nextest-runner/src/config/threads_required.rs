// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::get_num_cpus;
use serde::Deserialize;
use std::{cmp::Ordering, fmt};

/// Type for the threads-required config key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadsRequired {
    /// Take up "slots" equal to the number of threads.
    Count(usize),

    /// Take up as many slots as the number of CPUs.
    NumCpus,

    /// Take up as many slots as the number of test threads specified.
    NumTestThreads,
}

impl ThreadsRequired {
    /// Gets the actual number of test threads computed at runtime.
    pub fn compute(self, test_threads: usize) -> usize {
        match self {
            Self::Count(threads) => threads,
            Self::NumCpus => get_num_cpus(),
            Self::NumTestThreads => test_threads,
        }
    }
}

impl<'de> Deserialize<'de> for ThreadsRequired {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct V;

        impl serde::de::Visitor<'_> for V {
            type Value = ThreadsRequired;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                write!(
                    formatter,
                    "an integer, the string \"num-cpus\" or the string \"num-test-threads\""
                )
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if v == "num-cpus" {
                    Ok(ThreadsRequired::NumCpus)
                } else if v == "num-test-threads" {
                    Ok(ThreadsRequired::NumTestThreads)
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
                    Ordering::Greater => Ok(ThreadsRequired::Count(v as usize)),
                    // TODO: we don't currently support negative numbers here because it's not clear
                    // whether num-cpus or num-test-threads is better. It would probably be better
                    // to support a small expression syntax with +, -, * and /.
                    //
                    // I (Rain) checked out a number of the expression syntax crates and found that they
                    // either support too much or too little. We want just this minimal set of operators,
                    // plus. Probably worth just forking https://docs.rs/mexe or working with upstream
                    // to add support for operators.
                    Ordering::Equal | Ordering::Less => Err(serde::de::Error::invalid_value(
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
            threads-required = 2
        "#},
        Some(2)

        ; "positive"
    )]
    #[test_case(
        indoc! {r#"
            [profile.custom]
            threads-required = 0
        "#},
        None

        ; "zero"
    )]
    #[test_case(
        indoc! {r#"
            [profile.custom]
            threads-required = -1
        "#},
        None

        ; "negative"
    )]
    #[test_case(
        indoc! {r#"
            [profile.custom]
            threads-required = "num-cpus"
        "#},
        Some(get_num_cpus())

        ; "num-cpus"
    )]
    #[test_case(
        indoc! {r#"
            [profile.custom]
            test-threads = 1
            threads-required = "num-cpus"
        "#},
        Some(get_num_cpus())

        ; "num-cpus-with-custom-test-threads"
    )]
    #[test_case(
        indoc! {r#"
            [profile.custom]
            threads-required = "num-test-threads"
        "#},
        Some(get_num_cpus())

        ; "num-test-threads"
    )]
    #[test_case(
        indoc! {r#"
            [profile.custom]
            test-threads = 1
            threads-required = "num-test-threads"
        "#},
        Some(1)

        ; "num-test-threads-with-custom-test-threads"
    )]
    fn parse_threads_required(config_contents: &str, threads_required: Option<usize>) {
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
        match threads_required {
            None => assert!(config.is_err()),
            Some(t) => {
                let config = config.unwrap();
                let profile = config
                    .profile("custom")
                    .unwrap()
                    .into_evaluatable(&build_platforms());

                let test_threads = profile.test_threads().compute();
                let threads_required = profile.threads_required().compute(test_threads);
                assert_eq!(threads_required, t)
            }
        }
    }
}
