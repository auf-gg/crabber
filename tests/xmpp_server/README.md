# Test stand

ejabberd 26.09 in Docker for the XMPP domain `localhost`, filled with accounts, chats, rooms and
files. Everything the server keeps is under `data/`: the SQLite database
(`data/database/ejabberd.db`) and the uploaded files (`data/upload/`). Copy the directory and the
stand comes along, chats and files included.

```sh
docker compose up -d      # start
docker compose down       # stop
./seed.sh                 # wipe data/ and fill it again (about a minute)
```

Log in as **alice@localhost**, password `alice`, server `127.0.0.1`, with "trust self-signed
certificate" checked. Every account's password is its name. The web admin is at
http://localhost:5280/admin (`admin@localhost` / `admin`).

The container runs as uid:gid 1000:1000, so `data/` stays owned by you. For another uid, put
`UID=…` and `GID=…` in `.env` or the environment. Create `data/` with `./seed.sh`, not with
`docker compose up`: Docker would create the missing mount points as root.

## Ports (127.0.0.1 only)

| Port | |
| --- | --- |
| 5222 | client connections, STARTTLS |
| 5223 | client connections, direct TLS (XEP-0368) |
| 5443 | HTTP File Upload, `https://localhost:5443/upload/...` |
| 5280 | web admin |
| 5478/udp | STUN |

Registration is open (no CAPTCHA), so signing up can be tried from the login screen. There is no
traffic shaper: Jingle file transfer runs at full speed, not ejabberd's stock 3 KB/s.

## What's inside

Seen from Alice's account, spread over the last week. The dates are fixed when the stand is
seeded.

| Chat | What it covers |
| --- | --- |
| Bob (today) | photo, MP4 video, voice message, a PDF whose name needs percent-encoding, replies, reactions, an edit, a deletion, `/me`, `*bold* _italic_ ~strike~ \`code\``, quotes, a Python code block, a long message, read ticks, 2 unread |
| Carol (yesterday) | gallery: JPEG landscape/portrait, a phone photo stored sideways with EXIF orientation 6, PNG screenshot, transparent PNG, animated GIF, WebP sticker, 4000×700 panorama, a 24×24 icon; zip, CSV and Markdown files with a text preview; 2 unread |
| Dave (3 days ago) | voice messages (.opus, .ogg), MP3, vertical WebM, `.patch`, `.rs` and `.log` files, a Rust code block; a long roster name and no avatar |
| Eve (6 days ago) | Japanese, Arabic (RTL), Hebrew, Hindi, Thai, Korean, Georgian, Cyrillic; ZWJ emoji, flags; combining marks; a long word, a long URL and a Cyrillic one; read marker |
| Notes (Alice to herself) | a list, a link, a voice note |
| Frank | not a contact: a pending subscription request with a nick (XEP-0172), a message, an `.m4a` voice note |
| Mallory | spam; blocked, with a spam report |
| Room "Karelia trip" | non-anonymous, with an avatar and a subject; photo, video, voice, CSV; replies, a reply to a reply, reactions from several people, a mention, `/me`, an edit, a self-deletion, a moderated message, 3 unread |
| Room "Crabber dev" | semi-anonymous, members only, with an avatar; screenshot, diff, code file, reactions |
| Room "Secret project" | members only; Eve invites Alice (XEP-0249), the invite waits in offline storage for her first login |

Alice's bookmarks of the first two rooms (XEP-0402) and her read position in Bob's and Carol's
chats and in the trip room (XEP-0490) are on the server too. Everyone has a vCard and a PEP
nickname; Alice, Bob, Carol, Eve and Frank also an avatar (vCard and PEP), Dave and Mallory none.

Things to know:

- At first login Crabber reads only the newest 100 messages of an account's archive, plus the
  newest 100 of each room. Alice's chats come to 85 messages, so they fit; seeding more would
  push the oldest ones out.
- ejabberd deletes retracted and moderated messages from the archive. History keeps only the
  retraction itself, which points at nothing.
- Logging in changes the stand: read markers, offline messages, new history. Run `./seed.sh`
  again, or restore `data/` from git, to get it back.

## Files

- `conf/ejabberd.yml`: server configuration; `conf/server.pem`: self-signed certificate for
  `localhost` and its subdomains, valid until 2126.
- `seed.sh`: accounts, contacts and rooms through `ejabberdctl`, then `seed/seed.py`.
- `seed/seed.py`: tells the story through real XMPP connections (slixmpp, in the `seed` container),
  then moves each archived message to its made-up time.
- `seed/media/`, `seed/avatars/`: the files sent; `seed/make_media.sh` regenerates them
  (ffmpeg, ImageMagick).
