// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::config::helpers::deserialize_relative_path;
use camino::{Utf8Path, Utf8PathBuf};
use serde::{
    de::{self, Visitor},
    Deserialize, Deserializer,
};
use std::fmt;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) struct StoreConfigImpl {
    dir: StoreDir,
}

impl StoreConfigImpl {
    pub(crate) fn resolve_store_dir(
        &self,
        workspace_root: &Utf8Path,
        target_dir: &Utf8Path,
    ) -> Utf8PathBuf {
        match &self.dir {
            StoreDir::Path(path) => workspace_root.join(path),
            StoreDir::RelativeTo { dir, relative_to } => match relative_to {
                StoreRelativeTo::WorkspaceRoot => workspace_root.join(dir),
                StoreRelativeTo::TargetDir => target_dir.join(dir),
            },
        }
    }
}

#[derive(Clone, Debug)]
enum StoreDir {
    Path(Utf8PathBuf),
    RelativeTo {
        dir: Utf8PathBuf,
        relative_to: StoreRelativeTo,
    },
}

impl<'de> Deserialize<'de> for StoreDir {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct V;

        impl<'de2> Visitor<'de2> for V {
            type Value = StoreDir;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str(
                    "a path relative to the workspace root, \
                     or a map: { path = \"nextest\", relative-to = \"target\" }",
                )
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StoreDir::Path(v.into()))
            }

            fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
            where
                A: de::MapAccess<'de2>,
            {
                let de = de::value::MapAccessDeserializer::new(map);
                let map = StoreDirMap::deserialize(de)?;
                Ok(StoreDir::RelativeTo {
                    dir: map.path,
                    relative_to: map.relative_to,
                })
            }
        }

        deserializer.deserialize_any(V)
    }
}

/// A deserializer for `{ path = "nextest", relative-to = "target" }`.
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct StoreDirMap {
    #[serde(deserialize_with = "deserialize_relative_path")]
    path: Utf8PathBuf,
    relative_to: StoreRelativeTo,
}

/// A deserializer for store.dir.relative-to.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum StoreRelativeTo {
    WorkspaceRoot,
    TargetDir,
}
