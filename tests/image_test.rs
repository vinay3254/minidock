use std::path::PathBuf;

fn malicious_parent_entry_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("rootfs");
    let escaped = temp.path().join("escaped");
    let image = temp.path().join("malicious.tar.gz");

    let file = std::fs::File::create(&image).unwrap();
    let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(enc);

    let data = b"evil content";
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    let name_bytes = b"../../escaped";
    header.as_mut_bytes()[..name_bytes.len()].copy_from_slice(name_bytes);
    header.set_cksum();

    builder.append(&header, &data[..]).unwrap();
    builder.into_inner().unwrap().finish().unwrap();

    (temp, image, destination, escaped)
}

#[test]
fn build_then_extract_preserves_a_regular_file() {
    let temp = tempfile::tempdir().unwrap();
    let context = temp.path().join("context");
    std::fs::create_dir(&context).unwrap();
    std::fs::write(context.join("hello"), b"world").unwrap();
    let image = temp.path().join("rootfs.tar.gz");
    let rootfs = temp.path().join("rootfs");
    minidock::image::build_image(&context, &image).unwrap();
    minidock::image::extract_rootfs(&image, &rootfs).unwrap();
    assert_eq!(std::fs::read(rootfs.join("hello")).unwrap(), b"world");
}

#[test]
fn extraction_rejects_parent_directory_entries() {
    let (_temp, image, destination, escaped) = malicious_parent_entry_fixture();
    let error = minidock::image::extract_rootfs(&image, &destination).unwrap_err();
    assert!(error.to_string().contains("escapes rootfs"));
    assert!(!escaped.exists());
}

#[test]
fn extraction_rejects_absolute_entries() {
    let temp = tempfile::tempdir().unwrap();
    let image = temp.path().join("absolute.tar.gz");
    let destination = temp.path().join("rootfs");

    let file = std::fs::File::create(&image).unwrap();
    let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(enc);

    let data = b"absolute path content";
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    let name_bytes = b"/etc/malicious";
    header.as_mut_bytes()[..name_bytes.len()].copy_from_slice(name_bytes);
    header.set_cksum();

    builder.append(&header, &data[..]).unwrap();
    builder.into_inner().unwrap().finish().unwrap();

    let error = minidock::image::extract_rootfs(&image, &destination).unwrap_err();
    assert!(
        error.to_string().contains("absolute"),
        "error message should mention absolute: {error}"
    );
}

#[test]
fn extraction_rejects_symlink_with_parent_directory() {
    let temp = tempfile::tempdir().unwrap();
    let image = temp.path().join("symlink_parent.tar.gz");
    let destination = temp.path().join("rootfs");

    let file = std::fs::File::create(&image).unwrap();
    let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(enc);

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    header.set_cksum();

    builder
        .append_link(&mut header, "bad_link", "../escaped")
        .unwrap();
    builder.into_inner().unwrap().finish().unwrap();

    let error = minidock::image::extract_rootfs(&image, &destination).unwrap_err();
    assert!(
        error.to_string().contains("escapes rootfs") || error.to_string().contains(".."),
        "error should reject escaping symlink: {error}"
    );
}

#[test]
fn extraction_rejects_symlink_with_absolute_target() {
    let temp = tempfile::tempdir().unwrap();
    let image = temp.path().join("symlink_abs.tar.gz");
    let destination = temp.path().join("rootfs");

    let file = std::fs::File::create(&image).unwrap();
    let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(enc);

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    header.set_cksum();

    builder
        .append_link(&mut header, "bad_link", "/etc/passwd")
        .unwrap();
    builder.into_inner().unwrap().finish().unwrap();

    let error = minidock::image::extract_rootfs(&image, &destination).unwrap_err();
    assert!(
        error.to_string().contains("absolute"),
        "error should reject absolute symlink: {error}"
    );
}

#[test]
fn extraction_rejects_fifo_entry() {
    let temp = tempfile::tempdir().unwrap();
    let image = temp.path().join("fifo.tar.gz");
    let destination = temp.path().join("rootfs");

    let file = std::fs::File::create(&image).unwrap();
    let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(enc);

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Fifo);
    header.set_size(0);
    header.set_mode(0o644);
    header.set_cksum();

    builder
        .append_data(&mut header, "my_fifo", &b""[..])
        .unwrap();
    builder.into_inner().unwrap().finish().unwrap();

    let error = minidock::image::extract_rootfs(&image, &destination).unwrap_err();
    assert!(
        error.to_string().contains("fifo"),
        "error should reject fifo: {error}"
    );
}

#[test]
fn build_image_rejects_output_inside_context() {
    let temp = tempfile::tempdir().unwrap();
    let context = temp.path().join("context");
    std::fs::create_dir(&context).unwrap();
    std::fs::write(context.join("file.txt"), b"test").unwrap();

    let output = context.join("output.tar.gz");
    let error = minidock::image::build_image(&context, &output).unwrap_err();
    assert!(
        error.to_string().contains("inside context"),
        "expected inside context error: {error}"
    );
}

#[test]
fn build_image_rejects_symlink_escaping_context() {
    let temp = tempfile::tempdir().unwrap();
    let context = temp.path().join("context");
    std::fs::create_dir(&context).unwrap();
    std::os::unix::fs::symlink("../outside", context.join("bad_symlink")).unwrap();

    let output = temp.path().join("out.tar.gz");
    let error = minidock::image::build_image(&context, &output).unwrap_err();
    assert!(
        error.to_string().contains("..") || error.to_string().contains("escape"),
        "expected error rejecting escaping symlink: {error}"
    );
}

#[test]
fn round_trip_preserves_nested_structure_permissions_and_in_root_symlink() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let context = temp.path().join("context");
    std::fs::create_dir_all(context.join("nested/dir")).unwrap();
    std::fs::write(context.join("nested/dir/app.sh"), b"#!/bin/sh\necho hi\n").unwrap();
    std::fs::set_permissions(
        context.join("nested/dir/app.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    std::os::unix::fs::symlink("nested/dir/app.sh", context.join("link_to_app")).unwrap();

    let image = temp.path().join("image.tar.gz");
    let rootfs = temp.path().join("rootfs");

    minidock::image::build_image(&context, &image).unwrap();
    minidock::image::extract_rootfs(&image, &rootfs).unwrap();

    let app_path = rootfs.join("nested/dir/app.sh");
    assert_eq!(std::fs::read(&app_path).unwrap(), b"#!/bin/sh\necho hi\n");
    let meta = std::fs::symlink_metadata(&app_path).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o755);

    let link_path = rootfs.join("link_to_app");
    let link_meta = std::fs::symlink_metadata(&link_path).unwrap();
    assert!(link_meta.file_type().is_symlink());
    let target = std::fs::read_link(&link_path).unwrap();
    assert_eq!(target, std::path::Path::new("nested/dir/app.sh"));
}

#[test]
fn extraction_rejects_hard_link_entry() {
    let temp = tempfile::tempdir().unwrap();
    let image = temp.path().join("hardlink.tar.gz");
    let destination = temp.path().join("rootfs");

    let file = std::fs::File::create(&image).unwrap();
    let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(enc);

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Link);
    header.set_size(0);
    header.set_mode(0o644);
    header.set_cksum();

    builder
        .append_link(&mut header, "my_hardlink", "target_file")
        .unwrap();
    builder.into_inner().unwrap().finish().unwrap();

    let error = minidock::image::extract_rootfs(&image, &destination).unwrap_err();
    assert!(
        error.to_string().contains("hard link"),
        "error should reject hard link: {error}"
    );
}

#[test]
fn build_image_rejects_output_in_nonexistent_subdir_of_context_without_creating_it() {
    let temp = tempfile::tempdir().unwrap();
    let context = temp.path().join("context");
    std::fs::create_dir(&context).unwrap();
    std::fs::write(context.join("file.txt"), b"test").unwrap();

    let non_existent_subdir = context.join("does_not_exist/nested");
    let output = non_existent_subdir.join("out.tar.gz");

    let error = minidock::image::build_image(&context, &output).unwrap_err();
    assert!(error.to_string().contains("inside context directory"));
    assert!(
        !context.join("does_not_exist").exists(),
        "parent directories inside context should not have been created"
    );
}

#[test]
fn build_image_rejects_symlink_output_pointing_into_context() {
    let temp = tempfile::tempdir().unwrap();
    let context = temp.path().join("context");
    std::fs::create_dir(&context).unwrap();
    std::fs::write(context.join("file.txt"), b"test").unwrap();

    let target_inside = context.join("actual_out.tar.gz");
    std::fs::write(&target_inside, b"dummy").unwrap();

    let outside_symlink = temp.path().join("outside_symlink.tar.gz");
    std::os::unix::fs::symlink(&target_inside, &outside_symlink).unwrap();

    let error = minidock::image::build_image(&context, &outside_symlink).unwrap_err();
    assert!(error.to_string().contains("inside context directory"));
}
