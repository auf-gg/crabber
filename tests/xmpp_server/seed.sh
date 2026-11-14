#!/bin/sh
# Rebuilds data/ from scratch: a fresh server, then the accounts, contacts and rooms (here), then
# the chats, files, profiles and read state (seed/seed.py). Everything in data/ is lost.
set -eu
cd "$(dirname "$0")"

# ejabberdctl, minus the "id: unknown ID" line every call prints (the container runs as our uid).
ctl() {
    out=$(docker compose exec -T ejabberd ejabberdctl "$@" 2>&1) || { echo "$out" >&2; return 1; }
    printf '%s\n' "$out" | grep -v '^id: unknown ID' || true
}
# Mutual subscription, with the name and groups each side gives the other.
contacts() { # user1 user2 name-of-2-for-1 groups-of-2-for-1 name-of-1-for-2 groups-of-1-for-2
    ctl add_rosteritem "$1" localhost "$2" localhost "$3" "$4" both
    ctl add_rosteritem "$2" localhost "$1" localhost "$5" "$6" both
}
room() { # name options
    ctl create_room_with_opts "$1" conference.localhost localhost "$2"
}

docker compose down
rm -rf data
mkdir -p data/database data/upload data/logs
touch data/logs/.gitkeep
docker compose up -d --wait ejabberd

for user in admin alice bob carol dave eve frank mallory; do
    ctl register "$user" localhost "$user"
done

# Alice's list: roster names for Bob and Dave only, so Carol and Eve show their own nicknames.
contacts alice bob "Bobby" "Friends" "Alice" ""
contacts alice carol "" "Work" "Alice" "Work"
contacts alice dave "Dmitry Volkov-Zadunaisky" "Work,Friends" "Alice" ""
contacts alice eve "" "" "Alice" ""
contacts bob carol "" "" "" ""
contacts bob dave "" "" "" ""

room general "title:Karelia trip,description:Everything about the trip 🦀,persistent:true,public:true,mam:true,anonymous:false,affiliations:owner=bob@localhost;admin=alice@localhost;member=carol@localhost;member=dave@localhost;member=eve@localhost"
room dev "title:Crabber dev,description:Client development (a semi-anonymous room),persistent:true,public:false,members_only:true,mam:true,anonymous:true,affiliations:owner=alice@localhost;member=bob@localhost;member=dave@localhost"
room secret "title:Secret project 🤫,persistent:true,public:false,members_only:true,mam:true,anonymous:false,affiliations:owner=eve@localhost;member=carol@localhost;member=alice@localhost"

SEED_TZ=$(date +%z) docker compose run --rm --build seed
# The archive was rewritten under the running server: restart it so nothing stale stays cached.
docker compose restart ejabberd
docker compose up -d --wait ejabberd
echo "Done: log in as alice@localhost / alice (host 127.0.0.1, insecure TLS)."
