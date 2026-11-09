//! `box-chrome-pipe` fails closed: with no `boxsvc` user to hand the pipe to (any machine but the
//! box), the program starts as it would have, with nothing on fd 3 and no flag added.

use std::process::Command;

#[test]
fn with_no_service_user_the_program_runs_plain() {
    if std::path::Path::new("/etc/passwd").exists()
        && std::fs::read_to_string("/etc/passwd")
            .unwrap_or_default()
            .lines()
            .any(|l| l.starts_with("boxsvc:"))
    {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cdp.sock");
    let out = Command::new(env!("CARGO_BIN_EXE_box-chrome-pipe"))
        .arg(&socket)
        .arg("--")
        .args([
            "/bin/sh",
            "-c",
            "if [ -e /dev/fd/3 ]; then echo piped; else echo plain; fi; echo \"$#\"",
            "sh",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "plain\n0\n");
    assert!(String::from_utf8_lossy(&out.stderr).contains("no DevTools pipe"));
    assert!(!socket.exists());
}

#[test]
fn a_bad_call_is_refused() {
    let out = Command::new(env!("CARGO_BIN_EXE_box-chrome-pipe"))
        .arg("/nowhere/cdp.sock")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}
