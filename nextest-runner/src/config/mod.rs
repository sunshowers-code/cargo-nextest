// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Configuration support for nextest.
//!
//! ## Multi-pass parsing
//!
//! Nextest's configuration parsing happens in several passes, similar to a
//! typical compiler.
//!
//! * The first pass verifies that the configuration looks fine and is done
//!   very early in the process. A successful first phase parse is represented by
//!   [`EarlyProfile`].
//! * The second pass applies the host and target platforms to the configuration,
//!   resulting in an [`EvaluatableProfile`].
//! * The final pass resolves actual per-test settings, via [`TestSettings`].
//!
//! Multi-pass parsing allows for profile parsing errors to be returned as early
//! as possible -- before the host and target platforms are known. Returning
//! errors early leads to a better user experience.

mod archive;
mod config_impl;
mod helpers;
mod identifier;
mod junit;
mod max_fail;
mod nextest_version;
mod overrides;
mod priority;
mod retry_policy;
mod scripts;
mod slow_timeout;
mod store;
mod test_group;
mod test_threads;
mod threads_required;
mod tool_config;
mod track_default;

pub use archive::*;
pub use config_impl::*;
pub use identifier::*;
pub use junit::*;
pub use max_fail::*;
pub use nextest_version::*;
pub use overrides::*;
pub use priority::*;
pub use retry_policy::*;
pub(super) use scripts::*;
pub use slow_timeout::*;
pub use test_group::*;
pub use test_threads::*;
pub use threads_required::*;
pub use tool_config::*;
pub(super) use track_default::*;

#[cfg(test)]
mod test_helpers;
