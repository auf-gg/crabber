//! Against a local ejabberd in Docker:
//!   docker run -d --name ejabberd-test -p 5222:5222 -p 5443:5443 -p 5478:5478/udp ghcr.io/processone/ejabberd
//!   cargo test --test live -- --ignored --test-threads=1
//! The tests (re)create the accounts alice/alice and bob/bob themselves.
use std::str::FromStr;
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use crabber::xmpp::{self, Account, Cmd, Ev};
use tokio_xmpp::jid::BareJid;

/// Recreates the test accounts so every run starts from an empty roster and archive.
fn reset_accounts() {
    for user in ["alice", "bob"] {
        for args in [vec!["unregister", user, "localhost"], vec!["register", user, "localhost", user]] {
            let ok = std::process::Command::new("docker")
                .args(["exec", "ejabberd-test", "ejabberdctl"])
                .args(&args)
                .status()
                .is_ok_and(|s| s.success());
            assert!(ok || args[0] == "unregister", "ejabberdctl {args:?} failed");
        }
    }
}

fn login(user: &str) -> (UnboundedSender<Cmd>, UnboundedReceiver<Ev>) {
    login_as(user, user)
}

/// `profile` names the local data dir, so one account can run as several devices.
fn login_as(user: &str, profile: &str) -> (UnboundedSender<Cmd>, UnboundedReceiver<Ev>) {
    let _ = tokio_xmpp::rustls::crypto::ring::default_provider().install_default();
    let dir = std::env::temp_dir().join(format!("re-test-{profile}"));
    let _ = std::fs::create_dir_all(dir.join("avatars"));
    let _ = std::fs::create_dir_all(dir.join("files"));
    xmpp::connect(Account {
        jid: BareJid::from_str(&format!("{user}@localhost")).unwrap(),
        password: user.into(),
        host: Some("127.0.0.1".into()),
        port: 5222,
        insecure: true,
        dir,
    })
}

/// Waits for the first event matching `f`, failing after 15 s.
fn wait<T>(rx: &mut UnboundedReceiver<Ev>, mut f: impl FnMut(&Ev) -> Option<T>) -> T {
    let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let ev = rx.next().await.expect("channel closed");
                eprintln!("  ev: {ev:?}");
                if let Ev::Disconnected(e) = &ev {
                    panic!("disconnected: {e}");
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

#[test]
#[ignore]
fn chat_roundtrip() {
    reset_accounts();
    let (a, mut a_rx) = login("alice");
    let (b, mut b_rx) = login("bob");
    wait(&mut a_rx, |e| matches!(e, Ev::Online { .. }).then_some(()));
    wait(&mut b_rx, |e| matches!(e, Ev::Online { .. }).then_some(()));
    a.unbounded_send(Cmd::Sync { mam_after: None, rooms: vec![] }).unwrap();

    // Roster + subscription: alice adds bob, bob approves.
    a.unbounded_send(Cmd::AddContact { jid: "bob@localhost".into(), name: "Bob".into() }).unwrap();
    let from = wait(&mut b_rx, |e| match e {
        Ev::SubscribeRequest { jid } => Some(jid.clone()),
        _ => None,
    });
    assert_eq!(from, "alice@localhost");
    b.unbounded_send(Cmd::Approve { jid: from }).unwrap();
    wait(&mut a_rx, |e| match e {
        Ev::RosterItem(c) if c.jid == "bob@localhost" && (c.subscription == "to" || c.subscription == "both") => Some(()),
        _ => None,
    });

    // Message + delivery receipt + read marker.
    let id = xmpp::new_id();
    a.unbounded_send(Cmd::Send { to: "bob@localhost".into(), id: id.clone(), body: "привет".into(), groupchat: false, enc: "plain".into(), reply: None }).unwrap();
    let got = wait(&mut b_rx, |e| match e {
        Ev::Message { peer, msg, .. } if !msg.outgoing => Some((peer.clone(), msg.clone())),
        _ => None,
    });
    assert_eq!(got.0, "alice@localhost");
    assert_eq!(got.1.body, "привет");
    assert_eq!(got.1.msg_id, id);
    wait(&mut a_rx, |e| matches!(e, Ev::Receipt { id: r, .. } if *r == id).then_some(()));
    b.unbounded_send(Cmd::Displayed { to: "alice@localhost".into(), id: id.clone() }).unwrap();
    wait(&mut a_rx, |e| matches!(e, Ev::Displayed { id: r, by_me: false, .. } if *r == id).then_some(()));

    // Typing notification.
    b.unbounded_send(Cmd::Typing { to: "alice@localhost".into(), on: true }).unwrap();
    wait(&mut a_rx, |e| matches!(e, Ev::Typing { on: true, .. }).then_some(()));

    // MAM: a fresh session of bob gets the message from the archive.
    drop(b);
    let (b2, mut b2_rx) = login("bob");
    wait(&mut b2_rx, |e| matches!(e, Ev::Online { .. }).then_some(()));
    b2.unbounded_send(Cmd::Sync { mam_after: None, rooms: vec![] }).unwrap();
    wait(&mut b2_rx, |e| match e {
        Ev::Message { msg, archive_id: Some(_), .. } if msg.msg_id == id => Some(()),
        _ => None,
    });

    // Group chat: alice creates a room, invites bob, bob joins and receives a message.
    let room = format!("g{}@conference.localhost", xmpp::new_id());
    a.unbounded_send(Cmd::Join { room: room.clone(), nick: "alice".into(), since: None, create_name: Some("Тест".into()) }).unwrap();
    wait(&mut a_rx, |e| matches!(e, Ev::RoomJoined { room: r, .. } if *r == room).then_some(()));
    a.unbounded_send(Cmd::Invite { room: room.clone(), jid: "bob@localhost".into() }).unwrap();
    let invited = wait(&mut b2_rx, |e| match e {
        Ev::Invite { room, .. } => Some(room.clone()),
        _ => None,
    });
    assert_eq!(invited, room);
    b2.unbounded_send(Cmd::Join { room: room.clone(), nick: "bob".into(), since: None, create_name: None }).unwrap();
    wait(&mut b2_rx, |e| matches!(e, Ev::RoomJoined { occupants: 2, .. }).then_some(()));
    let gid = xmpp::new_id();
    a.unbounded_send(Cmd::Send { to: room.clone(), id: gid.clone(), body: "всем привет".into(), groupchat: true, enc: "plain".into(), reply: None }).unwrap();
    let m = wait(&mut b2_rx, |e| match e {
        Ev::Message { msg, groupchat: true, .. } if msg.msg_id == gid => Some(msg.clone()),
        _ => None,
    });
    assert_eq!((m.sender.as_str(), m.outgoing), ("alice", false));
    let echo = wait(&mut a_rx, |e| match e {
        Ev::Message { msg, groupchat: true, .. } if msg.msg_id == gid => Some(msg.outgoing),
        _ => None,
    });
    assert!(echo, "own MUC echo is marked outgoing");

    // HTTP upload.
    let file = std::env::temp_dir().join("re-upload.txt");
    std::fs::write(&file, b"hello file").unwrap();
    std::thread::sleep(Duration::from_secs(1)); // upload service discovery runs after login
    let fid = xmpp::new_id();
    a.unbounded_send(Cmd::Upload { to: "bob@localhost".into(), id: fid.clone(), path: file, groupchat: false, enc: "plain".into() }).unwrap();
    let url = wait(&mut a_rx, |e| match e {
        Ev::Uploaded { url, .. } => Some(url.clone()),
        Ev::SendFailed { error, .. } => panic!("upload failed: {error}"),
        _ => None,
    });
    let got = wait(&mut b2_rx, |e| match e {
        Ev::Message { msg, .. } if msg.msg_id == fid => Some(msg.url.clone()),
        _ => None,
    });
    assert_eq!(got.as_deref(), Some(url.as_str()));
    let target = std::env::temp_dir().join("re-test-bob").join("downloaded.txt");
    b2.unbounded_send(Cmd::Download { url: url.clone(), path: target.clone() }).unwrap();
    wait(&mut b2_rx, |e| matches!(e, Ev::Downloaded { .. }).then_some(()));
    assert_eq!(std::fs::read(&target).unwrap(), b"hello file");
}

#[test]
#[ignore]
fn audio_call_connects() {
    use crabber::call::{CallCmd, CallEv, CallState};
    let (a, mut a_rx) = login("alice");
    let (b, mut b_rx) = login("bob");
    wait(&mut a_rx, |e| matches!(e, Ev::Online { .. }).then_some(()));
    wait(&mut b_rx, |e| matches!(e, Ev::Online { .. }).then_some(()));
    std::thread::sleep(Duration::from_millis(500)); // initial presence

    a.unbounded_send(Cmd::Call(CallCmd::Start { to: "bob@localhost".into() })).unwrap();
    let call_state = |e: &Ev| match e {
        Ev::Call(CallEv::State { state, .. }) => Some(state.clone()),
        _ => None,
    };
    assert_eq!(wait(&mut b_rx, call_state), CallState::Incoming);
    b.unbounded_send(Cmd::Call(CallCmd::Accept)).unwrap();
    for (name, rx) in [("alice", &mut a_rx), ("bob", &mut b_rx)] {
        let state = wait(rx, |e| match call_state(e) {
            Some(s @ (CallState::Active | CallState::Ended(_))) => Some(s),
            _ => None,
        });
        assert_eq!(state, CallState::Active, "{name}");
    }
    std::thread::sleep(Duration::from_secs(2)); // let media flow
    // Both sides send 50 packets/s; after 2 s each should have received most of them.
    let got = crabber::call::RTP_RECEIVED.load(std::sync::atomic::Ordering::Relaxed);
    assert!(got > 100, "only {got} RTP packets received");
    a.unbounded_send(Cmd::Call(CallCmd::Hangup)).unwrap();
    let end = wait(&mut b_rx, |e| match call_state(e) {
        Some(CallState::Ended(r)) => Some(r),
        _ => None,
    });
    assert_eq!(end, "", "remote hangup is a normal end");
}

#[test]
#[ignore]
fn openpgp_xep0027_roundtrip() {
    let (a, mut a_rx) = login("alice");
    let (b, mut b_rx) = login("bob");
    wait(&mut a_rx, |e| matches!(e, Ev::Online { .. }).then_some(()));
    wait(&mut b_rx, |e| matches!(e, Ev::Online { .. }).then_some(()));
    let own_key = |tx: &UnboundedSender<Cmd>, rx: &mut UnboundedReceiver<Ev>| {
        tx.unbounded_send(Cmd::PgpGenerate).unwrap();
        wait(rx, |e| match e {
            Ev::PgpOwn { public: Some(p), .. } => Some(p.clone()),
            _ => None,
        })
    };
    let (a_pub, b_pub) = (own_key(&a, &mut a_rx), own_key(&b, &mut b_rx));
    a.unbounded_send(Cmd::PgpImportContact { jid: "bob@localhost".into(), armored: b_pub }).unwrap();
    b.unbounded_send(Cmd::PgpImportContact { jid: "alice@localhost".into(), armored: a_pub }).unwrap();
    wait(&mut a_rx, |e| matches!(e, Ev::PgpContact { fingerprint: Some(_), .. }).then_some(()));
    wait(&mut b_rx, |e| matches!(e, Ev::PgpContact { fingerprint: Some(_), .. }).then_some(()));

    let id = xmpp::new_id();
    a.unbounded_send(Cmd::Send { to: "bob@localhost".into(), id: id.clone(), body: "секрет по PGP".into(), groupchat: false, enc: "pgp".into(), reply: None }).unwrap();
    wait(&mut a_rx, |e| matches!(e, Ev::SentEncrypted { id: i, enc: 2, .. } if *i == id).then_some(()));
    let got = wait(&mut b_rx, |e| match e {
        Ev::Message { msg, .. } if msg.msg_id == id => Some(msg.clone()),
        _ => None,
    });
    assert_eq!((got.body.as_str(), got.enc), ("секрет по PGP", 2));

    // Files cannot be sent with XEP-0027: refused instead of leaking the plaintext file.
    let fid = xmpp::new_id();
    let file = std::env::temp_dir().join("re-pgp-file.txt");
    std::fs::write(&file, "x").unwrap();
    a.unbounded_send(Cmd::Upload { to: "bob@localhost".into(), id: fid.clone(), path: file, groupchat: false, enc: "pgp".into() }).unwrap();
    wait(&mut a_rx, |e| matches!(e, Ev::SendFailed { id: i, .. } if *i == fid).then_some(()));
}

#[test]
#[ignore]
fn omemo_crabber_to_crabber() {
    reset_accounts();
    for u in ["alice", "bob"] {
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("re-test-{u}")));
    }
    let (a, mut a_rx) = login("alice");
    let (b, mut b_rx) = login("bob");
    wait(&mut a_rx, |e| matches!(e, Ev::Online { .. }).then_some(()));
    wait(&mut b_rx, |e| matches!(e, Ev::Online { .. }).then_some(()));
    std::thread::sleep(Duration::from_secs(2)); // let both publish devicelist + bundle

    let send = |tx: &UnboundedSender<Cmd>, rx: &mut UnboundedReceiver<Ev>, to: &str, body: &str| {
        let id = xmpp::new_id();
        tx.unbounded_send(Cmd::Send { to: to.into(), id: id.clone(), body: body.into(), groupchat: false, enc: "omemo".into(), reply: None }).unwrap();
        wait(rx, |e| match e {
            Ev::SentEncrypted { id: i, .. } if *i == id => Some(()),
            Ev::SendFailed { id: i, error, .. } if *i == id => panic!("send failed: {error}"),
            _ => None,
        });
        id
    };
    let recv = |rx: &mut UnboundedReceiver<Ev>, id: &str| {
        wait(rx, |e| match e {
            Ev::Message { msg, .. } if msg.msg_id == id => Some((msg.body.clone(), msg.enc)),
            _ => None,
        })
    };
    for round in 0..3 {
        let id = send(&a, &mut a_rx, "bob@localhost", &format!("привет {round}"));
        assert_eq!(recv(&mut b_rx, &id), (format!("привет {round}"), 1));
        let id = send(&b, &mut b_rx, "alice@localhost", &format!("ответ {round}"));
        assert_eq!(recv(&mut a_rx, &id), (format!("ответ {round}"), 1));
    }
}

/// Same account on two devices (e.g. Windows + Linux), restart, offline delivery.
#[test]
#[ignore]
fn omemo_multi_device() {
    reset_accounts();
    for p in ["alice", "alice2", "bob"] {
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("re-test-{p}")));
    }
    let online = |rx: &mut UnboundedReceiver<Ev>| wait(rx, |e| matches!(e, Ev::Online { .. }).then_some(()));
    let (a, mut a_rx) = login_as("alice", "alice");
    online(&mut a_rx);
    let (b, mut b_rx) = login("bob");
    online(&mut b_rx);
    std::thread::sleep(Duration::from_secs(2));
    let send = |tx: &UnboundedSender<Cmd>, rx: &mut UnboundedReceiver<Ev>, to: &str, body: &str| {
        let id = xmpp::new_id();
        tx.unbounded_send(Cmd::Send { to: to.into(), id: id.clone(), body: body.into(), groupchat: false, enc: "omemo".into(), reply: None }).unwrap();
        wait(rx, |e| match e {
            Ev::SentEncrypted { id: i, .. } if *i == id => Some(()),
            Ev::SendFailed { id: i, error, .. } if *i == id => panic!("send failed: {error}"),
            _ => None,
        });
        id
    };
    let recv = |rx: &mut UnboundedReceiver<Ev>, id: &str| {
        wait(rx, |e| match e {
            Ev::Message { msg, .. } if msg.msg_id == id => Some((msg.body.clone(), msg.enc)),
            _ => None,
        })
    };
    eprintln!("== 1: first contact");
    let id = send(&a, &mut a_rx, "bob@localhost", "раз");
    assert_eq!(recv(&mut b_rx, &id), ("раз".into(), 1));

    eprintln!("== 2: alice adds a second device");
    let (a2, mut a2_rx) = login_as("alice", "alice2");
    online(&mut a2_rx);
    std::thread::sleep(Duration::from_secs(2));
    let id = send(&b, &mut b_rx, "alice@localhost", "два");
    assert_eq!(recv(&mut a_rx, &id), ("два".into(), 1), "old device");
    assert_eq!(recv(&mut a2_rx, &id), ("два".into(), 1), "new device");
    let id = send(&a2, &mut a2_rx, "bob@localhost", "три");
    assert_eq!(recv(&mut b_rx, &id), ("три".into(), 1));
    assert_eq!(recv(&mut a_rx, &id), ("три".into(), 1), "carbon to own other device");

    eprintln!("== 3: bob restarts");
    drop((b, b_rx));
    std::thread::sleep(Duration::from_secs(2));
    let (b, mut b_rx) = login("bob");
    online(&mut b_rx);
    let id = send(&a, &mut a_rx, "bob@localhost", "четыре");
    assert_eq!(recv(&mut b_rx, &id), ("четыре".into(), 1));
    assert_eq!(recv(&mut a2_rx, &id), ("четыре".into(), 1), "carbon of the first device's message");
    let id = send(&b, &mut b_rx, "alice@localhost", "пять");
    assert_eq!(recv(&mut a_rx, &id), ("пять".into(), 1));

    eprintln!("== 4: bob offline while alice writes");
    drop((b, b_rx));
    std::thread::sleep(Duration::from_secs(2));
    let id = send(&a, &mut a_rx, "bob@localhost", "шесть");
    let (b, mut b_rx) = login("bob");
    assert_eq!(recv(&mut b_rx, &id), ("шесть".into(), 1));
    let id = send(&b, &mut b_rx, "alice@localhost", "семь");
    assert_eq!(recv(&mut a_rx, &id), ("семь".into(), 1));
    assert_eq!(recv(&mut a2_rx, &id), ("семь".into(), 1));
}
