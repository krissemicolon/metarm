//! End-to-end tests that run the compiled `metarm` binary.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_metarm");

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> TempDir {
        let p = std::env::temp_dir().join(format!("metarm-test-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run(dir: &TempDir, args: &[&str]) -> Output {
    Command::new(BIN)
        .current_dir(&dir.0)
        .args(args)
        .output()
        .unwrap()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Little-endian TIFF: IFD0 with Make="SECRETCAM" and Orientation=`orientation`.
fn exif_tiff(orientation: u16) -> Vec<u8> {
    let mut t = b"II\x2A\0\x08\0\0\0".to_vec();
    t.extend_from_slice(&2u16.to_le_bytes());
    // Make: ASCII, count 10, offset 38 (8 + 2 + 2*12 + 4)
    t.extend_from_slice(&[0x0F, 0x01, 2, 0, 10, 0, 0, 0, 38, 0, 0, 0]);
    t.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0]);
    t.extend_from_slice(&orientation.to_le_bytes());
    t.extend_from_slice(&[0, 0]);
    t.extend_from_slice(&[0, 0, 0, 0]);
    t.extend_from_slice(b"SECRETCAM\0");
    t
}

fn jpeg_seg(marker: u8, payload: &[u8]) -> Vec<u8> {
    let mut s = vec![0xFF, marker];
    s.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
    s.extend_from_slice(payload);
    s
}

/// A structurally valid JPEG with EXIF (make + orientation 6) and a comment.
fn jpeg() -> Vec<u8> {
    let mut f = vec![0xFF, 0xD8];
    f.extend(jpeg_seg(0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0"));
    f.extend(jpeg_seg(
        0xE1,
        &[b"Exif\0\0".as_ref(), &exif_tiff(6)].concat(),
    ));
    f.extend(jpeg_seg(0xFE, b"SECRET COMMENT"));
    f.extend(jpeg_seg(0xDB, &[0; 65]));
    f.extend(jpeg_seg(0xDA, &[1, 1, 0, 0, 0x3F, 0]));
    f.extend_from_slice(&[0x12, 0x34, 0xFF, 0x00, 0x56]);
    f.extend_from_slice(&[0xFF, 0xD9]);
    f
}

fn png_chunk(ty: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut c = (data.len() as u32).to_be_bytes().to_vec();
    c.extend_from_slice(ty);
    c.extend_from_slice(data);
    c.extend_from_slice(&[0; 4]);
    c
}

fn png() -> Vec<u8> {
    let mut f = b"\x89PNG\r\n\x1a\n".to_vec();
    f.extend(png_chunk(b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0, 0, 0]));
    f.extend(png_chunk(b"tEXt", b"Author\0SECRET PERSON"));
    f.extend(png_chunk(b"IDAT", &[0; 4]));
    f.extend(png_chunk(b"IEND", &[]));
    f
}

fn write(path: &Path, data: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
}

#[test]
fn strips_single_file_and_keeps_orientation() {
    let d = TempDir::new("single");
    write(&d.path("a.jpg"), &jpeg());

    let out = run(&d, &["a.jpg"]);
    assert!(out.status.success(), "{:?}", out);
    assert!(out.stdout.is_empty(), "should be quiet without -v");

    let data = fs::read(d.path("a.jpg")).unwrap();
    assert!(!contains(&data, b"SECRET"));
    assert!(
        contains(&data, b"Exif\0\0"),
        "orientation EXIF should be kept"
    );
    assert!(
        data.ends_with(&[0x12, 0x34, 0xFF, 0x00, 0x56, 0xFF, 0xD9]),
        "scan data must be untouched"
    );
}

#[test]
fn all_flag_removes_exif_entirely() {
    let d = TempDir::new("all");
    write(&d.path("a.jpg"), &jpeg());

    assert!(run(&d, &["-a", "a.jpg"]).status.success());
    let data = fs::read(d.path("a.jpg")).unwrap();
    assert!(!contains(&data, b"Exif"));
    assert!(!contains(&data, b"SECRET"));
}

#[test]
fn second_run_is_a_noop() {
    let d = TempDir::new("idempotent");
    write(&d.path("a.jpg"), &jpeg());
    run(&d, &["a.jpg"]);
    let once = fs::read(d.path("a.jpg")).unwrap();

    let out = run(&d, &["-v", "a.jpg"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("no metadata"));
    assert_eq!(fs::read(d.path("a.jpg")).unwrap(), once);
}

#[test]
fn directory_requires_recursive_flag() {
    let d = TempDir::new("needs-r");
    write(&d.path("dir/a.jpg"), &jpeg());

    let out = run(&d, &["dir"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("is a directory"));
    assert!(contains(&fs::read(d.path("dir/a.jpg")).unwrap(), b"SECRET"));
}

#[test]
fn rf_strips_recursively_and_skips_unsupported() {
    let d = TempDir::new("rf");
    write(&d.path("dir/a.jpg"), &jpeg());
    write(&d.path("dir/sub/deep/b.png"), &png());
    write(&d.path("dir/sub/notes.txt"), b"SECRET notes");

    let out = run(&d, &["-rf", "dir"]);
    assert!(out.status.success(), "{:?}", out);
    assert!(!contains(
        &fs::read(d.path("dir/a.jpg")).unwrap(),
        b"SECRET"
    ));
    assert!(!contains(
        &fs::read(d.path("dir/sub/deep/b.png")).unwrap(),
        b"SECRET"
    ));
    assert_eq!(
        fs::read(d.path("dir/sub/notes.txt")).unwrap(),
        b"SECRET notes"
    );

    // Uppercase -R and combined flags work too.
    assert!(run(&d, &["-Rfv", "dir"]).status.success());
}

#[test]
fn explicit_errors_exit_nonzero_but_process_remaining_files() {
    let d = TempDir::new("errors");
    write(&d.path("a.jpg"), &jpeg());
    write(&d.path("notes.txt"), b"hello");

    let out = run(&d, &["-f", "missing.jpg", "notes.txt", "a.jpg"]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("missing.jpg: No such file or directory"),
        "{}",
        err
    );
    assert!(
        err.contains("notes.txt: unsupported file format"),
        "{}",
        err
    );
    assert!(!contains(&fs::read(d.path("a.jpg")).unwrap(), b"SECRET"));
}

#[test]
fn malformed_file_is_left_untouched() {
    let d = TempDir::new("malformed");
    let mut broken = jpeg();
    broken.truncate(30); // cut off mid-segment
    write(&d.path("broken.jpg"), &broken);

    let out = run(&d, &["broken.jpg"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(fs::read(d.path("broken.jpg")).unwrap(), broken);
}

#[cfg(unix)]
#[test]
fn preserves_permissions_and_follows_explicit_symlink() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let d = TempDir::new("perms");
    write(&d.path("a.jpg"), &jpeg());
    fs::set_permissions(d.path("a.jpg"), fs::Permissions::from_mode(0o640)).unwrap();
    symlink("a.jpg", d.path("link.jpg")).unwrap();

    assert!(run(&d, &["link.jpg"]).status.success());
    assert!(fs::symlink_metadata(d.path("link.jpg"))
        .unwrap()
        .file_type()
        .is_symlink());
    let meta = fs::metadata(d.path("a.jpg")).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o640);
    assert!(!contains(&fs::read(d.path("a.jpg")).unwrap(), b"SECRET"));
}
