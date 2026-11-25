// SPDX-FileCopyrightText: 2026 René Kijewski <crates.io@k6i.de>
// SPDX-License-Identifier: Apache-2.0

use std::fs::{FileTimes, set_times};
use std::path::Path;
use std::time::{Duration, SystemTime};

use base64_ng::web::FORGIVING;
use directories::ProjectDirs;
use monostate::MustBe;
use serde::{Deserialize, Serialize};
use tokio::fs::{OpenOptions, create_dir_all, metadata, read_to_string, remove_file};
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct StunList {
    pub(crate) sha: String,
    pub(crate) size: u64,
    pub(crate) content: Vec<String>,
}

impl StunList {
    pub(crate) async fn get() -> Result<Self, StunListError> {
        let project_dirs = ProjectDirs::from("de.k6i.nat-test", "k6i.de", "nat-test")
            .ok_or(StunListError::ProjectDirs)?;
        let cache_path = project_dirs.cache_dir().join("always-online-stun.json");

        let current = if let Ok(metadata) = metadata(&cache_path).await
            && metadata.is_file()
            && metadata.len() >= 3
            && let Ok(current) = read_to_string(&cache_path).await
            && let Ok(current) = deserialize(&cache_path, &current)
            && !current.content.is_empty()
        {
            if let Ok(modified) = metadata.modified()
                && let Ok(elapsed) = modified.elapsed()
                && elapsed < MAX_AGE
            {
                used_cache_info(&cache_path);
                return Ok(current);
            }
            Some(current)
        } else {
            None
        };

        let new = Self::fetch().await?;
        new.write_cache(&cache_path, current.as_ref()).await;
        Ok(new)
    }

    async fn fetch() -> Result<Self, StunListError> {
        let url = format!(
            "https://api.github.com/repos/{GH_ORG}/{GH_REPO}/contents/{GH_FILE}?ref={GH_BRANCH}"
        );

        #[derive(Debug, Deserialize)]
        struct FileContent {
            sha: String,
            size: u64,
            content: String,
            #[expect(unused)]
            encoding: MustBe!("base64"),
        }

        let FileContent {
            sha,
            size,
            content,
            encoding: _,
        } = reqwest::Client::new()
            .get(&url)
            .header("User-Agent", USER_AGENT)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let content = FORGIVING
            .decode_to_vec(&content)
            .map_err(StunListError::Base64)?;
        let mut content: Vec<_> = str::from_utf8(&content)
            .map_err(StunListError::Utf8)?
            .lines()
            .filter_map(|line| match line.trim_ascii() {
                "" => None,
                line => Some(line.to_owned()),
            })
            .collect();
        if content.is_empty() {
            return Err(StunListError::EmptyContent);
        }

        content.sort();
        Ok(StunList { sha, size, content })
    }

    async fn write_cache(&self, cache_path: &Path, current: Option<&Self>) {
        #[derive(pretty_error_debug::Debug, thiserror::Error)]
        enum CacheError<'a> {
            #[error("Could not update mtime of cache file `{}`", .0.display())]
            Touch(&'a Path, #[source] std::io::Error),
            #[error("Could not create cache directory `{}`", .0.display())]
            Mkdir(&'a Path, #[source] std::io::Error),
            #[error("Could not create or open cache file `{}`", .0.display())]
            Create(&'a Path, #[source] std::io::Error),
            #[error("Could not serialize cache file `{}`", .0.display())]
            Serialize(&'a Path, #[source] serde_json::Error),
            #[error("Could write cache file `{}`", .0.display())]
            Write(&'a Path, #[source] std::io::Error),
            #[error("Could delete cache file `{}`", .0.display())]
            Delete(&'a Path, #[source] std::io::Error),
        }

        if Some(self) == current {
            if let Err(err) =
                set_times(cache_path, FileTimes::new().set_modified(SystemTime::now()))
            {
                warn!("{:?}", CacheError::Touch(cache_path, err));
            }
            used_cache_info(cache_path);
            return;
        }

        let mut content = match serde_json::to_string_pretty(self) {
            Ok(content) => content,
            Err(err) => {
                warn!("{:?}", CacheError::Serialize(cache_path, err));
                return;
            }
        };
        content.push('\n');

        if let Some(parent) = cache_path.parent()
            && let Err(err) = create_dir_all(&parent).await
        {
            warn!("{:?}", CacheError::Mkdir(parent, err));
        }
        let cache = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(cache_path)
            .await;
        let mut cache = match cache {
            Ok(cache) => cache,
            Err(err) => {
                warn!("{:?}", CacheError::Create(cache_path, err));
                return;
            }
        };
        if let Err(err) = cache.write_all(content.as_bytes()).await {
            warn!("{:?}", CacheError::Write(cache_path, err));
            if let Err(err) = remove_file(cache_path).await {
                warn!("{:?}", CacheError::Delete(cache_path, err));
            }
            return;
        }

        info!(
            "Cached `{}` data in `{}`.",
            GH_REPO.escape_debug(),
            cache_path.display()
        );
    }
}

#[track_caller]
fn deserialize(cache_path: &Path, current: &str) -> Result<StunList, ()> {
    #[derive(pretty_error_debug::Debug, thiserror::Error)]
    #[error("Could not deserialize cache file `{}`", .0.display())]
    struct DeserializeError<'a, E: std::error::Error + 'static>(
        &'a Path,
        #[source] serde_path_to_error::Error<E>,
    );

    let mut deserializer = serde_json_lenient::de::Deserializer::from_slice(current.as_bytes());
    match serde_path_to_error::deserialize(&mut deserializer) {
        Ok(result) => Ok(result),
        Err(err) => {
            warn!("{:?}", DeserializeError(cache_path, err));
            Err(())
        }
    }
}

#[track_caller]
fn used_cache_info(cache_path: &Path) {
    info!(
        "Used cached `{}` data in `{}`.",
        GH_REPO.escape_debug(),
        cache_path.display()
    );
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum StunListError {
    #[error("Could not determine project directories")]
    ProjectDirs,
    #[error(
        "Could not fetch `{}`'s `{}`",
        GH_REPO.escape_debug(),
        GH_FILE.escape_debug(),
    )]
    Fetch(#[from] reqwest::Error),
    #[error(
        "Could not decode `{}`'s `{}` JSON content",
        GH_REPO.escape_debug(),
        GH_FILE.escape_debug(),
    )]
    Base64(#[from] base64_ng::web::ForgivingError),
    #[error(
        "Could not decode `{}`'s `{}` base64 content",
        GH_REPO.escape_debug(),
        GH_FILE.escape_debug(),
    )]
    Utf8(#[from] std::str::Utf8Error),
    #[error("GitHub content is empty")]
    EmptyContent,
}

const MAX_AGE: Duration = Duration::from_hours(24);
const USER_AGENT: &str = "nat-test (https://github.com/Kijewski/nat-test)";
const GH_ORG: &str = "pradt2";
const GH_REPO: &str = "always-online-stun";
const GH_BRANCH: &str = "master";
const GH_FILE: &str = "valid_hosts.txt";
