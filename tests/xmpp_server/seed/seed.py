"""Fills the Crabber test stand with chats. Run by seed.sh against a fresh server, right after it
made the accounts, their contacts and the rooms.

Every account is online while the story is told, so each message is delivered live and lands in
the archives of both sides (or the room's). ejabberd stamps a message with its arrival time and
uses that time as its archive id; the story gives each message a made-up time instead, and at the
end the archive is rewritten with those times, the ids that point at messages included. The chats
then spread over the last week rather than the minute the seeding took.
"""
import asyncio
import base64
import hashlib
import mimetypes
import os
import random
import re
import sqlite3
import ssl
import urllib.parse
import urllib.request
import uuid
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from pathlib import Path
from xml.etree import ElementTree as ET
from xml.sax.saxutils import escape, quoteattr

from slixmpp import ClientXMPP
from slixmpp.exceptions import IqError
from slixmpp.xmlstream.handler import Callback
from slixmpp.xmlstream.matcher import StanzaPath

DOMAIN = "localhost"
MUC = f"conference.{DOMAIN}"
GENERAL, DEV, SECRET = f"general@{MUC}", f"dev@{MUC}", f"secret@{MUC}"
HERE = Path(__file__).parent
MEDIA, AVATARS = HERE / "media", HERE / "avatars"
DB = Path("/db/ejabberd.db")

NOW = datetime.now(timezone.utc)
_tz = os.environ.get("SEED_TZ", "+0000")  # `date +%z` of the machine that seeds
TZ = timezone(timedelta(hours=int(_tz[:3]), minutes=int(_tz[0] + _tz[3:])))

NS_SID = "urn:xmpp:sid:0"
NS_REPLY = "urn:xmpp:reply:0"
NS_FALLBACK = "urn:xmpp:fallback:0"
NS_REACTIONS = "urn:xmpp:reactions:0"
NS_RETRACT = "urn:xmpp:message-retract:1"
NS_CORRECT = "urn:xmpp:message-correct:0"
NS_MARKERS = "urn:xmpp:chat-markers:0"
NS_RECEIPTS = "urn:xmpp:receipts"
NS_UPLOAD = "urn:xmpp:http:upload:0"
NS_MUC = "http://jabber.org/protocol/muc"
NS_MUC_USER = NS_MUC + "#user"
NS_PUBSUB = "http://jabber.org/protocol/pubsub"
NS_NICK = "http://jabber.org/protocol/nick"
NS_MDS = "urn:xmpp:mds:displayed:0"
NS_BOOKMARKS = "urn:xmpp:bookmarks:1"
STORE = "<store xmlns='urn:xmpp:hints'/>"
# Publish options of private PEP nodes (bookmarks, read markers).
PRIVATE = {"persist_items": "true", "max_items": "max", "send_last_published_item": "never", "access_model": "whitelist"}

INSECURE = ssl.create_default_context()
INSECURE.check_hostname = False
INSECURE.verify_mode = ssl.CERT_NONE


def tag(name, attrs=None, inner=""):
    """An element as text; `inner` is XML already."""
    a = "".join(f" {k}={quoteattr(str(v))}" for k, v in (attrs or {}).items())
    return f"<{name}{a}>{inner}</{name}>" if inner else f"<{name}{a}/>"


def text(name, value, attrs=None):
    return tag(name, attrs, escape(value))


class Clock:
    """Made-up send times: each message a little after the one before."""

    def __init__(self):
        self.t = NOW
        self.rng = random.Random(42)
        self.used = set()

    def day(self, days_ago, hour, minute=0):
        """Wall-clock time `days_ago` days back, in the seeding machine's zone."""
        self.t = NOW.astimezone(TZ).replace(hour=hour, minute=minute, second=0, microsecond=0) - timedelta(days=days_ago)

    def ago(self, minutes):
        self.t = NOW - timedelta(minutes=minutes)

    def next(self, gap=None):
        """Advances by `gap` seconds (a random pause by default); the new time in microseconds."""
        self.t += timedelta(seconds=self.rng.randint(20, 150) if gap is None else gap)
        assert self.t < NOW - timedelta(minutes=1), f"the story runs into the future: {self.t}"
        us = int(self.t.timestamp() * 1_000_000)
        while us in self.used:
            us += 1000
        self.used.add(us)
        return us


@dataclass
class Msg:
    id: str
    sender: "Bot"
    to: str  # bare JID of the recipient or the room
    room: bool
    text: str  # what a reply quotes: the body without its own quote, or the file's URL
    sids: dict = field(default_factory=dict)  # stanza-ids the receiving side got, by archive


class Bot(ClientXMPP):
    def __init__(self, user, nick):
        super().__init__(f"{user}@{DOMAIN}/seed", user)
        self.user, self.bare, self.nick = user, f"{user}@{DOMAIN}", nick
        self.nicks = {}  # room -> our nick there
        self.waiting = {}  # message id / ("join", room) / ("room", room) -> future
        self.started = asyncio.get_running_loop().create_future()
        # seed.sh made the contacts: answer no subscription request (frank's must stay pending).
        self.auto_authorize = None
        self.auto_subscribe = False
        self.ssl_context.check_hostname = False
        self.ssl_context.verify_mode = ssl.CERT_NONE
        self.add_event_handler("session_start", self._started)
        # slixmpp's own "message" event skips messages without a body: markers, reactions.
        self.register_handler(Callback("seed message", StanzaPath("message"), self._message))
        self.register_handler(Callback("seed presence", StanzaPath("presence"), self._presence))

    def _started(self, _):
        self.send_presence()
        self.started.set_result(None)

    def expect(self, key):
        fut = asyncio.get_running_loop().create_future()
        self.waiting[key] = fut
        return fut

    def _resolve(self, key, value=None, error=None):
        fut = self.waiting.pop(key, None)
        if fut is None or fut.done():
            return
        if error:
            fut.set_exception(RuntimeError(error))
        else:
            fut.set_result(value)

    def _message(self, msg):
        x = msg.xml
        if x.get("type") == "error":
            self._resolve(x.get("id"), error=ET.tostring(x, encoding="unicode"))
            return
        sids = {s.get("by"): s.get("id") for s in x.findall(f"{{{NS_SID}}}stanza-id")}
        origin = x.find(f"{{{NS_SID}}}origin-id")
        for key in {x.get("id"), origin.get("id") if origin is not None else None} - {None}:
            self._resolve(key, sids)
        frm = x.get("from", "")
        if "/" not in frm:  # from a room itself: a moderator's deletion
            self._resolve(("room", frm), x.get("id"))

    def _presence(self, pres):
        x = pres.xml
        room = x.get("from", "").split("/")[0]
        if x.get("type") == "error":
            self._resolve(("join", room), error=ET.tostring(x, encoding="unicode"))
        elif x.find(f"{{{NS_MUC_USER}}}x/{{{NS_MUC_USER}}}status[@code='110']") is not None:
            self._resolve(("join", room))

    async def iq(self, payload, to=None, kind="set"):
        iq = self.Iq()
        iq["id"], iq["type"] = self.new_id(), kind
        if to:
            iq["to"] = to
        iq.xml.append(ET.fromstring(payload))
        try:
            return (await iq.send(timeout=15)).xml
        except IqError as e:  # the error echoes the payload, avatars and all: keep it short
            raise RuntimeError(f"{self.user}: {payload[:150]}… → {e.iq['error']['condition']} {e.iq['error']['text']}") from None

    async def join(self, room, nick):
        self.nicks[room] = nick
        joined = self.expect(("join", room))
        self.send_raw(tag("presence", {"to": f"{room}/{nick}"}, tag("x", {"xmlns": NS_MUC}, tag("history", {"maxstanzas": 0}))))
        await asyncio.wait_for(joined, 15)

    async def publish(self, node, item_id, payload, options=None):
        opts = ""
        if options:
            fields = tag("field", {"var": "FORM_TYPE", "type": "hidden"}, text("value", NS_PUBSUB + "#publish-options"))
            fields += "".join(tag("field", {"var": f"pubsub#{k}"}, text("value", v)) for k, v in options.items())
            opts = tag("publish-options", None, tag("x", {"xmlns": "jabber:x:data", "type": "submit"}, fields))
        await self.iq(tag("pubsub", {"xmlns": NS_PUBSUB}, tag("publish", {"node": node}, tag("item", {"id": item_id}, payload)) + opts))

    async def profile(self, card, avatar=None):
        """vCard (what Crabber's contact card shows), PEP nick and, with `avatar`, the avatar both ways."""
        sub = {"EMAIL": "USERID", "TEL": "NUMBER", "ORG": "ORGNAME"}
        fields = "".join(tag(k, None, text(sub[k], v)) if k in sub else text(k, v) for k, v in card.items())
        if avatar:
            data = (AVATARS / avatar).read_bytes()
            mime = mimetypes.guess_type(avatar)[0]
            b64 = base64.b64encode(data).decode()
            fields += tag("PHOTO", None, text("TYPE", mime) + text("BINVAL", b64))
        await self.iq(tag("vCard", {"xmlns": "vcard-temp"}, fields))
        await self.publish(NS_NICK, "current", text("nick", card["NICKNAME"], {"xmlns": NS_NICK}))
        if avatar:
            sha = hashlib.sha1(data).hexdigest()
            await self.publish("urn:xmpp:avatar:data", sha, text("data", b64, {"xmlns": "urn:xmpp:avatar:data"}))
            info = tag("info", {"id": sha, "bytes": len(data), "type": mime})
            await self.publish("urn:xmpp:avatar:metadata", sha, tag("metadata", {"xmlns": "urn:xmpp:avatar:metadata"}, info))

    async def upload(self, name):
        """XEP-0363: a slot from the server, an HTTP PUT; the download URL."""
        data = (MEDIA / name).read_bytes()
        mime = mimetypes.guess_type(name)[0] or "application/octet-stream"
        request = tag("request", {"xmlns": NS_UPLOAD, "filename": name, "size": len(data), "content-type": mime})
        slot = await self.iq(request, to=f"upload.{DOMAIN}", kind="get")
        put, get = slot.find(f".//{{{NS_UPLOAD}}}put"), slot.find(f".//{{{NS_UPLOAD}}}get")
        headers = {h.get("name"): h.text for h in put.findall(f"{{{NS_UPLOAD}}}header")}

        # ejabberd leaves spaces and non-ASCII in the slot URLs as they are in the file name.
        def escape_url(url):
            return urllib.parse.quote(url, safe=":/?#[]@!$&'()*+,;=%")

        def send():
            req = urllib.request.Request(escape_url(put.get("url")), data=data, method="PUT", headers={"Content-Type": mime, **headers})
            urllib.request.urlopen(req, context=INSECURE).close()

        await asyncio.to_thread(send)
        return escape_url(get.get("url"))


class Stand:
    """Sends the story's messages, the way Crabber builds them, and notes their made-up times."""

    def __init__(self, bots):
        self.bots = bots
        self.clock = Clock()
        self.times = {}  # message id -> made-up time, us

    async def send(self, frm, to, inner, gap=None):
        """A message with `inner` payloads; waits until it is delivered (to a room: reflected)."""
        room = to.endswith("@" + MUC)
        mid = str(uuid.uuid4())
        self.times[mid] = self.clock.next(gap)
        receiver = frm if room else self.bots[to.split("@")[0]]
        delivered = receiver.expect(mid)
        attrs = {"to": to, "type": "groupchat" if room else "chat", "id": mid}
        frm.send_raw(tag("message", attrs, inner + tag("origin-id", {"xmlns": NS_SID, "id": mid})))
        return mid, await asyncio.wait_for(delivered, 15)

    @staticmethod
    def peer(frm, msg):
        """Where something about `msg` goes when `frm` sends it."""
        return msg.to if msg.room or frm is msg.sender else msg.sender.bare

    @staticmethod
    def ref(msg):
        """How other messages point at `msg`: the room's stanza-id in a room, our id in a chat."""
        return msg.sids[msg.to] if msg.room else msg.id

    async def say(self, frm, to, body, reply=None, gap=None, extra=""):
        room = to.endswith("@" + MUC)
        quoted = body
        refs = ""
        if reply:
            name = reply.sender.nicks[to] if room else reply.sender.nick
            lines = "\n".join(reply.text.splitlines()[:3])[:200]
            quote = f"> {name}: " + lines.replace("\n", "\n> ") + "\n"
            body = quote + body
            ref_to = f"{to}/{reply.sender.nicks[to]}" if room else reply.sender.bare
            refs = tag("reply", {"xmlns": NS_REPLY, "to": ref_to, "id": self.ref(reply)})
            refs += tag("fallback", {"xmlns": NS_FALLBACK, "for": NS_REPLY}, tag("body", {"start": 0, "end": len(quote)}))
        inner = text("body", body) + refs + extra + "<active xmlns='http://jabber.org/protocol/chatstates'/>"
        if not room:
            inner += f"<request xmlns='{NS_RECEIPTS}'/><markable xmlns='{NS_MARKERS}'/>"
        mid, sids = await self.send(frm, to, inner, gap)
        return Msg(mid, frm, to, room, quoted, sids)

    async def subject(self, frm, room, subject):
        await self.send(frm, room, text("subject", subject), gap=0)

    async def file(self, frm, to, name, gap=None):
        url = await frm.upload(name)
        return await self.say(frm, to, url, gap=gap, extra=tag("x", {"xmlns": "jabber:x:oob"}, text("url", url)))

    async def react(self, frm, msg, *emojis, gap=None):
        reactions = "".join(text("reaction", e) for e in emojis)
        await self.send(frm, self.peer(frm, msg), tag("reactions", {"xmlns": NS_REACTIONS, "id": self.ref(msg)}, reactions) + STORE, gap or 20)

    async def correct(self, msg, body, gap=None):
        inner = text("body", body) + tag("replace", {"xmlns": NS_CORRECT, "id": msg.id})
        await self.send(msg.sender, msg.to, inner, gap or 25)
        msg.text = body

    async def retract(self, msg, gap=None):
        inner = text("body", "/me retracted a previous message, but it's unsupported by your client.")
        inner += tag("retract", {"xmlns": NS_RETRACT, "id": self.ref(msg)}) + tag("fallback", {"xmlns": NS_FALLBACK, "for": NS_RETRACT}) + STORE
        await self.send(msg.sender, msg.to, inner, gap or 30)

    async def moderate(self, moderator, msg, reason):
        """XEP-0425; waits for the room's broadcast of the deletion (which the archive does not keep)."""
        broadcast = moderator.expect(("room", msg.to))
        retract = tag("retract", {"xmlns": NS_RETRACT}) + text("reason", reason)
        try:
            await moderator.iq(tag("moderate", {"xmlns": "urn:xmpp:message-moderate:1", "id": self.ref(msg)}, retract), to=msg.to)
        except Exception:  # older servers: the fastening-based :0
            v0 = tag("moderate", {"xmlns": "urn:xmpp:message-moderate:0"}, "<retract xmlns='urn:xmpp:message-retract:0'/>" + text("reason", reason))
            await moderator.iq(tag("apply-to", {"xmlns": "urn:xmpp:fasten:0", "id": self.ref(msg)}, v0), to=msg.to)
        await asyncio.wait_for(broadcast, 15)

    async def marker(self, frm, msg, kind="displayed", gap=None):
        """Read (or delivery) marker for `msg`, stored so the archive brings the ticks back."""
        ns = NS_MARKERS if kind == "displayed" else NS_RECEIPTS
        await self.send(frm, msg.sender.bare, tag(kind, {"xmlns": ns, "id": msg.id}) + STORE, gap or 10)


PEOPLE = {
    "alice": ("Alice", "alice.png", {
        "FN": "Alice Korolev", "NICKNAME": "Alice", "BDAY": "1994-03-12", "EMAIL": "alice@example.com",
        "TEL": "+7 900 123-45-67", "URL": "https://alice.example.com", "ORG": "Crabber", "TITLE": "Developer",
        "DESC": "Writing an XMPP client in Rust 🦀\nLoves hiking and Karelia."}),
    "bob": ("Bob", "bob.jpg", {
        "FN": "Bob Miller", "NICKNAME": "Bob", "BDAY": "1990-11-20", "EMAIL": "bob@example.com",
        "TEL": "+1 555 0100", "ORG": "Northwind Travel", "TITLE": "Travel planner", "DESC": "Organizing the Karelia trip 🏕️"}),
    "carol": ("Carol", "carol.png", {
        "FN": "Carol Lee", "NICKNAME": "Carol", "EMAIL": "carol@example.com", "ORG": "Crabber", "TITLE": "Designer",
        "URL": "https://carol.example.com/portfolio", "DESC": "Takes photos of everything 📸"}),
    "dave": ("Dave", None, {
        "FN": "Dmitry Alexandrovich Volkov-Zadunaisky", "NICKNAME": "Dave", "BDAY": "1988-07-01",
        "EMAIL": "dmitry.volkov-zadunaisky@very-long-company-name.example.com", "ORG": "Crabber",
        "TITLE": "Lead backend engineer, distributed systems team",
        "DESC": "No avatar: the fallback gets tested.\nA long name, title and email: wrapping gets tested.\n\nA third line after a blank one."}),
    "eve": ("Eve 🌸", "eve.jpg", {
        "FN": "Eve Tanaka (田中 イヴ)", "NICKNAME": "Eve 🌸", "URL": "https://eve.example.jp", "DESC": "東京からこんにちは 🗼"}),
    "frank": ("Frank", "frank.png", {
        "FN": "Frank Weber", "NICKNAME": "Frank", "ORG": "XMPP Meetup Berlin", "EMAIL": "frank@example.de"}),
    "mallory": ("🎁 Prize Center", None, {"FN": "Mallory", "NICKNAME": "🎁 Prize Center"}),
}


async def story(s, b):
    A, B, C, D, E, F, M = (b[u] for u in ["alice", "bob", "carol", "dave", "eve", "frank", "mallory"])
    a, bob, carol, dave, eve = A.bare, B.bare, C.bare, D.bare, E.bare
    clock = s.clock

    for user, (_, avatar, card) in PEOPLE.items():
        await b[user].profile(card, avatar)
    for room, members in {
        GENERAL: [(B, "Bob"), (A, "Alice"), (C, "Carol"), (D, "Dave"), (E, "Eve")],
        DEV: [(A, "alice"), (B, "bob"), (D, "dave")],
        SECRET: [(E, "Eve"), (C, "Carol")],
    }.items():
        for bot, nick in members:
            await bot.join(room, nick)
    for owner, room, avatar in [(B, GENERAL, "room-general.png"), (A, DEV, "room-dev.png")]:
        photo = text("TYPE", "image/png") + text("BINVAL", base64.b64encode((AVATARS / avatar).read_bytes()).decode())
        await owner.iq(tag("vCard", {"xmlns": "vcard-temp"}, tag("PHOTO", None, photo)), to=room)

    # --- Eve, six days ago: scripts, emoji, wrapping ---
    clock.day(6, 19, 5)
    await s.say(E, a, "こんにちは、アリス！元気ですか？ 🌸")
    await s.say(A, eve, "Hi Eve! All good here. How's Tokyo?")
    await s.say(E, a, "Rainy ☔ 雨です。Let's see how your client draws other scripts:")
    await s.say(E, a, "العربية تُكتب من اليمين إلى اليسار — and a mixed line: RTL in the middle, digits 123 and 456.\n"
                      "עברית · Ελληνικά · हिन्दी · ไทย · 한국어 · ქართული · Русский · Українська")
    await s.say(E, a, "ZWJ emoji: 👩‍💻 👨‍👩‍👧‍👦 🏳️‍🌈 🧑🏽‍🚀 ❤️‍🔥\nFlags: 🇷🇺 🇯🇵 🇧🇷 🇺🇦\nNew ones: 🫠 🫶 🪿 🫎")
    await s.say(E, a, "Wrapping: " + "Averylongwordwithoutasinglespace" * 5 + ", a long link "
                      "https://example.com/" + "very/long/path/" * 10 + "?query=" + "x" * 50
                      + " and one in Cyrillic: https://ru.wikipedia.org/wiki/Рускеала")
    await s.say(E, a, "Z̤͔ͧ̑̓ä͖̭̈̇lͮ̒ͫǧ̗͚̚o̙̔ͮ̇͐̇ — combining marks, s̶t̶r̶i̶k̶e̶ with U+0336")
    last = await s.say(A, eve, "😂")
    await s.marker(E, last)

    # --- Secret room, four days ago; Alice gets invited at the very end ---
    clock.day(4, 21, 0)
    await s.say(E, SECRET, "Shh… we're planning a birthday surprise for Bob 🎂")
    await s.say(C, SECRET, "I'm in! How about a crab-shaped cake 🦀")
    await s.file(E, SECRET, "logo.png")
    await s.say(E, SECRET, "Something like this")
    await s.say(C, SECRET, "Perfect 😂 Shall we invite Alice?")

    # --- Dave, three days ago: voice, code, text files ---
    clock.day(3, 11, 20)
    await s.say(D, a, "Hi Alice! Recorded a voice note about the release, have a listen")
    await s.file(D, a, "voice-dave.opus")
    await s.file(A, dave, "voice-alice.ogg", gap=90)
    await s.say(D, a, "OK, then here's the patch:")
    patch = await s.file(D, a, "fix-mam-paging.patch")
    await s.say(A, dave, "I'll look after lunch. This is how I'd do the backoff:", reply=patch, gap=300)
    await s.file(A, dave, "reconnect.rs")
    await s.say(D, a, "```rust\nlet delay = backoff.next();\ntokio::time::sleep(delay).await;\n```\nAnd reset it after a successful login?")
    await s.say(D, a, "And here's the crash log from the call:")
    await s.file(D, a, "crash.log")
    thanks = await s.say(A, dave, "Thanks, I'll dig into it 🙏")
    await s.react(D, thanks, "👍")
    clock.day(3, 16, 45)
    await s.say(D, a, "And some music for the intro, what do you think? 🎵")
    await s.file(D, a, "melody.mp3")
    await s.file(D, a, "vertical.webm")
    await s.say(A, dave, "Love the music 🔥")

    # --- Dev room, two days ago (semi-anonymous, members only) ---
    clock.day(2, 14, 0)
    await s.subject(A, DEV, "Crabber 0.9.2 — release on Friday")
    shot_text = await s.say(D, DEV, "Built a nightly, a test fails:")
    shot = await s.file(D, DEV, "screenshot.png", gap=5)
    await s.say(A, DEV, "That's the upload on the test server, I'll fix it", reply=shot)
    await s.say(D, DEV, "```diff\n- let url = slot.put;\n+ let url = slot.put.trim_end_matches('/');\n```")
    await s.say(B, DEV, "Checked on Windows — all good ✅")
    await s.say(A, DEV, "A draft of the reconnect backoff:")
    draft = await s.file(A, DEV, "reconnect.rs", gap=10)
    await s.react(D, draft, "👀")
    await s.react(B, draft, "👀", "🚀")
    await s.say(B, DEV, "`cargo clippy` complains about `needless_return` in xmpp.rs")
    await s.say(A, DEV, "/me is fixing clippy")
    await s.say(D, DEV, "By the way, about the archive: https://xmpp.org/extensions/xep-0313.html")
    await s.react(A, shot_text, "✅")

    # --- Notes to self, two days ago ---
    clock.day(2, 9, 0)
    await s.say(A, a, "Shopping list:\n- bread\n- cheese\n- batteries for the flashlight")
    await s.say(A, a, "Cabin Wi-Fi password: `karelia2026`")
    await s.say(A, a, "Read up on MDS: https://xmpp.org/extensions/xep-0490.html")

    # --- Carol, yesterday: a gallery and files ---
    clock.day(1, 18, 40)
    await s.say(C, a, "Here are the photos from the office party 📸")
    team = await s.file(C, a, "team.jpg", gap=10)
    for name in ["portrait.jpg", "phone-photo.jpg", "screenshot.png", "spin.gif", "sticker.webp", "logo.png", "panorama.jpg", "tiny.png"]:
        await s.file(C, a, name, gap=8)
    await s.say(A, carol, "Wow, that's a lot! Is the phone photo sideways? 🙃", gap=400)
    await s.say(C, a, "That's us on the office roof", reply=team)
    await s.say(C, a, "And all of them in one archive:")
    await s.file(C, a, "photos.zip")
    await s.file(C, a, "expenses.csv")
    seen = await s.say(A, carol, "Thanks! I'll check the expenses tomorrow")
    await s.marker(C, seen, "received", gap=2)
    await s.marker(C, seen, gap=5)
    carol_read = await s.say(C, a, "Sure 👌")
    clock.ago(200)
    await s.say(C, a, "Please take a look before lunch 🙏")
    await s.file(C, a, "TODO.md")

    # --- General room: yesterday and today ---
    clock.day(1, 10, 0)
    await s.subject(B, GENERAL, "Karelia trip 🏕️ Nov 15–17. We meet on Friday at 21:00 at Ladozhsky station")
    await s.say(B, GENERAL, "Hi all! I made a chat for the trip")
    await s.say(C, GENERAL, "Yay! 🎉")
    await s.say(D, GENERAL, "Hi everyone")
    await s.say(E, GENERAL, "Hi! Count me in, I'll be the one with the camera 😅")
    pano = await s.file(B, GENERAL, "panorama.jpg")
    await s.say(B, GENERAL, "Lake Ladoga last summer", gap=10)
    for bot, emoji in [(C, "❤️"), (D, "🔥"), (E, "😍"), (A, "❤️")]:
        await s.react(bot, pano, emoji, gap=15)
    where = await s.say(C, GENERAL, "Where was this taken?", reply=pano)
    await s.say(B, GENERAL, "Lahdenpohja, at sunset", reply=where)
    await s.file(D, GENERAL, "voice-room.oga")
    await s.say(E, GENERAL, "/me waves from Tokyo 👋")
    ask = await s.say(C, GENERAL, "Alice: are you definitely coming?")
    await s.say(A, GENERAL, "Yes! Already bought the tickets 🎫", reply=ask)
    pot = await s.say(D, GENERAL, "Who's bringing the pot?")
    await s.correct(pot, "Who's bringing the pot and the stove?")
    oops = await s.say(E, GENERAL, "Oops, wrong chat")
    await s.retract(oops)
    leak = await s.say(E, GENERAL, "my email password: hunter2")
    await s.moderate(B, leak, "A password in the group chat")
    await s.say(B, GENERAL, "The plan:\n1. *Friday* — the train\n2. *Saturday* — Ruskeala\n3. *Sunday* — Valaam\n\n_No tents needed_, we're staying in a cabin.")
    await s.file(C, GENERAL, "vertical.webm")
    clock.ago(130)
    await s.say(D, GENERAL, "It's 9,000 ₽ each, the expenses are here:")
    await s.file(D, GENERAL, "expenses.csv", gap=10)
    read_here = await s.say(A, GENERAL, "Sent 💸")
    ready = await s.say(B, GENERAL, "Great, everyone's in!")
    await s.react(C, ready, "👍")
    await s.say(E, GENERAL, "I'll send new photos tonight")
    await s.say(D, GENERAL, "OK")

    # --- Bob, today: one of everything ---
    clock.ago(110)
    await s.say(B, a, "Hi! So, are we going to Karelia? 🏞️")
    await s.say(A, bob, "Hi 👋 Of course! Already packing")
    view = await s.file(B, a, "karelia.jpg")
    await s.say(B, a, "The view from the cabin I booked", gap=10)
    await s.say(A, bob, "Wow, gorgeous! Is that right from the window?", reply=view)
    await s.say(B, a, "Yep. And a video from the waterfall:")
    video = await s.file(B, a, "waterfall.mp4", gap=10)
    await s.react(A, video, "❤️", "🔥")
    await s.file(B, a, "voice-bob.oga")
    await s.say(A, bob, "Listened to the voice note, got it")
    styled = await s.say(B, a, "Let's test the styling: *bold*, _italic_, ~strikethrough~ and `monospace`")
    await s.say(A, bob, "> Let's test the styling\nWorks! Quotes too")
    await s.say(B, a, "And a code block?\n```python\ndef hello(name: str) -> str:\n    return f\"Hello, {name}!\"\n```")
    await s.say(B, a, "Got the route from here: https://en.wikipedia.org/wiki/Ruskeala")
    route = await s.file(B, a, "Route via Pitkäranta.pdf")
    await s.say(A, bob, "I'll print it 🖨️", reply=route)
    tea = await s.say(A, bob, "/me went to put the kettle on ☕")
    await s.react(B, tea, "😂")
    meet = await s.say(A, bob, "Let's meet on Friday at 20:30 at Ladozhsky staton", gap=240)
    await s.correct(meet, "Let's meet on Friday at 20:30 at Ladozhsky station")
    await s.react(B, meet, "👍")
    await s.react(A, styled, "👌")
    oops = await s.say(B, a, "I'm about to delete this message")
    await s.retract(oops)
    pack = await s.say(B, a, "What to bring:\n\n"
                      "Clothes — thermal underwear, a fleece, a waterproof jacket, two pairs of socks a day. "
                      "Trekking shoes are best: Valaam is rocky, and Ruskeala gets slippery after the rain.\n\n"
                      "Food — snacks for the train and a thermos. The cabin has a kitchen, so we'll buy groceries there.\n\n"
                      "Documents — a passport is a must (no Valaam without it), insurance and some cash: "
                      "mobile data and card terminals there are hit and miss.")
    party = await s.say(A, bob, "🎉")
    await s.marker(B, party)
    clock.ago(12)
    await s.say(B, a, "Don't forget your passport!", gap=0)
    await s.say(B, a, "And a raincoat ☔")

    # --- A stranger and a spammer ---
    clock.ago(70)
    F.send_raw(tag("presence", {"to": a, "type": "subscribe"}, text("nick", "Frank", {"xmlns": NS_NICK})))
    await s.say(F, a, "Hi! It's Frank, we met at the XMPP meetup in Berlin. Please add me to your contacts 🙂")
    await s.file(F, a, "voice-frank.m4a")
    clock.ago(40)
    await s.say(M, a, "🎁 YOU WON an iPhone 17! Claim your prize before midnight: http://free-iphone.example/claim?id=31337")
    report = tag("report", {"xmlns": "urn:xmpp:reporting:1", "reason": "urn:xmpp:reporting:spam"})
    await A.iq(tag("block", {"xmlns": "urn:xmpp:blocking"}, tag("item", {"jid": M.bare}, report)))

    # --- Notes to self, just now ---
    clock.ago(5)
    await s.file(A, a, "voice-alice.ogg", gap=0)

    # --- Alice's devices: rooms to join, how far each chat is read (XEP-0402, XEP-0490) ---
    for room, name, nick in [(GENERAL, "Karelia trip", "Alice"), (DEV, "Crabber dev", "alice")]:
        conference = tag("conference", {"xmlns": NS_BOOKMARKS, "name": name, "autojoin": "true"}, text("nick", nick))
        await A.publish(NS_BOOKMARKS, room, conference, PRIVATE)

    def mds(by, sid):
        return tag("displayed", {"xmlns": NS_MDS}, tag("stanza-id", {"xmlns": NS_SID, "by": by, "id": sid}))

    # Bob's last two and Carol's last two stay unread; in the room, what came after Alice's transfer.
    for peer, by, msg in [(bob, a, pack), (carol, a, carol_read), (GENERAL, GENERAL, read_here)]:
        await A.publish(NS_MDS, peer, mds(by, msg.sids[by]), PRIVATE)


def backdate(times):
    """Moves every archived message to its made-up time. The archive id is that time too, so the
    ids that point at messages (replies, reactions, deletions, read markers) move along, and so
    does the origin_id column, where ejabberd keeps a room message's own id to find it again.
    Retracted and moderated messages are no longer there: ejabberd deletes them from the archive."""
    db = sqlite3.connect(DB, timeout=30)
    rows = db.execute("SELECT id, username, bare_peer, timestamp, xml, origin_id FROM archive ORDER BY timestamp").fetchall()
    moved, last, taken, unknown = {}, {}, set(), 0
    for row_id, user, peer, ts, xml, origin in rows:
        m = re.match(r"<message\b[^>]*?\sid=(['\"])(.*?)\1", xml)
        t = times.get(origin) or times.get(m and m.group(2))
        if t is None:  # made by the server: keeps its place right after what preceded it
            unknown += 1
            t = last.get((user, peer), ts) + 1000
        while (user, t) in taken:
            t += 1
        taken.add((user, t))
        last[(user, peer)] = t
        assert moved.get(str(ts), str(t)) == str(t), f"archive id {ts} used twice"
        moved[str(ts)] = str(t)
    ids = re.compile(r"\b\d{16}\b")

    def fix(s):
        return ids.sub(lambda m: moved.get(m.group(), m.group()), s)

    for row_id, user, peer, ts, xml, origin in rows:
        t = int(moved[str(ts)])
        stamp = datetime.fromtimestamp(t / 1e6, timezone.utc).strftime("%Y-%m-%d %H:%M:%S")
        db.execute("UPDATE archive SET timestamp = ?, xml = ?, origin_id = ?, created_at = ? WHERE id = ?",
                   (t, fix(xml), origin and fix(origin), stamp, row_id))
    for table, column in [("pubsub_item", "payload"), ("spool", "xml")]:
        for rowid, value in db.execute(f"SELECT rowid, {column} FROM {table}").fetchall():
            if fix(value) != value:
                db.execute(f"UPDATE {table} SET {column} = ? WHERE rowid = ?", (fix(value), rowid))
    db.commit()
    print(f"archive: {len(rows)} messages moved back in time, {unknown} of them made by the server")
    for user, n in db.execute("SELECT username, count(*) FROM archive GROUP BY username ORDER BY username"):
        print(f"  {user}: {n}")
    db.close()


async def main():
    bots = {user: Bot(user, nick) for user, (nick, _, _) in PEOPLE.items()}
    for bot in bots.values():
        bot.connect("localhost", 5222)
    await asyncio.wait_for(asyncio.gather(*(b.started for b in bots.values())), 30)
    await asyncio.sleep(1)  # everyone's initial presence is in before the first message
    stand = Stand(bots)
    await story(stand, bots)
    # Alice is offline when Eve invites her: the invite waits for her next login.
    bots["alice"].disconnect()
    await asyncio.sleep(1)
    invite = tag("x", {"xmlns": "jabber:x:conference", "jid": SECRET, "reason": "Join us, we're planning a surprise for Bob 🤫"})
    bots["eve"].send_raw(tag("message", {"to": bots["alice"].bare, "id": str(uuid.uuid4())}, invite))
    await asyncio.sleep(1)
    for bot in bots.values():
        bot.disconnect()
    await asyncio.sleep(2)
    backdate(stand.times)


if __name__ == "__main__":
    asyncio.run(main())
