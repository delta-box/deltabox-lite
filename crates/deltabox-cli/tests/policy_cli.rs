use deltabox_core::policy::Policy;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture {
    root: tempfile::TempDir,
    policy: PathBuf,
}

impl Fixture {
    fn new(raw: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let policy = root.path().join("policy.toml");
        std::fs::write(&policy, raw).unwrap();
        Self { root, policy }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_deltabox"))
            .current_dir(self.root.path())
            .env("DELTABOX_STATE_DIR", self.root.path().join("state"))
            .args(args)
            .output()
            .unwrap()
    }

    fn check(&self) -> Output {
        self.run(&["policy", "check", self.policy.to_str().unwrap(), "--json"])
    }

    fn assert_no_state(&self) {
        let mut entries: Vec<_> = std::fs::read_dir(self.root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        entries.sort();
        assert_eq!(entries, ["policy.toml"]);
    }
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "status: {}; stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
}

fn failure(output: &Output, message: &str) {
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(message), "expected {message:?}: {stderr}");
}

fn json(output: &Output) -> Value {
    success(output);
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn check_valid_policy_reports_resolved_policy_without_creating_state() {
    let fixture = Fixture::new(
        r#"
[filesystem]
read = [".", "/usr"]
write = ["."]
deny = [".env", "*.pem"]
allow_approval = false

[network]
mode = "allowlist"
allow = ["example.com:443"]
allow_approval = true
deny = ["blocked.example:443"]

[limits]
max_write_bytes = 1048576
"#,
    );
    let result = json(&fixture.check());
    assert_eq!(result["valid"], true);
    assert_eq!(result["policy"]["filesystem"]["allow_approval"], false);
    assert_eq!(result["policy"]["network"]["allow"][0], "example.com:443");
    assert_eq!(result["policy"]["network"]["allow_private"], false);
    assert_eq!(result["policy"]["limits"]["max_write_bytes"], 1048576);
    assert!(result["policy"]["limits"]["max_processes"].is_null());

    let human = fixture.run(&["policy", "check", "policy.toml"]);
    success(&human);
    assert!(String::from_utf8_lossy(&human.stdout).contains("Policy is valid"));
    fixture.assert_no_state();
}

#[test]
fn check_rejects_semantically_invalid_policies() {
    for (raw, expected) in [
        (
            "[network]\nmode = 'deny_all'\nallow = ['example.com:443']\n",
            "network.allow is only valid",
        ),
        (
            "[network]\nmode = 'allowlist'\nallow = ['*']\n",
            "network allowlist must not contain '*'",
        ),
        (
            "[network]\nmode = 'allow_audit'\nallow_approval = true\n",
            "allow_approval requires network allowlist mode",
        ),
        (
            "[network]\nmode = 'allowlist'\nallow_private = true\n",
            "network deny and allow_private require allow_approval",
        ),
        (
            "[network]\nmode = 'allowlist'\nallow_approval = true\nallow = ['https://example.com']\n",
            "invalid policy",
        ),
    ] {
        let fixture = Fixture::new(raw);
        failure(&fixture.check(), expected);
        fixture.assert_no_state();
    }
}

#[test]
fn check_reports_unknown_keys_at_every_policy_level() {
    for (raw, key) in [
        ("netwrok = {}", "netwrok"),
        ("[filesystem]\nwrites = ['.']", "filesystem.writes"),
        ("[filesystem]\nallow_aproval = false", "filesystem.allow_aproval"),
        ("[network]\nallow_aproval = true", "network.allow_aproval"),
        ("[limits]\nmax_proccesses = 32", "limits.max_proccesses"),
        (
            "[network]\nmode = { allowlist = [], alowlist = [] }",
            "network.mode.alowlist",
        ),
    ] {
        let fixture = Fixture::new(raw);
        failure(&fixture.check(), &format!("unknown policy key '{key}'"));
        fixture.assert_no_state();
    }
}

#[test]
fn check_reports_malformed_toml_and_wrong_field_types() {
    let malformed = Fixture::new("[network\nmode = 'deny_all'");
    failure(&malformed.check(), "invalid policy TOML");
    malformed.assert_no_state();

    let wrong_type = Fixture::new("[limits]\nmax_processes = 'many'");
    failure(&wrong_type.check(), "invalid policy fields");
    wrong_type.assert_no_state();
}

#[test]
fn missing_policy_files_report_read_errors_without_creating_state() {
    let fixture = Fixture::new("");
    failure(
        &fixture.run(&["policy", "check", "missing.toml"]),
        "failed to read policy missing.toml",
    );
    failure(
        &fixture.run(&["policy", "show", "--policy", "missing.toml"]),
        "failed to read policy missing.toml",
    );
    fixture.assert_no_state();
}

#[test]
fn legacy_allowlists_are_valid_and_shown_in_canonical_form() {
    let fixture =
        Fixture::new("[network]\nmode = { allowlist = ['example.com:443', '127.0.0.1:3000'] }\n");
    let checked = json(&fixture.check());
    assert_eq!(checked["policy"]["network"]["mode"], "allowlist");
    assert_eq!(
        checked["policy"]["network"]["allow"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let shown = fixture.run(&["policy", "show", "--policy", "policy.toml"]);
    success(&shown);
    let policy: Policy = toml::from_str(std::str::from_utf8(&shown.stdout).unwrap()).unwrap();
    assert_eq!(serde_json::to_value(policy).unwrap(), checked["policy"]);
    assert!(String::from_utf8_lossy(&shown.stdout).contains("mode = \"allowlist\""));
    fixture.assert_no_state();

    let duplicate = Fixture::new(
        "[network]\nmode = { allowlist = ['example.com:443'] }\nallow = ['other.example:443']\n",
    );
    failure(&duplicate.check(), "specified in both mode and allow");
}

#[test]
fn default_agent_output_roundtrips_through_check() {
    let fixture = Fixture::new("");
    let expected = serde_json::to_value(Policy::default_agent()).unwrap();
    let json_shown = json(&fixture.run(&["policy", "show", "--json"]));
    assert_eq!(json_shown, expected);

    let toml_shown = fixture.run(&["policy", "show"]);
    success(&toml_shown);
    std::fs::write(&fixture.policy, &toml_shown.stdout).unwrap();
    let checked = json(&fixture.check());
    assert_eq!(checked["policy"], expected);
    fixture.assert_no_state();
}

#[test]
fn explicit_partial_policy_uses_runtime_defaults_without_agent_grants() {
    let fixture = Fixture::new("[network]\nmode = 'deny_all'\n");
    let shown = json(&fixture.run(&["policy", "show", "--policy", "policy.toml", "--json"]));
    let loaded = Policy::load(Path::new(&fixture.policy)).unwrap();
    assert_eq!(shown, serde_json::to_value(loaded).unwrap());
    assert_eq!(shown["filesystem"]["read"], serde_json::json!([]));
    assert_eq!(shown["filesystem"]["deny"], serde_json::json!([]));
    assert_eq!(shown["filesystem"]["allow_approval"], true);
    assert!(shown["limits"]["max_processes"].is_null());
    assert!(shown["limits"]["max_storage_bytes"].is_null());
    fixture.assert_no_state();
}

#[test]
fn show_rejects_semantic_errors_and_preserves_runtime_unknown_key_compatibility() {
    let invalid = Fixture::new("[network]\nmode = 'deny_all'\nallow = ['example.com:443']\n");
    failure(
        &invalid.run(&["policy", "show", "--policy", "policy.toml"]),
        "network.allow is only valid",
    );
    invalid.assert_no_state();

    // `check` is stricter than runtime loading. `show` describes the actual
    // policy loaded at runtime, even for files containing ignored keys.
    let unknown = Fixture::new("[network]\nmode = 'deny_all'\nfuture_option = true\n");
    let loaded = Policy::load(&unknown.policy).unwrap();
    let shown = json(&unknown.run(&["policy", "show", "--policy", "policy.toml", "--json"]));
    assert_eq!(shown, serde_json::to_value(loaded).unwrap());
    failure(
        &unknown.check(),
        "unknown policy key 'network.future_option'",
    );
    unknown.assert_no_state();
}
