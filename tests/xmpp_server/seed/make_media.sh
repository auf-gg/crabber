#!/bin/sh
# Regenerates the files seed.py sends (seed/media/). Needs ffmpeg (libx264, libvpx-vp9, libopus,
# libmp3lame), ImageMagick 7 and python3. The results are committed: run this only to change them.
set -eu
cd "$(dirname "$0")"
ROOT=$(cd ../../.. && pwd)
FONT=${FONT:-/usr/share/fonts/noto/NotoSans-Bold.ttf}
MONO=${MONO:-/usr/share/fonts/noto/NotoSansMono-Regular.ttf}
mkdir -p media avatars
ff() { ffmpeg -nostdin -hide_banner -loglevel error -y "$@"; }
# Caption in the corner, so a wrong crop or scale is visible at a glance.
label() { magick "$1" -gravity southwest -font "$FONT" -pointsize "$3" -fill white -stroke black -strokewidth 2 -annotate +24+20 "$2" "$1"; }

# --- images ---
magick -seed 7 -size 1600x1067 plasma:skyblue-forestgreen -blur 0x6 -modulate 100,130 -quality 85 media/karelia.jpg
label media/karelia.jpg "JPEG 1600×1067" 48
ff -f lavfi -i "mandelbrot=s=1080x1440:start_scale=1.2:end_scale=1.2" -frames:v 1 -q:v 3 media/portrait.jpg
label media/portrait.jpg "JPEG 1080×1440" 56
magick -seed 21 -size 1280x960 plasma:orange-purple -blur 0x10 -quality 85 media/team.jpg
label media/team.jpg "Team, 1280×960" 48
# Stored sideways with EXIF Orientation=6: shown right only by a viewer that applies it.
magick -size 1512x2016 gradient:'#ffd27f'-'#3b6fd4' -gravity center -font "$FONT" -pointsize 160 -fill white -stroke black \
  -strokewidth 4 -annotate +0-200 "UP" -pointsize 70 -annotate +0+150 "EXIF Orientation 6" -rotate -90 -orient RightTop \
  -quality 85 media/phone-photo.jpg
# ImageMagick writes no EXIF of its own: add an APP1 with just Orientation=6 after the JFIF header.
python3 -I - media/phone-photo.jpg <<'EOF'
import struct, sys
path = sys.argv[1]
data = open(path, "rb").read()
tiff = b"MM\x00\x2a" + struct.pack(">IH", 8, 1) + struct.pack(">HHIHH", 0x0112, 3, 1, 6, 0) + struct.pack(">I", 0)
app1 = b"\xff\xe1" + struct.pack(">H", 2 + 6 + len(tiff)) + b"Exif\x00\x00" + tiff
at = 2 + 2 + struct.unpack(">H", data[4:6])[0] if data[2:4] == b"\xff\xe0" else 2
open(path, "wb").write(data[:at] + app1 + data[at:])
EOF
ff -f lavfi -i "gradients=s=4000x700:c0=0x0b1d51:c1=0xff7e5f:c2=0xfeb47b:n=3:type=linear:speed=0" -frames:v 1 -q:v 3 media/panorama.jpg
label media/panorama.jpg "Panorama 4000×700" 64
magick -size 1280x800 xc:'#1e1e2e' -font "$MONO" -pointsize 26 -fill '#a6e3a1' -annotate +40+70 '$ cargo test --test live -- --ignored' \
  -fill '#cdd6f4' -annotate +40+120 'running 12 tests' -annotate +40+160 'test chat_roundtrip ... ok' \
  -annotate +40+200 'test muc_history ... ok' -fill '#f38ba8' -annotate +40+240 'test file_upload ... FAILED' \
  -fill '#cdd6f4' -annotate +40+300 'failures:' -annotate +40+340 '    upload slot: service-unavailable' \
  -fill '#f9e2af' -annotate +40+400 'test result: FAILED. 11 passed; 1 failed' media/screenshot.png
label media/screenshot.png "PNG 1280×800" 36
magick "$ROOT/assets/crabber.png" -resize 512x512 media/logo.png
magick "$ROOT/assets/crabber.png" -resize 420x420 -background none -rotate 12 -gravity center -extent 512x512 -define webp:lossless=false \
  -quality 90 media/sticker.webp
magick -size 24x24 xc:'#e64553' -fill white -draw 'circle 12,12 12,5' media/tiny.png
magick "$ROOT/assets/crabber.png" -resize 200x200 -background none -gravity center -extent 280x280 -write mpr:crab +delete \
  $(seq 0 15 345 | sed 's/.*/( mpr:crab -background none -rotate & -gravity center -extent 280x280 -background #f5e0dc -flatten )/') \
  -delay 6 -loop 0 -layers Optimize media/spin.gif

# --- video ---
ff -f lavfi -t 6 -i "testsrc2=s=1280x720:r=25" -f lavfi -t 6 -i "sine=f=440:sample_rate=48000" \
  -vf "drawtext=fontfile=$FONT:text='MP4 H.264 1280×720':fontsize=48:fontcolor=white:box=1:boxcolor=black@0.5:x=40:y=h-90" \
  -c:v libx264 -preset slow -crf 30 -pix_fmt yuv420p -c:a aac -b:a 64k -movflags +faststart media/waterfall.mp4
ff -f lavfi -t 4 -i "mandelbrot=s=720x1280:rate=25" -f lavfi -t 4 -i "sine=f=660:sample_rate=48000" \
  -vf "drawtext=fontfile=$FONT:text='WebM VP9 720×1280':fontsize=44:fontcolor=white:box=1:boxcolor=black@0.5:x=30:y=h-90" \
  -c:v libvpx-vp9 -b:v 600k -deadline good -cpu-used 4 -c:a libopus -b:a 48k media/vertical.webm

# --- audio ---
# "Speech": a wobbling tone in syllable-like bursts, so the waveform is not flat.
voice() { # file seconds pitch
  ff -f lavfi -i "aevalsrc='0.5*sin(2*PI*($3+30*sin(2*PI*5*t))*t)*pow(abs(sin(2*PI*1.7*t)),0.6)*(0.6+0.4*sin(2*PI*0.3*t))':s=48000:d=$2" \
    -ac 1 -c:a libopus -b:a 24k -application voip "$1"
}
voice media/voice-bob.oga 7 170
voice media/voice-alice.ogg 3 260
voice media/voice-dave.opus 42 130
voice media/voice-room.oga 5 150
ff -f lavfi -i "aevalsrc='0.5*sin(2*PI*220*t)*abs(sin(2*PI*1.3*t))':s=44100:d=4" -ac 1 -c:a aac -b:a 48k media/voice-frank.m4a
ff -f lavfi -i "aevalsrc='0.3*sin(2*PI*(261.63*pow(2,floor(mod(t*2,8))/12*2))*t)+0.2*sin(2*PI*130.81*t)':s=44100:d=12" \
  -c:a libmp3lame -b:a 96k -metadata title="Test melody" -metadata artist="Crabber" media/melody.mp3

# --- documents ---
magick -size 1240x1754 xc:white -font "$FONT" -fill '#1f2937' -pointsize 64 -annotate +100+200 'Karelia route' \
  -pointsize 36 -annotate +100+320 '1. Petrozavodsk — Kizhi (ferry, 1.5 h)' -annotate +100+390 '2. Sortavala — Ruskeala' \
  -annotate +100+460 '3. Valaam (hydrofoil, 1 h)' -annotate +100+530 '4. Back through Lahdenpohja' \
  -fill '#6b7280' -pointsize 28 -annotate +100+1650 'A test PDF for Crabber' "media/Route via Pitkäranta.pdf"
python3 -I - media <<'EOF'
import sys, zipfile, pathlib
d = pathlib.Path(sys.argv[1])
with zipfile.ZipFile(d / "photos.zip", "w", zipfile.ZIP_DEFLATED) as z:
    for n in ["karelia.jpg", "team.jpg", "logo.png"]:
        z.write(d / n, n)
EOF

# --- avatars (dave has none: the fallback gets tested too). Small: ejabberd caps a PEP item at 250 KB ---
magick -size 256x256 radial-gradient:'#f5c2e7'-'#8839ef' -fill white -draw 'circle 128,128 128,70' -fill '#8839ef' \
  -draw 'circle 128,128 128,100' -depth 8 -colors 64 avatars/alice.png
magick -seed 3 -size 256x256 plasma:'#04a5e5'-'#40a02b' -blur 0x4 -quality 85 avatars/bob.jpg
ff -f lavfi -i "mandelbrot=s=256x256:start_x=-0.743643887037151:start_y=0.13182590420533:start_scale=0.002:end_scale=0.002" \
  -frames:v 1 avatars/carol.png
magick avatars/carol.png -colors 128 avatars/carol.png
magick -seed 11 -size 256x256 plasma:'#fe640b'-'#df8e1d' -blur 0x3 -quality 85 avatars/eve.jpg
magick -size 256x256 xc:'#179299' -fill '#eff1f5' -draw 'roundrectangle 48,48 208,208 30,30' -fill '#179299' \
  -draw 'rectangle 96,96 160,160' avatars/frank.png
magick -size 256x256 pattern:checkerboard -fill '#d20f39' -colorize 40 avatars/room-general.png
magick "$ROOT/assets/crabber.png" -resize 256x256 avatars/room-dev.png

ls -la media avatars
