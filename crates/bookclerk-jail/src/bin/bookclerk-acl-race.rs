//! Cross-process ACL grant/revoke helper for Windows integration tests.
//!
//! Repeatedly grants then revokes a Package SID ACE on a shared directory so
//! two independent processes can race the DACL RMW path.

#![cfg_attr(windows, allow(unsafe_code))]

use std::process::ExitCode;

fn main() -> ExitCode {
    #[cfg(not(windows))]
    {
        eprintln!("bookclerk-acl-race: Windows only");
        ExitCode::from(2)
    }
    #[cfg(windows)]
    {
        match run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("bookclerk-acl-race: {err}");
                ExitCode::from(1)
            }
        }
    }
}

#[cfg(windows)]
fn run() -> Result<(), String> {
    use std::env;
    use std::path::PathBuf;
    use std::thread;
    use std::time::Duration;

    let mut args = env::args().skip(1);
    let mut dir = None;
    let mut sid = None;
    let mut rounds = 10u32;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dir" => dir = Some(PathBuf::from(args.next().ok_or("--dir needs path")?)),
            "--sid" => sid = Some(args.next().ok_or("--sid needs value")?),
            "--rounds" => {
                rounds = args
                    .next()
                    .ok_or("--rounds needs value")?
                    .parse()
                    .map_err(|err| format!("bad rounds: {err}"))?;
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    let dir = dir.ok_or("missing --dir")?;
    let sid = sid.ok_or("missing --sid")?;

    for i in 0..rounds {
        let grant = bookclerk_sandbox::spawn::grant_path_access(&sid, &dir, true)
            .map_err(|err| format!("grant[{i}]: {err}"))?;
        // Brief overlap window so a peer can grant concurrently.
        thread::sleep(Duration::from_millis(5));
        if !bookclerk_sandbox::spawn::dacl_mentions_sid(&dir, &sid)
            .map_err(|err| format!("dacl check[{i}]: {err}"))?
        {
            return Err(format!("grant[{i}] did not appear in DACL"));
        }
        drop(grant); // revoke
                     // After revoke, our SID should be gone — unless a peer still holds it
                     // (different SID). Our SID must never be resurrected after we revoke.
        thread::sleep(Duration::from_millis(5));
        if bookclerk_sandbox::spawn::dacl_mentions_sid(&dir, &sid)
            .map_err(|err| format!("dacl after revoke[{i}]: {err}"))?
        {
            // Another round may have re-granted in this process only if we
            // loop — immediately after drop it must be absent before the next
            // grant in this process.
            return Err(format!("sid resurrected or not revoked after drop[{i}]"));
        }
    }
    println!("{{\"ok\":true,\"rounds\":{rounds}}}");
    Ok(())
}
