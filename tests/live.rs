//! Against a local ejabberd in Docker:
//!   docker run -d --name ejabberd-test -p 5222:5222 -p 5443:5443 -p 5478:5478/udp ghcr.io/processone/ejabberd
//!   cargo test --test live -- --ignored --test-threads=1
//! The tests (re)create the accounts alice/alice, bob/bob and carol/carol themselves.
//! Without Docker, EJABBERDCTL names the ejabberdctl command of a local server instead.
use std::str::FromStr;
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use crabber::xmpp::{self, Account, Cmd, Ev};
use tokio_xmpp::jid::BareJid;

/// Recreates the test accounts so every run starts from an empty roster and archive.
fn reset_accounts() {
    for user in ["alice", "bob", "carol"] {
        for args in [vec!["unregister", user, "localhost"], vec!["register", user, "localhost", user]] {
            let mut cmd = match std::env::var("EJABBERDCTL") {
                Ok(ctl) => std::process::Command::new(ctl),
                Err(_) => {
                    let mut c = std::process::Command::new("docker");
                    c.args(["exec", "ejabberd-test", "ejabberdctl"]);
                    c
                }
            };
            let ok = cmd.args(&args).status().is_ok_and(|s| s.success());
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
        device_id: None,
        fast: None,
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

    // Roster + subscription: alice adds bob, bob approves; the request carries her nick (XEP-0172).
    a.unbounded_send(Cmd::SetNick("Алиса".into())).unwrap();
    a.unbounded_send(Cmd::AddContact { jid: "bob@localhost".into(), name: "Bob".into() }).unwrap();
    let (from, nick) = wait(&mut b_rx, |e| match e {
        Ev::SubscribeRequest { jid, nick } => Some((jid.clone(), nick.clone())),
        _ => None,
    });
    assert_eq!(from, "alice@localhost");
    assert_eq!(nick.as_deref(), Some("Алиса"));
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
    a.unbounded_send(Cmd::Join { room: room.clone(), nick: "alice".into(), since: None, after: None, create_name: Some("Тест".into()) }).unwrap();
    wait(&mut a_rx, |e| matches!(e, Ev::RoomJoined { room: r, .. } if *r == room).then_some(()));
    a.unbounded_send(Cmd::Invite { room: room.clone(), jid: "bob@localhost".into() }).unwrap();
    let invited = wait(&mut b2_rx, |e| match e {
        Ev::Invite { room, .. } => Some(room.clone()),
        _ => None,
    });
    assert_eq!(invited, room);
    b2.unbounded_send(Cmd::Join { room: room.clone(), nick: "bob".into(), since: None, after: None, create_name: None }).unwrap();
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

/// XEP-0234 over XEP-0261: with no HTTP Upload (pretended), alice's file goes to bob directly;
/// bob takes the offer like a download and gets the same bytes, alice a delivery tick.
#[test]
#[ignore]
fn jingle_file_transfer() {
    reset_accounts();
    /// Pretends there is no HTTP Upload until the test ends (also when it fails), so the later
    /// tests in this run upload as usual. The clients read it after their roster, any time later.
    struct NoUpload;
    impl Drop for NoUpload {
        fn drop(&mut self) {
            // SAFETY: tests run one at a time (--test-threads=1).
            unsafe { std::env::remove_var("CRABBER_NO_HTTP_UPLOAD") };
        }
    }
    // SAFETY: tests run one at a time (--test-threads=1).
    unsafe { std::env::set_var("CRABBER_NO_HTTP_UPLOAD", "1") };
    let _no_upload = NoUpload;
    let (a, mut a_rx) = login("alice");
    let (b, mut b_rx) = login("bob");
    wait(&mut a_rx, |e| matches!(e, Ev::Online { .. }).then_some(()));
    wait(&mut b_rx, |e| matches!(e, Ev::Online { .. }).then_some(()));
    a.unbounded_send(Cmd::AddContact { jid: "bob@localhost".into(), name: String::new() }).unwrap();
    let from = wait(&mut b_rx, |e| match e {
        Ev::SubscribeRequest { jid, .. } => Some(jid.clone()),
        _ => None,
    });
    b.unbounded_send(Cmd::Approve { jid: from }).unwrap();
    // Presence again: what the server sends on approval alone does not always reach alice in time.
    b.unbounded_send(Cmd::SetShow(None)).unwrap();
    // alice must see bob online: the file goes to one of his resources.
    wait(&mut a_rx, |e| matches!(e, Ev::Presence { jid, online: true } if jid == "bob@localhost").then_some(()));

    // Three blocks, the last one short. Small: IBB goes through the server, whose default c2s
    // shaper allows 3 KB/s.
    let data: Vec<u8> = (0..16_484u32).map(|i| (i * 7 % 251) as u8).collect();
    let src = std::env::temp_dir().join("re-jingle-src.bin");
    std::fs::write(&src, &data).unwrap();
    let id = xmpp::new_id();
    a.unbounded_send(Cmd::Upload { to: "bob@localhost".into(), id: id.clone(), path: src, groupchat: false, enc: "plain".into() }).unwrap();

    let url = wait(&mut b_rx, |e| match e {
        Ev::Message { msg, .. } if msg.url.as_deref().is_some_and(|u| u.starts_with("jingle://")) => msg.url.clone(),
        _ => None,
    });
    assert!(url.ends_with("/re-jingle-src.bin"), "{url}");
    let dst = std::env::temp_dir().join("re-jingle-dst.bin");
    let _ = std::fs::remove_file(&dst);
    b.unbounded_send(Cmd::Download { url: url.clone(), path: dst.clone() }).unwrap();
    wait(&mut b_rx, |e| match e {
        Ev::Downloaded { url: u } if *u == url => Some(()),
        Ev::DownloadFailed { error, .. } => panic!("download failed: {error}"),
        _ => None,
    });
    assert_eq!(std::fs::read(&dst).unwrap(), data);
    wait(&mut a_rx, |e| match e {
        Ev::Uploaded { id: i, url, .. } if *i == id => {
            assert!(url.starts_with("jingle://"));
            Some(())
        }
        Ev::SendFailed { error, .. } => panic!("send failed: {error}"),
        _ => None,
    });
    wait(&mut a_rx, |e| matches!(e, Ev::Receipt { id: i, .. } if *i == id).then_some(()));
}

/// XEP-0421: bob writes as "x" and leaves; carol takes the nick "x" and tries to correct his
/// message. The room's occupant-ids differ, so the correction is refused. Needs
/// mod_muc_occupantid on the server.
#[test]
#[ignore]
fn occupant_id_guards_corrections() {
    reset_accounts();
    let (a, mut a_rx) = login("alice");
    wait(&mut a_rx, |e| matches!(e, Ev::Online { .. }).then_some(()));
    let room = format!("o{}@conference.localhost", xmpp::new_id());
    a.unbounded_send(Cmd::Join { room: room.clone(), nick: "alice".into(), since: None, after: None, create_name: Some("Ники".into()) }).unwrap();
    wait(&mut a_rx, |e| matches!(e, Ev::RoomJoined { room: r, .. } if *r == room).then_some(()));
    std::thread::sleep(Duration::from_secs(1)); // room config and disco after creation

    let say = |tx: &UnboundedSender<Cmd>, id: &str, body: &str| {
        tx.unbounded_send(Cmd::Send { to: room.clone(), id: id.into(), body: body.into(), groupchat: true, enc: "plain".into(), reply: None }).unwrap();
    };
    let (b, mut b_rx) = login("bob");
    wait(&mut b_rx, |e| matches!(e, Ev::Online { .. }).then_some(()));
    b.unbounded_send(Cmd::Join { room: room.clone(), nick: "x".into(), since: None, after: None, create_name: None }).unwrap();
    wait(&mut b_rx, |e| matches!(e, Ev::RoomJoined { room: r, .. } if *r == room).then_some(()));
    let id = xmpp::new_id();
    say(&b, &id, "оригинал");
    let original = wait(&mut a_rx, |e| match e {
        Ev::Message { msg, occupant, .. } if msg.msg_id == id => Some(occupant.clone()),
        _ => None,
    });
    let original = original.expect("the room stamps an occupant-id");
    b.unbounded_send(Cmd::Leave { room: room.clone() }).unwrap();
    std::thread::sleep(Duration::from_millis(500));

    let (c, mut c_rx) = login("carol");
    wait(&mut c_rx, |e| matches!(e, Ev::Online { .. }).then_some(()));
    c.unbounded_send(Cmd::Join { room: room.clone(), nick: "x".into(), since: None, after: None, create_name: None }).unwrap();
    wait(&mut c_rx, |e| matches!(e, Ev::RoomJoined { room: r, .. } if *r == room).then_some(()));
    c.unbounded_send(Cmd::Correct { to: room.clone(), replace: id.clone(), body: "подделка".into(), groupchat: true, enc: "plain".into() }).unwrap();
    let (sender, forged) = wait(&mut a_rx, |e| match e {
        Ev::Correct { id: i, sender, occupant, .. } if *i == id => Some((sender.clone(), occupant.clone())),
        _ => None,
    });
    assert_eq!(sender, "x", "same nick");
    assert_ne!(forged.as_ref(), Some(&original), "but someone else");

    // What the app then does with it.
    let store = crabber::store::Store::open(std::path::Path::new(":memory:")).unwrap();
    let msg = crabber::store::Msg { msg_id: id.clone(), sender: "x".into(), outgoing: false, body: "оригинал".into(), url: None, ts: 1, status: 0, enc: 0, edited: false, reply_to: None, retracted: false };
    store.insert_msg(&room, &msg);
    store.set_occupant(&room, &id, "x", &original);
    assert!(!store.correct(&room, &id, "x", "подделка", forged.as_deref()));
}

/// XEP-0388/0484: the password login hands out a FAST token; the next login works with the token
/// alone (the password given is wrong). Needs mod_auth_fast on the server.
#[test]
#[ignore]
fn fast_token_login() {
    reset_accounts();
    let _ = tokio_xmpp::rustls::crypto::ring::default_provider().install_default();
    let device = "0f8fad5b-d9cb-469f-a165-70867728950e".to_string();
    let connect = |password: &str, fast: Option<tokio_xmpp::FastToken>| {
        xmpp::connect(Account {
            jid: BareJid::from_str("alice@localhost").unwrap(),
            password: password.into(),
            host: Some("127.0.0.1".into()),
            port: 5222,
            insecure: true,
            dir: std::env::temp_dir().join("re-test-fast"),
            device_id: Some(device.clone()),
            fast,
        })
    };
    let (tx, mut rx) = connect("alice", None);
    let token = wait(&mut rx, |e| match e {
        Ev::FastToken(Some(t)) => Some(t.clone()),
        _ => None,
    });
    assert!(token.mechanism.starts_with("HT-SHA-256-"), "{token:?}");
    wait(&mut rx, |e| matches!(e, Ev::Online { .. }).then_some(()));
    drop((tx, rx));

    let (_tx, mut rx) = connect("not the password", Some(token));
    wait(&mut rx, |e| matches!(e, Ev::Online { .. }).then_some(()));
}

/// XEP-0077: a new account from the client (the test server allows it from loopback, no CAPTCHA),
/// then a login with it; a taken name is refused.
#[test]
#[ignore]
fn register_account() {
    let user = format!("reg{}", std::process::id());
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let register = |name: &str| {
        let (tx, mut rx) = crabber::register::start("localhost".into(), Some("127.0.0.1".into()), 5222, true);
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    match rx.next().await.expect("closed") {
                        crabber::register::Ev::Form(f) => {
                            assert!(f.ready(), "the test server asks for nothing but name and password: {f:?}");
                            tx.unbounded_send(crabber::register::Submit { username: name.into(), password: "pw".into(), values: Default::default() }).unwrap();
                        }
                        crabber::register::Ev::Done => return Ok(()),
                        crabber::register::Ev::Failed(e) => return Err(e),
                    }
                }
            })
            .await
            .expect("timed out")
        })
    };
    register(&user).unwrap();
    assert!(register(&user).is_err(), "the name is taken now");

    let (_tx, mut rx) = xmpp::connect(Account {
        jid: BareJid::from_str(&format!("{user}@localhost")).unwrap(),
        password: "pw".into(),
        host: Some("127.0.0.1".into()),
        port: 5222,
        insecure: true,
        dir: std::env::temp_dir().join(format!("re-test-{user}")),
        device_id: None,
        fast: None,
    });
    wait(&mut rx, |e| matches!(e, Ev::Online { .. }).then_some(()));
    let ctl = std::env::var("EJABBERDCTL").unwrap_or_else(|_| "ejabberdctl".into());
    let _ = std::process::Command::new(ctl).args(["unregister", &user, "localhost"]).status();
}

/// XEP-0313 in a room: bob joins after the fact and gets the history from the room's archive;
/// rejoining after the newest message he has brings only what he missed.
#[test]
#[ignore]
fn room_history_from_archive() {
    reset_accounts();
    let (a, mut a_rx) = login("alice");
    wait(&mut a_rx, |e| matches!(e, Ev::Online { .. }).then_some(()));
    let room = format!("h{}@conference.localhost", xmpp::new_id());
    a.unbounded_send(Cmd::Join { room: room.clone(), nick: "alice".into(), since: None, after: None, create_name: Some("Архив".into()) }).unwrap();
    wait(&mut a_rx, |e| matches!(e, Ev::RoomJoined { room: r, .. } if *r == room).then_some(()));
    std::thread::sleep(Duration::from_secs(1)); // the room config (MAM on) is set right after creation
    let say = |body: &str| {
        a.unbounded_send(Cmd::Send { to: room.clone(), id: xmpp::new_id(), body: body.into(), groupchat: true, enc: "plain".into(), reply: None }).unwrap();
    };
    say("первое");
    say("второе");
    std::thread::sleep(Duration::from_secs(1));

    let (b, mut b_rx) = login("bob");
    wait(&mut b_rx, |e| matches!(e, Ev::Online { .. }).then_some(()));
    b.unbounded_send(Cmd::Join { room: room.clone(), nick: "bob".into(), since: None, after: None, create_name: None }).unwrap();
    let mut got = vec![];
    while got.len() < 2 {
        got.push(wait(&mut b_rx, |e| match e {
            Ev::Message { msg, sid: Some(sid), live: false, .. } => Some((msg.body.clone(), sid.clone())),
            _ => None,
        }));
    }
    assert_eq!(got.iter().map(|g| g.0.as_str()).collect::<Vec<_>>(), ["первое", "второе"]);

    // bob drops out; alice writes; bob comes back after the newest message he has.
    b.unbounded_send(Cmd::Leave { room: room.clone() }).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    say("третье");
    std::thread::sleep(Duration::from_secs(1));
    b.unbounded_send(Cmd::Join { room: room.clone(), nick: "bob".into(), since: None, after: Some(got[1].1.clone()), create_name: None }).unwrap();
    let body = wait(&mut b_rx, |e| match e {
        Ev::Message { msg, .. } => Some(msg.body.clone()),
        _ => None,
    });
    assert_eq!(body, "третье", "only what bob missed");
}

/// XEP-0054: alice fills in her profile, then changes her avatar; bob still sees the profile.
#[test]
#[ignore]
fn vcard_profile_survives_avatar_change() {
    reset_accounts();
    let (a, mut a_rx) = login("alice");
    let (b, mut b_rx) = login("bob");
    wait(&mut a_rx, |e| matches!(e, Ev::Online { .. }).then_some(()));
    wait(&mut b_rx, |e| matches!(e, Ev::Online { .. }).then_some(()));

    let mut profile = xmpp::Profile::default();
    profile[0] = "Алиса".into();
    profile[3] = "alice@example.com".into();
    profile[8] = "строка 1\nстрока 2".into();
    a.unbounded_send(Cmd::SetProfile(profile.clone())).unwrap();
    wait(&mut a_rx, |e| match e {
        Ev::Profile { jid, profile: Some(_) } if jid == "alice@localhost" => Some(()),
        Ev::Error(e) => panic!("{e}"),
        _ => None,
    });

    // 1×1 PNG.
    let png = std::env::temp_dir().join("re-avatar.png");
    std::fs::write(&png, b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0\x1f\x15\xc4\x89\0\0\0\nIDATx\x9cc\0\x01\0\0\x05\0\x01\r\n-\xb4\0\0\0\0IEND\xaeB`\x82").unwrap();
    a.unbounded_send(Cmd::SetAvatar(png)).unwrap();
    wait(&mut a_rx, |e| match e {
        Ev::Avatar { jid } if jid == "alice@localhost" => Some(()),
        Ev::Error(e) => panic!("{e}"),
        _ => None,
    });

    b.unbounded_send(Cmd::GetProfile { jid: "alice@localhost".into() }).unwrap();
    let seen = wait(&mut b_rx, |e| match e {
        Ev::Profile { jid, profile } if jid == "alice@localhost" => Some(profile.clone()),
        _ => None,
    });
    assert_eq!(seen, Some(profile));
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

    a.unbounded_send(Cmd::Call(CallCmd::Start { to: "bob@localhost".into(), video: false })).unwrap();
    let call_state = |e: &Ev| match e {
        Ev::Call(CallEv::State { state, .. }) => Some(state.clone()),
        _ => None,
    };
    assert_eq!(wait(&mut b_rx, call_state), CallState::Incoming);
    b.unbounded_send(Cmd::Call(CallCmd::Accept { video: false })).unwrap();
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

/// XEP-0272 mesh in a room: alice starts the call, bob and carol join at the same moment (their
/// preparation and crossing sessions must sort themselves out). Every pair gets a session that
/// connects and carries audio; when bob leaves, the others drop him.
#[test]
#[ignore]
fn group_call_mesh() {
    use crabber::groupcall::{GroupCmd, GroupEv};
    reset_accounts();
    // Nothing is heard from the speakers while three clients play each other.
    crabber::audio::set_volumes(1.0, 0.0);
    let users = ["alice", "bob", "carol"];
    let mut clients: Vec<_> = users.iter().map(|u| login(u)).collect();
    for (_, rx) in clients.iter_mut() {
        wait(rx, |e| matches!(e, Ev::Online { .. }).then_some(()));
    }
    let room = format!("g{}@conference.localhost", xmpp::new_id());
    clients[0].0.unbounded_send(Cmd::Join { room: room.clone(), nick: "alice".into(), since: None, after: None, create_name: Some("Звонок".into()) }).unwrap();
    wait(&mut clients[0].1, |e| matches!(e, Ev::RoomJoined { room: r, .. } if *r == room).then_some(()));
    std::thread::sleep(Duration::from_millis(500)); // room configuration (non-anonymous)
    for (i, (tx, rx)) in clients.iter_mut().enumerate().skip(1) {
        tx.unbounded_send(Cmd::Join { room: room.clone(), nick: users[i].into(), since: None, after: None, create_name: None }).unwrap();
        wait(rx, |e| matches!(e, Ev::RoomJoined { room: r, .. } if *r == room).then_some(()));
    }
    std::thread::sleep(Duration::from_millis(500));

    let join = |tx: &UnboundedSender<Cmd>| tx.unbounded_send(Cmd::Group(GroupCmd::Join { room: room.clone() })).unwrap();
    join(&clients[0].0);
    wait(&mut clients[0].1, |e| matches!(e, Ev::Group(GroupEv::Joined { .. })).then_some(()));
    std::thread::sleep(Duration::from_secs(1));
    join(&clients[1].0);
    join(&clients[2].0);

    let before = crabber::call::RTP_RECEIVED.load(std::sync::atomic::Ordering::Relaxed);
    for (i, (_, rx)) in clients.iter_mut().enumerate() {
        let mut connected = std::collections::BTreeSet::new();
        wait(rx, |e| {
            match e {
                Ev::Group(GroupEv::Peer { nick, connected: true, .. }) => {
                    connected.insert(nick.clone());
                }
                Ev::Group(GroupEv::Ended { reason, .. }) => panic!("{}: call ended: {reason}", users[i]),
                _ => {}
            }
            (connected.len() == 2).then_some(())
        });
        let others: std::collections::BTreeSet<String> = users.iter().filter(|u| **u != users[i]).map(|u| u.to_string()).collect();
        assert_eq!(connected, others, "{} reaches everyone", users[i]);
    }
    std::thread::sleep(Duration::from_secs(2));
    let got = crabber::call::RTP_RECEIVED.load(std::sync::atomic::Ordering::Relaxed) - before;
    // Six directions at 50 packets/s.
    assert!(got > 300, "only {got} RTP packets received");

    clients[1].0.unbounded_send(Cmd::Group(GroupCmd::Leave)).unwrap();
    wait(&mut clients[1].1, |e| matches!(e, Ev::Group(GroupEv::Ended { .. })).then_some(()));
    for i in [0, 2] {
        wait(&mut clients[i].1, |e| matches!(e, Ev::Group(GroupEv::PeerGone { nick }) if nick == "bob").then_some(()));
    }
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
    // With Bind 2 (XEP-0386) the server drops offline messages: the archive has them, as the app syncs.
    wait(&mut b_rx, |e| matches!(e, Ev::Online { .. }).then_some(()));
    b.unbounded_send(Cmd::Sync { mam_after: None, rooms: vec![] }).unwrap();
    assert_eq!(recv(&mut b_rx, &id), ("шесть".into(), 1));
    let id = send(&b, &mut b_rx, "alice@localhost", "семь");
    assert_eq!(recv(&mut a_rx, &id), ("семь".into(), 1));
    assert_eq!(recv(&mut a2_rx, &id), ("семь".into(), 1));
}
