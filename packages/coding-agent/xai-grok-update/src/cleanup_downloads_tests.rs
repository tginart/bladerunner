use super::*;
use std::time::Duration;

fn make_all_stale(dir: &Path) {
    let old = std::time::SystemTime::now() - (STALE_TMP_AGE + Duration::from_secs(60));
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_times(std::fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn executable_is_in_use_sees_this_test_process() {
    let exe = std::env::current_exe().expect("current_exe");
    assert!(
        executable_is_in_use(&exe),
        "the process table must show {}",
        exe.display()
    );
}

#[test]
fn executable_is_in_use_is_false_for_a_missing_binary() {
    assert!(!executable_is_in_use(Path::new("/no/such/grok-binary")));
}

#[test]
fn executable_is_in_use_is_false_for_an_idle_file() {
    let dir = tempfile::tempdir().unwrap();
    let idle = dir.path().join("grok-0.1.140-macos-aarch64");
    std::fs::write(&idle, "idle").unwrap();
    assert!(!executable_is_in_use(&idle));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn executable_is_in_use_matches_a_hard_link() {
    let exe = std::env::current_exe().expect("current_exe");
    let link = exe.with_file_name(format!(
        "{}.hardlink-{}",
        exe.file_name().unwrap().to_string_lossy(),
        std::process::id()
    ));
    std::fs::hard_link(&exe, &link).expect("hard link next to current_exe");
    let is_seen = executable_is_in_use(&link);
    std::fs::remove_file(&link).unwrap();
    assert!(
        is_seen,
        "the inode of {} must match {}",
        link.display(),
        exe.display()
    );
}

#[tokio::test]
async fn cleanup_keeps_an_older_binary_that_is_in_use() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    for v in ["0.1.140", "0.1.141", "0.1.142", "0.1.143", "0.1.144"] {
        std::fs::write(d.join(format!("grok-{v}-macos-aarch64")), v).unwrap();
    }
    std::fs::write(d.join("grok-0.1.145-macos-aarch64"), "current").unwrap();
    make_all_stale(d);

    let live = d.join("grok-0.1.140-macos-aarch64");
    cleanup_old_downloads_with(d, "grok", "0.1.145", |path| path == live).await;

    assert!(d.join("grok-0.1.145-macos-aarch64").exists(), "current");
    assert!(d.join("grok-0.1.144-macos-aarch64").exists(), "N-1");
    assert!(live.exists(), "in use");
    for idle in ["0.1.141", "0.1.142", "0.1.143"] {
        assert!(
            !d.join(format!("grok-{idle}-macos-aarch64")).exists(),
            "idle {idle}"
        );
    }
}
