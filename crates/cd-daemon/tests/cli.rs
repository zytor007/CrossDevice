use std::process::Command;

fn crossdev() -> Command {
    Command::new(env!("CARGO_BIN_EXE_crossdev"))
}

#[test]
fn geometry_report_matches_measured_profiles() {
    let output = crossdev()
        .args(["geometry", "--air", "both"])
        .output()
        .expect("run crossdev geometry");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("50.8%"), "missing 13-inch coverage");
    assert!(stdout.contains("57.2%"), "missing 15-inch coverage");
    assert!(stdout.contains("1.65×"));
    assert!(stdout.contains("1.70×"));
    assert!(stdout.contains("[通过] 布局校验"));
}

#[test]
fn demo_runs_all_two_node_checks() {
    let output = crossdev()
        .args(["demo", "--air", "both"])
        .output()
        .expect("run crossdev demo");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("0 失败"));
    assert!(stdout.contains("对端掉线回切"));
    assert!(stdout.contains("750ms 超时回切"));
}

#[test]
fn geometry_json_is_machine_readable() {
    let output = crossdev()
        .args(["geometry", "--air", "both", "--json"])
        .output()
        .expect("run crossdev geometry --json");
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let profiles = value["profiles"].as_array().expect("profiles array");
    assert_eq!(profiles.len(), 2);
    assert_eq!(value["protocol"]["c1_event_bytes"], 20);
    for profile in profiles {
        for check in profile["checks"].as_array().unwrap() {
            assert_eq!(check["passed"], true, "{check}");
        }
    }
}

#[test]
fn excessive_gap_is_reported_as_a_failure() {
    let output = crossdev()
        .args(["geometry", "--air", "13", "--gap-mm", "25"])
        .output()
        .expect("run crossdev geometry with gap");
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("共享边: 无"), "{stdout}");
}

#[test]
fn link_self_test_covers_c0_and_c1() {
    let output = crossdev()
        .args(["link", "self-test"])
        .output()
        .expect("run crossdev link self-test");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("C1:"), "{stdout}");
    assert!(stdout.contains("[通过]"), "{stdout}");
}

#[test]
fn clipboard_self_test_covers_guard_and_c2() {
    let output = crossdev()
        .args(["clipboard", "self-test"])
        .output()
        .expect("run crossdev clipboard self-test");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("防回环"), "{stdout}");
    assert!(stdout.contains("C2:"), "{stdout}");
    assert!(stdout.contains("[通过]"), "{stdout}");
}

#[test]
fn files_self_test_covers_manifest_and_tasks() {
    let output = crossdev()
        .args(["files", "self-test"])
        .output()
        .expect("run crossdev files self-test");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("manifest:"), "{stdout}");
    assert!(stdout.contains("paused"), "{stdout}");
    assert!(stdout.contains("[通过]"), "{stdout}");
}

#[test]
fn files_roundtrip_copies_and_resumes() {
    let output = crossdev()
        .args(["files", "roundtrip"])
        .output()
        .expect("run crossdev files roundtrip");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("断点续跑"), "{stdout}");
    assert!(stdout.contains("冲突重命名"), "{stdout}");
    assert!(stdout.contains("[通过]"), "{stdout}");
}

#[test]
fn pair_self_test_persists_device() {
    let output = crossdev()
        .args(["pair", "self-test"])
        .output()
        .expect("run crossdev pair self-test");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("PAIRED"), "{stdout}");
    assert!(stdout.contains("持久化"), "{stdout}");
    assert!(stdout.contains("[通过]"), "{stdout}");
}

#[test]
fn sync_self_test_covers_full_network_stack() {
    let output = crossdev()
        .args(["sync", "self-test"])
        .output()
        .expect("run crossdev sync self-test");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("SAS:"), "{stdout}");
    assert!(stdout.contains("chunks"), "{stdout}");
    assert!(stdout.contains("[通过]"), "{stdout}");
}

#[test]
fn daemon_self_test_covers_whitelist_resume_and_http() {
    let output = crossdev()
        .args(["daemon", "self-test"])
        .output()
        .expect("run crossdev daemon self-test");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("SAS:"), "{stdout}");
    assert!(stdout.contains("配对请求"), "{stdout}");
    assert!(stdout.contains("去重"), "{stdout}");
    assert!(stdout.contains("续传"), "{stdout}");
    assert!(stdout.contains("图片"), "{stdout}");
    assert!(stdout.contains("API 文件发送"), "{stdout}");
    assert!(stdout.contains("双向同时发送"), "{stdout}");
    assert!(stdout.contains("配对设置"), "{stdout}");
    assert!(stdout.contains("同名策略"), "{stdout}");
    assert!(stdout.contains("文件剪贴板"), "{stdout}");
    assert!(stdout.contains("ask"), "{stdout}");
    assert!(stdout.contains("传输取消"), "{stdout}");
    assert!(stdout.contains("暂停/继续"), "{stdout}");
    assert!(stdout.contains("布局编辑"), "{stdout}");
    assert!(stdout.contains("设备撤销"), "{stdout}");
    assert!(stdout.contains("HTTP"), "{stdout}");
    assert!(stdout.contains("[通过]"), "{stdout}");
}
