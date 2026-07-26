use std::fs;
use std::net::TcpListener;
use std::process::Command;

use tempfile::tempdir;

#[test]
fn bare_clean_removes_the_entire_configured_cache() {
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path().join("faster-beamer-cache");
    let outside_sentinel = temp_dir.path().join("keep.txt");
    let nested_cache_entry = cache_dir.join("drive_c").join("slides").join("frame.pdf");
    fs::write(&outside_sentinel, b"keep").unwrap();

    for clean_flag in ["--clean", "-c"] {
        fs::create_dir_all(nested_cache_entry.parent().unwrap()).unwrap();
        fs::write(&nested_cache_entry, b"cached frame").unwrap();

        let output = Command::new(env!("CARGO_BIN_EXE_faster-beamer"))
            .arg(clean_flag)
            .env("FASTER_BEAMER_CACHE_DIR", &cache_dir)
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "bare {} failed:\nstdout: {}\nstderr: {}",
            clean_flag,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!cache_dir.exists());
        assert!(outside_sentinel.exists());
    }

    let idempotent_clean = Command::new(env!("CARGO_BIN_EXE_faster-beamer"))
        .arg("--clean")
        .env("FASTER_BEAMER_CACHE_DIR", &cache_dir)
        .status()
        .unwrap();
    assert!(idempotent_clean.success());
}

#[test]
fn clean_with_input_remains_scoped() {
    let temp_dir = tempdir().unwrap();
    let input_dir = temp_dir.path().join("deck");
    let input_file = input_dir.join("slides.tex");
    let output_file = input_dir.join("slides.pdf");
    let synctex_file = input_dir.join("slides.synctex.gz");
    let stale_temp_file = input_dir.join("faster-beamer-temp-stale.tex");
    let cache_dir = temp_dir.path().join("faster-beamer-cache");
    let unrelated_cache_entry = cache_dir.join("unrelated.txt");
    fs::create_dir_all(&input_dir).unwrap();
    fs::create_dir_all(&cache_dir).unwrap();
    fs::write(&input_file, b"source").unwrap();
    fs::write(&output_file, b"published PDF").unwrap();
    fs::write(&synctex_file, b"published SyncTeX").unwrap();
    fs::write(&stale_temp_file, b"stale temp source").unwrap();
    fs::write(&unrelated_cache_entry, b"other cached data").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_faster-beamer"))
        .arg("--clean")
        .arg(&input_file)
        .env("FASTER_BEAMER_CACHE_DIR", &cache_dir)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(input_file.exists());
    assert!(output_file.exists());
    assert!(!synctex_file.exists());
    assert!(!stale_temp_file.exists());
    assert!(unrelated_cache_entry.exists());
}

#[test]
fn input_is_still_required_without_clean() {
    let output = Command::new(env!("CARGO_BIN_EXE_faster-beamer"))
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("INPUT"));
}

#[test]
fn clean_with_other_options_still_requires_input() {
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path().join("faster-beamer-cache");
    let cache_entry = cache_dir.join("drive_c").join("slides").join("frame.pdf");
    fs::create_dir_all(cache_entry.parent().unwrap()).unwrap();
    fs::write(&cache_entry, b"cached frame").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_faster-beamer"))
        .args(["--clean", "--watch"])
        .env("FASTER_BEAMER_CACHE_DIR", &cache_dir)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(cache_entry.exists());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("INPUT is required when --clean is combined with other arguments"));
}

#[test]
fn bare_clean_rejects_parent_components_in_the_cache_path() {
    let temp_dir = tempdir().unwrap();
    let cache_parent = temp_dir.path().join("cache-parent");
    let child = cache_parent.join("child");
    let outside_sentinel = cache_parent.join("keep.txt");
    fs::create_dir_all(&child).unwrap();
    fs::write(&outside_sentinel, b"keep").unwrap();
    let cache_path_with_parent = child.join("..");

    let output = Command::new(env!("CARGO_BIN_EXE_faster-beamer"))
        .arg("--clean")
        .env("FASTER_BEAMER_CACHE_DIR", &cache_path_with_parent)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(child.exists());
    assert!(outside_sentinel.exists());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("Refusing to clean a cache path containing a parent component"));
}

#[test]
fn bare_clean_refuses_to_remove_a_cache_with_a_live_watcher() {
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path().join("faster-beamer-cache");
    let cache_entry = cache_dir.join("drive_c").join("slides").join("frame.pdf");
    let guard_dir = cache_dir.join("guards");
    let guard_file = guard_dir.join("active.guard");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    fs::create_dir_all(cache_entry.parent().unwrap()).unwrap();
    fs::create_dir_all(&guard_dir).unwrap();
    fs::write(&cache_entry, b"cached frame").unwrap();
    fs::write(
        &guard_file,
        format!(
            "pid={}\ninput=slides.tex\nfingerprint=test\naddr={}\n",
            std::process::id(),
            listener.local_addr().unwrap()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_faster-beamer"))
        .arg("--clean")
        .env("FASTER_BEAMER_CACHE_DIR", &cache_dir)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(cache_entry.exists());
    assert!(guard_file.exists());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("Refusing to clean all caches while a faster-beamer watcher is active"));
}
