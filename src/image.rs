use anyhow::Context;
use std::path::{Component, Path, PathBuf};

/// Safely extracts a rootfs tar.gz archive into `destination`.
///
/// Validates that no entries escape `destination` through lexical traversal or symlinks,
/// and rejects special files (devices, FIFOs, hard links).
pub fn extract_rootfs(image: &Path, destination: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(destination)
        .with_context(|| format!("creating destination directory {}", destination.display()))?;
    let file =
        std::fs::File::open(image).with_context(|| format!("opening image {}", image.display()))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    archive.set_preserve_permissions(true);

    for entry in archive.entries().context("reading archive entries")? {
        let mut entry = entry.context("reading archive entry")?;
        let path = entry.path().context("reading entry path")?.to_path_buf();
        validate_archive_path(&path)?;
        reject_unsafe_entry_type(entry.header().entry_type(), &path)?;
        if entry.header().entry_type().is_symlink() {
            let target = entry
                .link_name()
                .context("reading symlink target")?
                .ok_or_else(|| {
                    anyhow::anyhow!("symlink entry missing target: {}", path.display())
                })?;
            validate_symlink_target(&target, &path)?;
        }
        entry
            .unpack_in(destination)
            .with_context(|| format!("extracting {}", path.display()))?;
    }
    Ok(())
}

fn validate_archive_path(path: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(
        !path.is_absolute(),
        "archive path is absolute: {}",
        path.display()
    );
    anyhow::ensure!(
        path.components()
            .all(|part| !matches!(part, Component::ParentDir)),
        "archive entry escapes rootfs: {}",
        path.display()
    );
    Ok(())
}

fn validate_symlink_target(target: &Path, path: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(
        !target.is_absolute(),
        "symlink {} has absolute target: {}",
        path.display(),
        target.display()
    );

    let mut depth: usize = path.parent().map_or(0, |p| p.components().count());

    for part in target.components() {
        match part {
            Component::ParentDir => {
                if depth == 0 {
                    anyhow::bail!(
                        "symlink {} escapes rootfs: target contains parent directory reference: {}",
                        path.display(),
                        target.display()
                    );
                }
                depth -= 1;
            }
            Component::Normal(_) => {
                depth += 1;
            }
            Component::RootDir => {
                anyhow::bail!(
                    "symlink {} has absolute target: {}",
                    path.display(),
                    target.display()
                );
            }
            Component::CurDir | Component::Prefix(_) => {}
        }
    }
    Ok(())
}

fn reject_unsafe_entry_type(entry_type: tar::EntryType, path: &Path) -> anyhow::Result<()> {
    if entry_type.is_block_special() {
        anyhow::bail!("archive contains block device at {}", path.display());
    }
    if entry_type.is_character_special() {
        anyhow::bail!("archive contains character device at {}", path.display());
    }
    if entry_type.is_fifo() {
        anyhow::bail!("archive contains fifo at {}", path.display());
    }
    if entry_type.is_hard_link() {
        anyhow::bail!("archive contains hard link at {}", path.display());
    }
    match entry_type {
        tar::EntryType::Regular
        | tar::EntryType::Continuous
        | tar::EntryType::Directory
        | tar::EntryType::Symlink
        | tar::EntryType::GNULongName
        | tar::EntryType::GNULongLink
        | tar::EntryType::GNUSparse
        | tar::EntryType::XHeader
        | tar::EntryType::XGlobalHeader => Ok(()),
        _ => anyhow::bail!(
            "archive contains unsupported entry type at {}",
            path.display()
        ),
    }
}

/// Builds a compressed tar.gz rootfs image from a `context` directory into `output`.
///
/// Walks the directory recursively without following symlinks and writes relative paths.
/// Fails if `output` is located inside `context`, or if any special devices / escaping symlinks exist.
pub fn build_image(context: &Path, output: &Path) -> anyhow::Result<()> {
    let context_canon = context
        .canonicalize()
        .with_context(|| format!("resolving context path {}", context.display()))?;
    anyhow::ensure!(
        context_canon.is_dir(),
        "context path is not a directory: {}",
        context.display()
    );

    // If output exists or is a symlink, verify it doesn't resolve inside context
    if output.exists() || output.is_symlink() {
        if let Ok(existing_canon) = output.canonicalize() {
            if existing_canon.starts_with(&context_canon) {
                anyhow::bail!(
                    "output path {} is inside context directory {}",
                    output.display(),
                    context.display()
                );
            }
        }
    }

    let parent = output.parent().unwrap_or(Path::new(""));
    let parent_path = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };

    // Before creating non-existent parent directories, ensure ancestors don't reside inside context
    let mut ancestor = parent_path;
    while !ancestor.exists() {
        if let Some(p) = ancestor.parent() {
            if p.as_os_str().is_empty() {
                ancestor = Path::new(".");
                break;
            }
            ancestor = p;
        } else {
            ancestor = Path::new(".");
            break;
        }
    }
    let ancestor_canon = ancestor
        .canonicalize()
        .with_context(|| format!("resolving output parent ancestor {}", ancestor.display()))?;
    if ancestor_canon.starts_with(&context_canon) {
        anyhow::bail!(
            "output path {} is inside context directory {}",
            output.display(),
            context.display()
        );
    }

    let parent_canon = if parent_path.exists() {
        parent_path
            .canonicalize()
            .with_context(|| format!("resolving output parent {}", parent_path.display()))?
    } else {
        std::fs::create_dir_all(parent_path)
            .with_context(|| format!("creating parent directory for {}", output.display()))?;
        parent_path
            .canonicalize()
            .with_context(|| format!("resolving output parent {}", parent_path.display()))?
    };
    let file_name = output
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("output path has no file name: {}", output.display()))?;
    let output_canon = parent_canon.join(file_name);

    if output_canon.starts_with(&context_canon) {
        anyhow::bail!(
            "output path {} is inside context directory {}",
            output.display(),
            context.display()
        );
    }

    let file = std::fs::File::create(output)
        .with_context(|| format!("creating output file {}", output.display()))?;
    let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(enc);
    builder.follow_symlinks(false);

    let build_res = (|| -> anyhow::Result<()> {
        walk_and_append(&mut builder, &context_canon, Path::new(""))?;
        builder.finish().context("finishing tar archive")?;
        let enc = builder.into_inner().context("finalizing tar builder")?;
        enc.finish().context("finishing gzip compression")?;
        Ok(())
    })();

    if build_res.is_err() {
        let _ = std::fs::remove_file(output);
    }

    build_res
}

fn walk_and_append<W: std::io::Write>(
    builder: &mut tar::Builder<W>,
    current_dir: &Path,
    rel_dir: &Path,
) -> anyhow::Result<()> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(current_dir)
        .with_context(|| format!("reading directory {}", current_dir.display()))?
    {
        let entry = entry.with_context(|| format!("reading entry in {}", current_dir.display()))?;
        entries.push(entry);
    }
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let file_name = entry.file_name();
        let full_path = entry.path();
        let rel_path = if rel_dir.as_os_str().is_empty() {
            PathBuf::from(&file_name)
        } else {
            rel_dir.join(&file_name)
        };

        let meta = std::fs::symlink_metadata(&full_path)
            .with_context(|| format!("getting metadata for {}", full_path.display()))?;
        let file_type = meta.file_type();

        if file_type.is_symlink() {
            let target = std::fs::read_link(&full_path)
                .with_context(|| format!("reading symlink at {}", full_path.display()))?;
            anyhow::ensure!(
                !target.is_absolute(),
                "symlink {} target is absolute: {}",
                rel_path.display(),
                target.display()
            );
            let mut depth: usize = rel_path.parent().map_or(0, |p| p.components().count());
            for part in target.components() {
                match part {
                    Component::ParentDir => {
                        if depth == 0 {
                            anyhow::bail!(
                                "symlink {} target escapes rootfs: {}",
                                rel_path.display(),
                                target.display()
                            );
                        }
                        depth -= 1;
                    }
                    Component::Normal(_) => depth += 1,
                    Component::RootDir => {
                        anyhow::bail!(
                            "symlink {} target is absolute: {}",
                            rel_path.display(),
                            target.display()
                        );
                    }
                    _ => {}
                }
            }
            builder
                .append_path_with_name(&full_path, &rel_path)
                .with_context(|| format!("archiving symlink {}", rel_path.display()))?;
        } else if file_type.is_dir() {
            builder
                .append_path_with_name(&full_path, &rel_path)
                .with_context(|| format!("archiving directory {}", rel_path.display()))?;
            walk_and_append(builder, &full_path, &rel_path)?;
        } else if file_type.is_file() {
            builder
                .append_path_with_name(&full_path, &rel_path)
                .with_context(|| format!("archiving file {}", rel_path.display()))?;
        } else {
            anyhow::bail!(
                "unsupported special file in context at {}",
                rel_path.display()
            );
        }
    }
    Ok(())
}
