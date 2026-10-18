#![cfg(windows)]

use base64::Engine as _;
use serde_json::Value;
use std::{fs, path::{Path, PathBuf}, process::{Command, Output}};

struct Session { root: tempfile::TempDir, project: PathBuf, id: String }
impl Session {
    fn command(&self) -> Command {
        let mut c=Command::new(env!("CARGO_BIN_EXE_deltabox"));
        c.env("DELTABOX_STATE_DIR",self.root.path().join("state"))
         .env("DELTABOX_RUNTIME_DIR",self.root.path().join("runtime"));
        c
    }
    fn raw(&self,args:&[&str])->Output {self.command().args(args).output().unwrap()}
    fn json(&self,args:&[&str])->Value {
        let out=self.raw(args);
        assert!(out.status.success(),"{args:?}: {}",String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn action(&self,action:&str,extra:&[&str])->Value {
        let mut args=vec![action,&self.id,"--project",self.project.to_str().unwrap()];
        args.extend_from_slice(extra);self.json(&args)
    }
    fn powershell(&self,body:&str)->Value {
        let script=format!("$ErrorActionPreference='Stop'; $ProgressPreference='SilentlyContinue'; [Console]::OutputEncoding=[Text.Encoding]::UTF8; {body}");
        let bytes:Vec<_>=script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let encoded=base64::engine::general_purpose::STANDARD.encode(bytes);
        let shell=PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let value=self.action("exec",&["--",shell.to_str().unwrap(),"-NoLogo","-NoProfile","-NonInteractive","-EncodedCommand",&encoded]);
        assert_eq!(value["exit_code"],0,"{body}: {value}");value
    }
    fn write(&self,name:&str,contents:&str) {
        let out=self.raw(&["write",&self.id,"--project",self.project.to_str().unwrap(),name,"--content",contents]);
        assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    }
}
impl Drop for Session {
    fn drop(&mut self) {if !self.id.is_empty() {let _=self.raw(&["session","stop",&self.id,"--project",self.project.to_str().unwrap()]);}}
}
fn quoted(path:&Path)->String {format!("'{}'",path.to_string_lossy().replace('\'',"''"))}

#[test]
#[ignore = "requires installed WinFsp and native MXC PSEC filesystem-deny support; run explicitly on a supported Windows host"]
fn powershell_through_lifecycle_and_isolation() {
    let root=tempfile::tempdir().unwrap();
    let project=root.path().join("project with spaces");fs::create_dir(&project).unwrap();
    fs::write(project.join("a.txt"),b"abcdefgh").unwrap();
    fs::write(project.join("deleted.txt"),b"restore me").unwrap();
    fs::write(project.join(".env"),b"secret").unwrap();
    fs::create_dir(project.join("folder")).unwrap();
    fs::write(project.join("folder/child.txt"),b"child").unwrap();
    fs::create_dir_all(project.join("folder/existing/deeper")).unwrap();
    fs::write(project.join("folder/existing/deeper/input.txt"),b"existing nested file").unwrap();
    fs::create_dir(project.join(".ssh")).unwrap();
    fs::write(project.join(".ssh/id_rsa"),b"secret").unwrap();
    let mut s=Session {root,project,id:String::new()};
    let info=s.json(&["session","start",s.project.to_str().unwrap(),"--fs-mode","cbcow-through","--require-fs-mode","--json"]);
    s.id=info["session_id"].as_str().unwrap().into();
    assert_eq!(info["host_visibility"],"immediate");
    assert_eq!(info["state_provider"],"windows-winfsp");
    s.powershell(r#"
      if ((Get-Content -LiteralPath 'folder/existing/deeper/input.txt' -Raw) -ne 'existing nested file') {throw 'existing nested read failed'}
      Set-Content -LiteralPath 'folder/existing/deeper/new.txt' -Value 'existing nested write'
      if ((Get-Content -LiteralPath 'folder/existing/deeper/new.txt' -Raw).Trim() -ne 'existing nested write') {throw 'existing nested write failed'}
      $f=[IO.File]::Open('a.txt',[IO.FileMode]::Open,[IO.FileAccess]::Write,[IO.FileShare]::ReadWrite)
      try { $f.Position=2; $f.Write([Text.Encoding]::UTF8.GetBytes('XY'),0,2) } finally { $f.Dispose() }
      Set-Content -LiteralPath 'new.txt' -Value 'new'
      New-Item -ItemType Directory -Path 'sub' | Out-Null
      Set-Content -LiteralPath 'sub/中文.txt' -Value 'unicode'
      Move-Item -LiteralPath 'folder' -Destination 'moved'
      Remove-Item -LiteralPath 'deleted.txt'
      Rename-Item -LiteralPath 'a.txt' -NewName 'renamed.txt'
      if ((Get-Content -LiteralPath 'renamed.txt' -Raw) -ne 'abXYefgh') {throw 'unexpected range contents'}
    "#);
    assert_eq!(fs::read(s.project.join("renamed.txt")).unwrap(),b"abXYefgh");
    assert!(!s.project.join("deleted.txt").exists());
    assert!(s.action("diff",&["--json"])["pending_changes"].as_u64().unwrap()>0);
    s.action("abort",&[]);
    assert_eq!(fs::read(s.project.join("a.txt")).unwrap(),b"abcdefgh");
    assert_eq!(fs::read(s.project.join("deleted.txt")).unwrap(),b"restore me");
    assert!(s.project.join("folder/child.txt").is_file());
    assert_eq!(fs::read(s.project.join("folder/existing/deeper/input.txt")).unwrap(),b"existing nested file");
    assert!(!s.project.join("folder/existing/deeper/new.txt").exists());
    for name in ["new.txt","sub","moved","renamed.txt"] {assert!(!s.project.join(name).exists(),"{name}");}

    s.write("a.txt","checkpoint");
    let checkpoint=s.action("checkpoint",&["--label","saved"])["id"].as_str().unwrap().to_owned();
    s.powershell("$f=[IO.File]::OpenWrite('a.txt'); try {$f.SetLength(2)} finally {$f.Dispose()}; Add-Content -LiteralPath 'a.txt' -Value 'append'");
    s.action("abort",&["--checkpoint",&checkpoint]);
    assert_eq!(fs::read(s.project.join("a.txt")).unwrap(),b"checkpoint");
    s.action("commit",&[]);
    s.write(s.project.join("a.txt").to_str().unwrap(),"after commit");
    let status=s.json(&["session","status",&s.id,"--project",s.project.to_str().unwrap()]);
    let pid=status["helper_pid"].as_u64().unwrap();
    let killed=Command::new("taskkill.exe").args(["/F","/PID",&pid.to_string()]).output().unwrap();
    assert!(killed.status.success(),"{}",String::from_utf8_lossy(&killed.stderr));
    let reopened=s.json(&["session","status",&s.id,"--project",s.project.to_str().unwrap()]);
    assert_ne!(reopened["helper_pid"],pid);
    assert_eq!(fs::read(s.project.join("a.txt")).unwrap(),b"after commit");
    s.action("abort",&[]);
    assert_eq!(fs::read(s.project.join("a.txt")).unwrap(),b"checkpoint");

    let project_state=fs::read_dir(s.root.path().join("state/projects")).unwrap().next().unwrap().unwrap().path();
    let state=project_state.join(&s.id).join("session.json");
    let script=format!(r#"
      foreach($p in @({source},{state},'.env','.ssh/id_rsa')) {{
        try {{[void][IO.File]::ReadAllBytes($p);throw "unexpected read: $p"}}
        catch [UnauthorizedAccessException] {{}}
      }}
      try {{[IO.File]::WriteAllText({outside},'escaped');throw 'unexpected outside write'}}
      catch [UnauthorizedAccessException] {{}}
      Set-Content -LiteralPath 'allowed.txt' -Value 'inside'
      Write-Output 'isolation-ok'
    "#,source=quoted(&s.project.join("a.txt")),state=quoted(&state),outside=quoted(&s.root.path().join("outside.txt")));
    assert!(s.powershell(&script)["stdout"].as_str().unwrap().contains("isolation-ok"));
    assert!(s.project.join("allowed.txt").exists());
    assert!(!s.root.path().join("outside.txt").exists());
    s.action("abort",&[]);
    let out=s.raw(&["session","start",s.project.to_str().unwrap(),"--fs-mode","copy-private"]);
    assert!(!out.status.success());assert!(String::from_utf8_lossy(&out.stderr).contains("reserved"));
    let out=s.raw(&["session","start",s.project.to_str().unwrap(),"--durable"]);
    assert!(!out.status.success());assert!(String::from_utf8_lossy(&out.stderr).contains("strict durability"));
}

#[test]
#[ignore = "requires installed WinFsp and native MXC PSEC filesystem-deny support; run explicitly on a supported Windows host"]
fn private_sessions_isolate_publish_sync_and_recover() {
    private_sessions_lifecycle(false);
}

#[test]
#[ignore = "requires installed WinFsp, local NTFS and native MXC PSEC filesystem-deny support"]
fn strict_private_sessions_isolate_publish_sync_and_recover() {
    private_sessions_lifecycle(true);
}

fn private_sessions_lifecycle(durable: bool) {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("private project with spaces");
    fs::create_dir(&project).unwrap();
    fs::write(project.join("a.txt"), b"old").unwrap();
    fs::write(project.join("b.txt"), b"old").unwrap();
    let mut s = Session {root, project, id: String::new()};
    let start = |s: &Session| {
        let mut args = vec!["session", "start", s.project.to_str().unwrap(), "--fs-mode", "cbcow-private", "--require-fs-mode", "--json"];
        if durable { args.push("--durable"); }
        s.json(&args)
    };
    let info = start(&s);
    s.id = info["session_id"].as_str().unwrap().into();
    assert_eq!(info["resolved_fs_mode"], "cbcow-private");
    assert_eq!(info["host_visibility"], "on-commit");
    assert_eq!(info["rollback_guarantee"], "private-live-tree");
    assert_eq!(info["durability"], if durable {"strict"} else {"process-crash"});
    let sibling = start(&s)["session_id"].as_str().unwrap().to_owned();
    // Keep the sibling in a guard as well, so failures always stop both helpers.
    struct StopSession<'a> { session: &'a Session, id: &'a str }
    impl Drop for StopSession<'_> {
        fn drop(&mut self) {
            let _ = self.session.raw(&["session", "stop", self.id, "--project", self.session.project.to_str().unwrap()]);
        }
    }
    let sibling_guard = StopSession { session: &s, id: &sibling };
    let sibling_action = |action: &str, extra: &[&str]| {
        let mut args = vec![action, &sibling, "--project", s.project.to_str().unwrap()];
        args.extend_from_slice(extra);
        s.json(&args)
    };
    let through = s.raw(&["session", "start", s.project.to_str().unwrap(), "--fs-mode", "cbcow-through"]);
    assert!(!through.status.success());
    s.write("a.txt", "ours");
    if durable {
        for script in [
            "[IO.File]::WriteAllText('native.txt','native strict')",
            "New-Item -ItemType Directory -Path 'native-dir' | Out-Null; [IO.File]::WriteAllText('native-dir/child.txt','child')",
            "Move-Item -LiteralPath 'native-dir' -Destination 'renamed-dir'",
            "Remove-Item -LiteralPath 'native.txt'",
            "[IO.File]::SetAttributes('a.txt',[IO.FileAttributes]::ReadOnly)",
            "[IO.File]::SetAttributes('a.txt',[IO.FileAttributes]::Normal)",
        ] { s.powershell(script); }
        assert!(!s.project.join("renamed-dir").exists());
    }
    let view = PathBuf::from(info["mountpoint"].as_str().unwrap());
    assert_eq!(fs::read(view.join("a.txt")).unwrap(), b"ours");
    assert_eq!(fs::read(s.project.join("a.txt")).unwrap(), b"old");
    let out = s.raw(&["write", &sibling, "--project", s.project.to_str().unwrap(), "b.txt", "--content", "sibling"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    s.action("commit", &[]);
    assert_eq!(sibling_action("commit", &[])["folded_publications"], 1);
    assert_eq!(fs::read(s.project.join("a.txt")).unwrap(), b"ours");
    assert_eq!(fs::read(s.project.join("b.txt")).unwrap(), b"sibling");
    s.write("b.txt", "overlap");
    let conflict = s.raw(&["commit", &s.id, "--project", s.project.to_str().unwrap()]);
    assert!(!conflict.status.success());
    assert!(String::from_utf8_lossy(&conflict.stderr).contains("host paths changed"));
    let sync = s.action("sync", &[]);
    assert_eq!(sync["conflicts"][0]["path"], "b.txt");
    assert_eq!(fs::read(view.join("b.txt.deltabox-theirs")).unwrap(), b"sibling");
    s.action("commit", &[]);
    assert_eq!(fs::read(s.project.join("b.txt")).unwrap(), b"overlap");
    assert!(!s.project.join("b.txt.deltabox-theirs").exists());
    s.write("a.txt", "pending");
    let status = s.json(&["session", "status", &s.id, "--project", s.project.to_str().unwrap()]);
    let killed = Command::new("taskkill.exe").args(["/F", "/PID", &status["helper_pid"].to_string()]).output().unwrap();
    assert!(killed.status.success());
    s.action("abort", &[]);
    assert_eq!(fs::read(s.project.join("a.txt")).unwrap(), b"ours");
    let status = s.json(&["session", "status", &s.id, "--project", s.project.to_str().unwrap()]);
    let reopened_view = PathBuf::from(status["mountpoint"].as_str().unwrap());
    assert_eq!(fs::read(reopened_view.join("a.txt")).unwrap(), b"ours");
    fs::write(s.project.join("a.txt"), b"external").unwrap();
    s.write("a.txt", "should conflict");
    assert!(!s.raw(&["commit", &s.id, "--project", s.project.to_str().unwrap(), "--ours"]).status.success());
    s.action("abort", &[]);
    let script = format!(r#"
      foreach($p in @({host},{live})) {{
        try {{[void][IO.File]::ReadAllBytes($p);throw "unexpected read: $p"}}
        catch [UnauthorizedAccessException] {{}}
      }}
      [IO.File]::WriteAllText('sandbox.txt','private')
    "#, host = quoted(&s.project.join("a.txt")), live = {
        let project_state = fs::read_dir(s.root.path().join("state/projects")).unwrap().next().unwrap().unwrap().path();
        quoted(&project_state.join(&s.id).join("live/a.txt"))
    });
    s.powershell(&script);
    assert!(!s.project.join("sandbox.txt").exists());
    sibling_action("abort", &[]);
    s.json(&["session", "stop", &sibling, "--project", s.project.to_str().unwrap()]);
    drop(sibling_guard);
}
