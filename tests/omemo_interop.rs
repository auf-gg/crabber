//! OMEMO interop against python-oldmemo and python-twomemo (tests/omemo_echo_bot.py) on the local ejabberd.
//!   pip install slixmpp slixmpp-omemo "oldmemo[xml]" "twomemo[xml]"
//!   OMEMO_PY=<python with those packages> cargo test --test omemo_interop -- --ignored --test-threads=1
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use crabber::xmpp::{self, Account, Cmd, Ev};
use tokio_xmpp::jid::BareJid;

fn reset(users: &[&str]) {
    for user in users {
        let _ = Command::new("docker").args(["exec", "ejabberd-test", "ejabberdctl", "unregister", user, "localhost"]).status();
        let ok = Command::new("docker").args(["exec", "ejabberd-test", "ejabberdctl", "register", user, "localhost", user]).status().is_ok_and(|s| s.success());
        assert!(ok, "register {user}");
    }
}

/// Fresh account directory, so every run starts with a new OMEMO identity.
fn login(user: &str) -> (UnboundedSender<Cmd>, UnboundedReceiver<Ev>) {
    let _ = tokio_xmpp::rustls::crypto::ring::default_provider().install_default();
    let dir = std::env::temp_dir().join(format!("re-omemo-{user}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("avatars")).unwrap();
    xmpp::connect(Account {
        jid: BareJid::from_str(&format!("{user}@localhost")).unwrap(),
        password: user.into(),
        host: Some("127.0.0.1".into()),
        port: 5222,
        insecure: true,
        dir,
        device_id: None,
        fast: None,
    })
}

fn wait<T>(rx: &mut UnboundedReceiver<Ev>, secs: u64, mut f: impl FnMut(&Ev) -> Option<T>) -> T {
    let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(secs), async {
            loop {
                let ev = rx.next().await.expect("channel closed");
                if std::env::var("VERBOSE").is_ok() { eprintln!("  ev: {}", format!("{ev:?}").chars().take(300).collect::<String>()); }
                if let Ev::Error(e) | Ev::SendFailed { error: e, .. } = &ev {
                    eprintln!("  !! {e}");
                }
                if let Some(v) = f(&ev) {
                    return v;
                }
            }
        })
        .await
        .expect("timed out")
    })
}

struct Bot(Child);
impl Drop for Bot {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_bot() -> Bot {
    let py = std::env::var("OMEMO_PY").unwrap_or_else(|_| "python".into());
    let mut child = Command::new(py)
        .args(["-u", "tests/omemo_echo_bot.py"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start python echo bot");
    let stdout = child.stdout.take().unwrap();
    // Owned by Bot right away so it is killed on every path, including a failed start.
    let bot = Bot(child);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            eprintln!("  bot: {line}");
            let _ = tx.send(line);
        }
    });
    loop {
        let line = rx.recv_timeout(Duration::from_secs(30)).expect("bot did not become ready");
        if line == "READY" {
            return bot;
        }
    }
}

fn send(tx: &UnboundedSender<Cmd>, to: &str, body: &str, enc: &str) -> String {
    let id = xmpp::new_id();
    tx.unbounded_send(Cmd::Send { to: to.into(), id: id.clone(), body: body.into(), groupchat: false, enc: enc.into(), reply: None }).unwrap();
    id
}

#[test]
#[ignore]
fn omemo_with_python_oldmemo_twomemo_and_between_our_clients() {
    reset(&["alice", "bob", "carol"]);
    let _bot = start_bot();
    let (a, mut a_rx) = login("alice");
    let (b, mut b_rx) = login("bob");
    wait(&mut a_rx, 15, |e| matches!(e, Ev::Online { .. }).then_some(()));
    wait(&mut b_rx, 15, |e| matches!(e, Ev::Online { .. }).then_some(()));
    std::thread::sleep(Duration::from_secs(2)); // bundles and device lists published

    // 1. Rust -> python -> Rust, twice per version (key exchange, then a normal message). The bot
    // has both versions, so "auto" picks OMEMO 2, and a chat never set ("") uses legacy; the bot
    // answers with the version it got.
    for (enc, echo) in [("auto", "echo2"), ("", "echo")] {
        for text in ["Привет, OMEMO!", "второе сообщение"] {
            let id = send(&a, "carol@localhost", text, enc);
            wait(&mut a_rx, 20, |e| matches!(e, Ev::SentEncrypted { id: i, enc: 1, .. } if *i == id).then_some(()));
            let got = wait(&mut a_rx, 20, |e| match e {
                Ev::Message { peer, msg, .. } if peer == "carol@localhost" && !msg.outgoing => Some(msg.clone()),
                _ => None,
            });
            assert_eq!(got.body, format!("{echo}: {text}"));
            assert_eq!(got.enc, 1, "reply was OMEMO-encrypted");
        }
    }

    // 2. Between two of our clients, both directions; OMEMO 2, then legacy.
    let id = send(&b, "alice@localhost", "от боба", "omemo2");
    wait(&mut b_rx, 20, |e| matches!(e, Ev::SentEncrypted { id: i, .. } if *i == id).then_some(()));
    let got = wait(&mut a_rx, 20, |e| match e {
        Ev::Message { peer, msg, .. } if peer == "bob@localhost" && !msg.outgoing => Some(msg.clone()),
        _ => None,
    });
    assert_eq!((got.body.as_str(), got.enc), ("от боба", 1));
    for (text, enc) in [("ответ алисы", "auto"), ("старый OMEMO", "omemo-legacy")] {
        send(&a, "bob@localhost", text, enc);
        let got = wait(&mut b_rx, 20, |e| match e {
            Ev::Message { peer, msg, .. } if peer == "alice@localhost" && !msg.outgoing => Some(msg.clone()),
            _ => None,
        });
        assert_eq!((got.body.as_str(), got.enc), (text, 1));
    }

    // 3. python starts the session (key exchange) with each version; we answer with an empty OMEMO
    // message, which the bot decrypts and reports in plain text.
    for (cmd, greeting, empty) in [("kex2", "привет от бота 2", "empty2"), ("kex", "привет от бота", "empty")] {
        send(&b, "carol@localhost", cmd, "plain");
        let mut next = || {
            wait(&mut b_rx, 20, |e| match e {
                Ev::Message { peer, msg, .. } if peer == "carol@localhost" && !msg.outgoing => Some(msg.clone()),
                _ => None,
            })
        };
        let got = next();
        assert_eq!((got.body.as_str(), got.enc), (greeting, 1));
        let got = next();
        assert_eq!((got.body.as_str(), got.enc), (empty, 0), "the bot decrypted our empty OMEMO message");
    }

    // 4. Encrypted file (XEP-0454 aesgcm://).
    let file = std::env::temp_dir().join("re-omemo-file.txt");
    std::fs::write(&file, "секретный файл").unwrap();
    let fid = xmpp::new_id();
    a.unbounded_send(Cmd::Upload { to: "bob@localhost".into(), id: fid.clone(), path: file, groupchat: false, enc: "omemo-legacy".into() }).unwrap();
    let url = wait(&mut b_rx, 30, |e| match e {
        Ev::Message { msg, .. } if msg.msg_id == fid => msg.url.clone(),
        _ => None,
    });
    assert!(url.starts_with("aesgcm://"), "{url}");
    let target = std::env::temp_dir().join("re-omemo-file-dl.txt");
    b.unbounded_send(Cmd::Download { url: url.clone(), path: target.clone() }).unwrap();
    wait(&mut b_rx, 20, |e| matches!(e, Ev::Downloaded { .. }).then_some(()));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "секретный файл");
    // The server only ever saw ciphertext.
    let https = url.replacen("aesgcm://", "https://", 1);
    let https = https.split('#').next().unwrap();
    let out = Command::new("curl").args(["-sk", https]).output().unwrap();
    assert_ne!(out.stdout, "секретный файл".as_bytes());
}
