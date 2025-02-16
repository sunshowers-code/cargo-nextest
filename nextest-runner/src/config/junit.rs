// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;

/// Global JUnit configuration stored within a profile.
///
/// Returned by an [`EvaluatableProfile`](crate::config::EvaluatableProfile).
#[derive(Clone, Debug)]
pub struct JunitConfig<'cfg> {
    path: Utf8PathBuf,
    report_name: &'cfg str,
    store_success_output: bool,
    store_failure_output: bool,
}

impl<'cfg> JunitConfig<'cfg> {
    pub(super) fn new(
        custom_data: Option<&'cfg JunitImpl>,
        default_data: &'cfg DefaultJunitImpl,
    ) -> Option<Self> {
        let path = custom_data
            .map(|custom| &custom.path)
            .unwrap_or(&default_data.path)
            .as_deref();

        path.map(|path| {
            let report_name = custom_data
                .and_then(|custom| custom.report_name.as_deref())
                .unwrap_or(&default_data.report_name);
            let store_success_output = custom_data
                .and_then(|custom| custom.store_success_output)
                .unwrap_or(default_data.store_success_output);
            let store_failure_output = custom_data
                .and_then(|custom| custom.store_failure_output)
                .unwrap_or(default_data.store_failure_output);
            Self {
                path: path.to_owned(),
                report_name,
                store_success_output,
                store_failure_output,
            }
        })
    }

    /// Returns the absolute path to the JUnit report.
    pub fn path(&self, store_dir: &Utf8Path) -> Utf8PathBuf {
        store_dir.join(&self.path)
    }

    /// Returns the name of the JUnit report.
    pub fn report_name(&self) -> &'cfg str {
        self.report_name
    }

    /// Returns true if success output should be stored.
    pub fn store_success_output(&self) -> bool {
        self.store_success_output
    }

    /// Returns true if failure output should be stored.
    pub fn store_failure_output(&self) -> bool {
        self.store_failure_output
    }
}

#[derive(Clone, Debug)]
pub(super) struct DefaultJunitImpl {
    path: Option<Utf8PathBuf>,
    report_name: String,
    store_success_output: bool,
    store_failure_output: bool,
}

impl DefaultJunitImpl {
    // Default values have all fields defined on them.
    pub(crate) fn for_default_profile(data: JunitImpl) -> Self {
        DefaultJunitImpl {
            path: data.path,
            report_name: data
                .report_name
                .expect("junit.report present in default profile"),
            store_success_output: data
                .store_success_output
                .expect("junit.store-success-output present in default profile"),
            store_failure_output: data
                .store_failure_output
                .expect("junit.store-failure-output present in default profile"),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) struct JunitImpl {
    #[serde(default)]
    path: Option<Utf8PathBuf>,
    #[serde(default)]
    report_name: Option<String>,
    #[serde(default)]
    store_success_output: Option<bool>,
    #[serde(default)]
    store_failure_output: Option<bool>,
}
