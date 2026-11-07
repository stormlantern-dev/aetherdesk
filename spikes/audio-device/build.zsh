#!/bin/zsh
# Builds the spike into ./build: the plug-in bundle, the reader and the
# probe. No sudo. Signs ad hoc unless AUROVAL_IDENTITY names an identity.
set -euo pipefail
cd ${0:A:h}
out=build
rm -rf $out && mkdir -p $out/Auroval.driver/Contents/MacOS
flags=(-arch arm64 -O2 -Wall -Wextra -Wno-unused-parameter -mmacosx-version-min=14.0)
clang $flags -bundle -o $out/Auroval.driver/Contents/MacOS/Auroval driver.c \
  -framework CoreFoundation -framework CoreAudio
cp Info.plist $out/Auroval.driver/Contents/
clang $flags -o $out/reader reader.c
clang $flags -o $out/probe probe.c -framework AudioToolbox -framework CoreAudio -framework CoreFoundation
identity=${AUROVAL_IDENTITY:--}
codesign --force --sign $identity $out/Auroval.driver
codesign --force --sign $identity $out/reader
echo "built in ${0:A:h}/$out (signed: $identity)"
