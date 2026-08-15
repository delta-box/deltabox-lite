use std::process::Command;

#[test]
fn quoted_shell_command_explains_how_to_run_shell_syntax() {
    let project = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_deltabox"))
        .current_dir(project.path())
        .args(["exec", "unused-session", "--", "echo hello > hello.txt"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("accepts an argv command"), "{stderr}");
    assert!(stderr.contains("sh -c"), "{stderr}");
}
