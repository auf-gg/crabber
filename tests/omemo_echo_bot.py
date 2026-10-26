"""Reference OMEMO peer for interop tests: python-oldmemo via slixmpp-omemo.

Logs in as carol@localhost, decrypts every OMEMO message and answers "echo: <text>" encrypted.
Run by tests/omemo_interop.rs; needs `pip install slixmpp slixmpp-omemo oldmemo[xml]`.
"""
import asyncio
import ssl
import sys
from typing import Any, Dict, FrozenSet, Optional

from omemo.storage import Just, Maybe, Nothing, Storage
from omemo.types import DeviceInformation, JSONType
from slixmpp import JID, ClientXMPP
from slixmpp.plugins import register_plugin
from slixmpp.stanza import Message
from slixmpp_omemo import XEP_0384, TrustLevel


class MemoryStorage(Storage):
    def __init__(self) -> None:
        super().__init__()
        self.data: Dict[str, JSONType] = {}

    async def _load(self, key: str) -> Maybe[JSONType]:
        return Just(self.data[key]) if key in self.data else Nothing()

    async def _store(self, key: str, value: JSONType) -> None:
        self.data[key] = value

    async def _delete(self, key: str) -> None:
        self.data.pop(key, None)


class XEP_0384Impl(XEP_0384):  # pylint: disable=invalid-name
    def __init__(self, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, **kwargs)
        self.__storage: Optional[Storage] = None

    def plugin_init(self) -> None:
        self.__storage = MemoryStorage()
        super().plugin_init()

    @property
    def storage(self) -> Storage:
        assert self.__storage is not None
        return self.__storage

    @property
    def _btbv_enabled(self) -> bool:
        return True

    async def _devices_blindly_trusted(self, blindly_trusted: FrozenSet[DeviceInformation], identifier: Optional[str]) -> None:
        pass

    async def _prompt_manual_trust(self, manually_trusted: FrozenSet[DeviceInformation], identifier: Optional[str]) -> None:
        session_manager = await self.get_session_manager()
        for device in manually_trusted:
            await session_manager.set_trust(device.bare_jid, device.identity_key, TrustLevel.TRUSTED.value)


register_plugin(XEP_0384Impl)


class EchoBot(ClientXMPP):
    def __init__(self, jid: str, password: str) -> None:
        super().__init__(jid, password)
        self.add_event_handler("session_start", self.start)
        self.add_event_handler("omemo_initialized", lambda _: print("READY", flush=True))
        self.add_event_handler("message", self.message)

    async def start(self, _: Any) -> None:
        self.send_presence()
        await self.get_roster()

    async def message(self, stanza: Message) -> None:
        if stanza["type"] not in ("chat", "normal"):
            return
        xep_0384: XEP_0384 = self["xep_0384"]
        if not xep_0384.is_encrypted(stanza):
            return
        try:
            decrypted, _ = await xep_0384.decrypt_message(stanza)
        except Exception as e:  # pylint: disable=broad-exception-caught
            print("DECRYPT-FAIL", repr(e), flush=True)
            return
        body = decrypted["body"]
        if not body:
            return
        print("GOT", body, flush=True)
        reply = self.make_message(mto=stanza["from"].bare, mtype="chat")
        reply["body"] = "echo: " + body
        encrypted, errors = await xep_0384.encrypt_message(reply, JID(stanza["from"].bare))
        if errors:
            print("ENCRYPT-ERRORS", errors, flush=True)
        if encrypted is not None:
            encrypted.send()


def main() -> None:
    # Windows pipes default to the ANSI code page; the test messages are Cyrillic.
    sys.stdout.reconfigure(encoding="utf-8")  # type: ignore[attr-defined]
    bot = EchoBot("carol@localhost", "carol")
    bot.register_plugin("xep_0199")
    bot.register_plugin("xep_0380")
    bot.register_plugin("xep_0384", module=sys.modules[__name__])
    # The test server uses a self-signed certificate.
    bot.ssl_context.check_hostname = False
    bot.ssl_context.verify_mode = ssl.CERT_NONE
    bot.connect("127.0.0.1", 5222)
    asyncio.get_event_loop().run_forever()


if __name__ == "__main__":
    main()
