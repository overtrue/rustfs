// Copyright 2024 RustFS Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    io,
    path::{Component, Path},
};

use super::error::Result;
use crate::disk::error_conv::to_file_error;
use rustfs_utils::path::SLASH_SEPARATOR;
use tokio::fs;
use tracing::warn;

use super::error::DiskError;

/// Check path length according to OS limits.
pub fn check_path_length(path_name: &str) -> Result<()> {
    // Apple OS X path length is limited to 1016
    if cfg!(target_os = "macos") && path_name.len() > 1016 {
        return Err(DiskError::FileNameTooLong);
    }

    // Disallow more than 1024 characters on windows, there
    // are no known name_max limits on Windows.
    if cfg!(target_os = "windows") && path_name.len() > 1024 {
        return Err(DiskError::FileNameTooLong);
    }

    // On Unix we reject paths if they are just '.', '..' or '/'
    let invalid_paths = [".", "..", "/"];
    if invalid_paths.contains(&path_name) {
        return Err(DiskError::FileAccessDenied);
    }

    // Check each path segment length is > 255 on all Unix
    // platforms, look for this value as NAME_MAX in
    // /usr/include/linux/limits.h
    let mut count = 0usize;
    for c in path_name.chars() {
        match c {
            '/' => count = 0,
            '\\' if cfg!(target_os = "windows") => count = 0, // Reset
            _ => {
                count += 1;
                if count > 255 {
                    return Err(DiskError::FileNameTooLong);
                }
            }
        }
    }

    // Success.
    Ok(())
}

/// Check if the given disk path is the root disk.
/// On Windows, always return false.
/// On Unix, compare the disk paths.
#[tracing::instrument(level = "debug", skip_all)]
pub fn is_root_disk(disk_path: &str, root_disk: &str) -> Result<bool> {
    if cfg!(target_os = "windows") {
        return Ok(false);
    }

    rustfs_utils::os::same_disk(disk_path, root_disk).map_err(|e| to_file_error(e).into())
}

/// Create a directory and all its parent components if they are missing.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn make_dir_all(path: impl AsRef<Path>, base_dir: impl AsRef<Path>) -> Result<()> {
    check_path_length(path.as_ref().to_string_lossy().to_string().as_str())?;

    reliable_mkdir_all(path.as_ref(), base_dir.as_ref())
        .await
        .map_err(to_file_error)?;

    Ok(())
}

/// Check if a directory is empty.
/// Only reads one entry to determine if the directory is empty.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn is_empty_dir(path: impl AsRef<Path>) -> bool {
    read_dir(path.as_ref(), 1).await.is_ok_and(|v| v.is_empty())
}

/// Check if an object directory contains subdirectories that may represent nested objects.
/// Returns true if the directory is "empty" (only contains xl.meta and/or data directories),
/// meaning no nested object directories exist.
/// This is used when an object key might be a prefix of another object
/// (e.g., both "foo/bar" and "foo/bar/xyzzy" exist as objects).
#[tracing::instrument(level = "debug", skip_all)]
pub async fn is_empty_dir_except_xlmeta(path: impl AsRef<Path>) -> bool {
    match read_dir(path.as_ref(), 0).await {
        Ok(entries) => {
            // Check if there are any subdirectories that are NOT data directories (UUID-based).
            // Data directories have names that are valid UUIDs, while nested objects have
            // human-readable names that wouldn't be valid UUIDs.
            !entries.iter().any(|name| {
                // Only consider directories (entries ending with /)
                if !name.ends_with(SLASH_SEPARATOR) {
                    return false;
                }
                // Get the directory name without trailing slash
                let dir_name = name.trim_end_matches(SLASH_SEPARATOR);
                // If it's a valid UUID, it's a data directory, not a nested object
                uuid::Uuid::parse_str(dir_name).is_err()
            })
        }
        Err(_) => true,
    }
}

// read_dir  count read limit. when count == 0 unlimit.
/// Return file names in the directory.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn read_dir(path: impl AsRef<Path>, count: i32) -> std::io::Result<Vec<String>> {
    let mut entries = fs::read_dir(path.as_ref()).await?;

    let mut volumes = Vec::new();

    let mut count = count;

    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();

        if name.is_empty() || name == "." || name == ".." {
            continue;
        }

        let file_type = entry.file_type().await?;

        if file_type.is_file() {
            volumes.push(name);
        } else if file_type.is_dir() {
            volumes.push(format!("{name}{SLASH_SEPARATOR}"));
        }
        count -= 1;
        if count == 0 {
            break;
        }
    }

    Ok(volumes)
}

#[tracing::instrument(level = "debug", skip_all)]
pub async fn rename_all(
    src_file_path: impl AsRef<Path>,
    dst_file_path: impl AsRef<Path>,
    base_dir: impl AsRef<Path>,
) -> Result<()> {
    reliable_rename(src_file_path, dst_file_path.as_ref(), base_dir)
        .await
        .map_err(to_file_error)?;

    Ok(())
}

async fn reliable_rename(
    src_file_path: impl AsRef<Path>,
    dst_file_path: impl AsRef<Path>,
    base_dir: impl AsRef<Path>,
) -> io::Result<()> {
    if let Some(parent) = dst_file_path.as_ref().parent()
        && !file_exists(parent)
    {
        // info!("reliable_rename reliable_mkdir_all parent: {:?}", parent);
        reliable_mkdir_all(parent, base_dir.as_ref()).await?;
    }

    let mut i = 0;
    loop {
        if let Err(e) = super::fs::rename_std(src_file_path.as_ref(), dst_file_path.as_ref()) {
            if e.kind() == io::ErrorKind::NotFound {
                break;
            }

            if i == 0 {
                i += 1;
                continue;
            }
            warn!(
                "reliable_rename failed. src_file_path: {:?}, dst_file_path: {:?}, base_dir: {:?}, err: {:?}",
                src_file_path.as_ref(),
                dst_file_path.as_ref(),
                base_dir.as_ref(),
                e
            );
            return Err(e);
        }

        break;
    }

    Ok(())
}

pub async fn reliable_mkdir_all(path: impl AsRef<Path>, base_dir: impl AsRef<Path>) -> io::Result<()> {
    let mut i = 0;

    let mut base_dir = base_dir.as_ref();
    loop {
        if let Err(e) = os_mkdir_all(path.as_ref(), base_dir).await {
            if e.kind() == io::ErrorKind::NotFound && i == 0 {
                i += 1;

                if let Some(base_parent) = base_dir.parent()
                    && let Some(c) = base_parent.components().next()
                    && c != Component::RootDir
                {
                    base_dir = base_parent
                }
                continue;
            }

            return Err(e);
        }

        break;
    }

    Ok(())
}

/// Create a directory and all its parent components if they are missing.
/// Without recursion support, fall back to create_dir_all
/// This function will not create directories under base_dir.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn os_mkdir_all(dir_path: impl AsRef<Path>, base_dir: impl AsRef<Path>) -> io::Result<()> {
    if !base_dir.as_ref().to_string_lossy().is_empty() && base_dir.as_ref().starts_with(dir_path.as_ref()) {
        return Ok(());
    }

    if let Some(parent) = dir_path.as_ref().parent() {
        // Without recursion support, fall back to create_dir_all
        if let Err(e) = super::fs::make_dir_all(&parent).await {
            if e.kind() == io::ErrorKind::AlreadyExists {
                return Ok(());
            }

            return Err(e);
        }
        // Box::pin(os_mkdir_all(&parent, &base_dir)).await?;
    }

    if let Err(e) = super::fs::mkdir(dir_path.as_ref()).await {
        if e.kind() == io::ErrorKind::AlreadyExists {
            return Ok(());
        }

        return Err(e);
    }

    Ok(())
}

/// Check if a file exists.
/// Returns true if the file exists, false otherwise.
#[tracing::instrument(level = "debug", skip_all)]
pub fn file_exists(path: impl AsRef<Path>) -> bool {
    std::fs::metadata(path.as_ref()).map(|_| true).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::fs;

    /// Test is_empty_dir_except_xlmeta for directories with various contents.
    /// This function is used to detect if an object directory contains nested objects.
    #[tokio::test]
    async fn test_is_empty_dir_except_xlmeta() {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path();

        // Test 1: Empty directory should be considered "empty"
        let empty_dir = base_path.join("empty");
        fs::create_dir(&empty_dir).await.unwrap();
        assert!(is_empty_dir_except_xlmeta(&empty_dir).await, "Empty directory should return true");

        // Test 2: Directory with only xl.meta should be considered "empty"
        let object_dir = base_path.join("object");
        fs::create_dir(&object_dir).await.unwrap();
        fs::write(object_dir.join("xl.meta"), "metadata").await.unwrap();
        assert!(
            is_empty_dir_except_xlmeta(&object_dir).await,
            "Directory with only xl.meta should return true"
        );

        // Test 3: Directory with xl.meta and a UUID data directory should be "empty"
        // (data directories use UUID names)
        let object_with_data = base_path.join("object_with_data");
        fs::create_dir(&object_with_data).await.unwrap();
        fs::write(object_with_data.join("xl.meta"), "metadata").await.unwrap();
        let data_dir = object_with_data.join("550e8400-e29b-41d4-a716-446655440000");
        fs::create_dir(&data_dir).await.unwrap();
        assert!(
            is_empty_dir_except_xlmeta(&object_with_data).await,
            "Directory with xl.meta and UUID data dir should return true"
        );

        // Test 4: Directory with xl.meta and a non-UUID subdirectory (nested object)
        // should NOT be considered "empty"
        let object_with_nested = base_path.join("object_with_nested");
        fs::create_dir(&object_with_nested).await.unwrap();
        fs::write(object_with_nested.join("xl.meta"), "metadata").await.unwrap();
        let nested_dir = object_with_nested.join("xyzzy");
        fs::create_dir(&nested_dir).await.unwrap();
        assert!(
            !is_empty_dir_except_xlmeta(&object_with_nested).await,
            "Directory with xl.meta and non-UUID subdir should return false"
        );

        // Test 5: Non-existent directory should be considered "empty" (error case)
        let non_existent = base_path.join("non_existent");
        assert!(
            is_empty_dir_except_xlmeta(&non_existent).await,
            "Non-existent directory should return true (error case)"
        );
    }

    /// Test that UUID-named directories are correctly identified as data directories
    #[tokio::test]
    async fn test_uuid_directory_detection() {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path();

        let object_dir = base_path.join("object");
        fs::create_dir(&object_dir).await.unwrap();
        fs::write(object_dir.join("xl.meta"), "metadata").await.unwrap();

        // Various UUID formats that should be recognized as data directories
        let uuid_dirs = [
            "550e8400-e29b-41d4-a716-446655440000",
            "00000000-0000-0000-0000-000000000000",
            "ffffffff-ffff-ffff-ffff-ffffffffffff",
        ];

        for uuid_name in uuid_dirs {
            let uuid_dir = object_dir.join(uuid_name);
            fs::create_dir(&uuid_dir).await.unwrap();
        }

        // Should still be "empty" because all subdirectories are UUID data directories
        assert!(
            is_empty_dir_except_xlmeta(&object_dir).await,
            "Directory with only UUID data directories should return true"
        );

        // Now add a non-UUID directory (representing a nested object)
        let nested_object = object_dir.join("nested");
        fs::create_dir(&nested_object).await.unwrap();

        // Should now NOT be "empty" because there's a non-UUID subdirectory
        assert!(
            !is_empty_dir_except_xlmeta(&object_dir).await,
            "Directory with non-UUID subdirectory should return false"
        );
    }
}
