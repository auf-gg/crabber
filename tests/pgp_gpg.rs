//! XEP-0027 payloads against real GnuPG, in a throwaway GNUPGHOME:
//!   cargo test --test pgp_gpg -- --ignored
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crabber::pgp;

/// Git for Windows ships an MSYS gpg that wants /c/... paths.
fn posix(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    match s.as_bytes() {
        [d, b':', ..] => format!("/{}{}", (*d as char).to_ascii_lowercase(), &s[2..]),
        _ => s,
    }
}

fn gpg(home: &Path, args: &[&str], stdin: Option<&[u8]>) -> Vec<u8> {
    let mut child = Command::new(std::env::var("GPG").unwrap_or_else(|_| "gpg".into()))
        .env("GNUPGHOME", posix(home))
        .args(["--batch", "--yes", "--quiet", "--trust-model", "always", "--pinentry-mode", "loopback", "--passphrase", ""])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("gpg not found (set GPG=path)");
    child.stdin.take().unwrap().write_all(stdin.unwrap_or_default()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "gpg {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

fn home() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("re-gpg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
#[ignore]
fn interop_with_gnupg() {
    let home = home();
    // gpg-side identity.
    gpg(&home, &["--quick-gen-key", "gpg user <gpg@localhost>", "default", "default", "never"], None);
    let gpg_pub = String::from_utf8(gpg(&home, &["--armor", "--export", "gpg@localhost"], None)).unwrap();
    let gpg_key = pgp::parse_public(&gpg_pub).unwrap();

    // Our identity, imported into gpg.
    let ours = pgp::generate("ours <xmpp:ours@localhost>").unwrap();
    gpg(&home, &["--import"], Some(pgp::armor_public(&pgp::public_of(&ours)).as_bytes()));

    // us -> gpg
    let payload = pgp::encrypt("Привет из Rust", &[&gpg_key]).unwrap();
    let plain = gpg(&home, &["--decrypt"], Some(pgp::add_armor("MESSAGE", &payload).as_bytes()));
    assert_eq!(String::from_utf8(plain).unwrap(), "Привет из Rust");

    // gpg -> us
    let armored = gpg(&home, &["--armor", "--encrypt", "-r", "ours@localhost"], Some("Привет от GnuPG".as_bytes()));
    let payload = pgp::strip_armor(&String::from_utf8(armored).unwrap());
    assert_eq!(pgp::decrypt(&payload, &ours).unwrap(), "Привет от GnuPG");

    // Our presence signature verifies in gpg, and gpg's in ours.
    let sig = pgp::sign_status("Online", &ours).unwrap();
    std::fs::write(home.join("status.txt"), "Online").unwrap();
    std::fs::write(home.join("status.sig"), pgp::add_armor("SIGNATURE", &sig)).unwrap();
    gpg(&home, &["--verify", &posix(&home.join("status.sig")), &posix(&home.join("status.txt"))], None);
    let gpg_sig = gpg(&home, &["--armor", "--detach-sign", "-u", "gpg@localhost"], Some(b"Away"));
    let gpg_sig = pgp::strip_armor(&String::from_utf8(gpg_sig).unwrap());
    assert!(pgp::verify_status("Away", &gpg_sig, &gpg_key));
    assert!(pgp::key_ids(&gpg_key).contains(&pgp::signature_issuer(&gpg_sig).unwrap()));
    let _ = Command::new("gpgconf").env("GNUPGHOME", posix(&home)).args(["--kill", "gpg-agent"]).status();
    let _ = std::fs::remove_dir_all(&home);
}
