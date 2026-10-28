#!/usr/bin/env nu
# RustDesk for the benchmark in #33 (docs/benchmark.md), set up without
# clicking through its settings: LAN only, direct IP, one permanent password.
#
#   scripts/bench/rustdesk.nu setup      on Linux: install on the Mac if needed, write its config
#   scripts/bench/rustdesk.nu connect    on Linux: open the Flatpak client to the Mac
#   scripts/bench/rustdesk.nu status     on Linux: is RustDesk running and listening on the Mac
#
# What `setup` writes on the Mac (~/Library/Preferences/com.carriez.RustDesk/):
#   RustDesk2.toml [options]: custom-rendezvous-server, relay-server and
#     api-server pointed at 127.0.0.1 (no registration with RustDesk's public
#     servers, so the Mac can't be reached from the internet by ID),
#     direct-server = Y (direct IP on port 21118), whitelist = the Linux
#     machine's address only, enable-lan-discovery = N, verification by the
#     permanent password only, approve-mode = password.
#   RustDesk.toml: the permanent password, in plain text; RustDesk accepts
#     that (old configs had it) and stores it hashed once it saves its config.
# RustDesk must not run while the files are written (it would write its own
# copy back), so `setup` quits it first.
#
# The password is generated here, stored in the Linux keyring (secret-tool,
# service aetherdesk-bench, item rustdesk-mac) and sent to the Mac over SSH
# on stdin. It is never printed or put on a command line on the Mac.
#
# Needs: $env.BENCH_MAC, and SSH access as in docs/benchmark.md (aether-mac).

const KEYRING = [service aetherdesk-bench item rustdesk-mac]
const PREFS = "~/Library/Preferences/com.carriez.RustDesk"
const APP = "/Applications/RustDesk.app"

def main [] {
  print (help main)
}

# Installs RustDesk on the Mac if missing and writes its LAN-only config.
def "main setup" [
  --new-password   # replace the stored password with a new one
] {
  let mac = (mac-ip)
  let linux = ^ip route get $mac | parse --regex 'src (?<src>\S+)' | get src.0
  let pw = (password $new_password)

  # Homebrew only; it's not installed for you.
  remote $"test -d ($APP) || \(command -v brew >/dev/null && HOMEBREW_NO_AUTO_UPDATE=1 brew install --cask rustdesk) || \(echo no Homebrew on the Mac: install RustDesk by hand; exit 1)"
  # The config, on the Mac by the same script; the password goes over stdin.
  let r = $pw | ^ssh ...(ssh-opts) (ssh-host) $"zsh -lc 'cd (repo) && scripts/bench/rustdesk.nu mac-configure --whitelist ($linux)'" | complete
  print ($r.stdout | str trim)
  if $r.exit_code != 0 { error make {msg: $"on the Mac: ($r.stderr | str trim)"} }
  print "password: in the Linux keyring (secret-tool lookup service aetherdesk-bench item rustdesk-mac)"
}

# Opens the Flatpak RustDesk client, connected to the Mac with the stored password.
def "main connect" [] {
  let mac = (mac-ip)
  let pw = ^secret-tool lookup ...$KEYRING | complete | get stdout | str trim
  if ($pw | is-empty) { error make {msg: "no stored password: run `rustdesk.nu setup` first"} }
  # The password is on this machine's command line for the client's lifetime;
  # the client has no other way to take it.
  ^setsid -f flatpak run com.rustdesk.RustDesk --connect $mac --password $pw o+e> /dev/null
  print $"RustDesk client connecting to the Mac \(($mac | str replace --regex '\d+$' 'x'))…"
}

# Whether RustDesk runs on the Mac, listens for direct IP, and its options.
def "main status" [] {
  let running = remote "pgrep -x RustDesk >/dev/null && echo running || echo stopped"
  let listening = remote "lsof -nP -iTCP:21118 -sTCP:LISTEN >/dev/null 2>&1 && echo yes || echo no"
  let opts = remote $"cat ($PREFS)/RustDesk2.toml 2>/dev/null || true" | from toml | get -o options | default {}
  let pw = remote $"grep -c ^password ($PREFS)/RustDesk.toml 2>/dev/null || echo 0"
  {
    rustdesk: $running
    "listening on 21118": $listening
    "id server": ($opts | get -o custom-rendezvous-server | default "public (default)")
    "direct ip": ($opts | get -o direct-server | default "off")
    whitelist: ($opts | get -o whitelist | default "none" | str replace --all --regex '\d+\.\d+\.\d+\.\d+' '<ip>')
    verification: ($opts | get -o verification-method | default "both")
    "permanent password set": (($pw | into int) > 0)
  } | print
}

# Runs on the Mac: quits RustDesk and writes its config. Password on stdin.
def "main mac-configure" [
  --whitelist: string   # the only address allowed to connect
] {
  if (^uname | str trim) != "Darwin" { error make {msg: "mac-configure runs on the Mac"} }
  let pw = ^cat | str trim
  if ($pw | str length) < 16 { error make {msg: "need a password of 16+ characters on stdin"} }
  if ($pw | str starts-with "00") or ($pw | str starts-with "01") {
    error make {msg: "the password must not start with 00 or 01 (RustDesk reads those as encrypted)"}
  }
  if $whitelist == null { error make {msg: "--whitelist <linux-ip> is required"} }

  # RustDesk writes its config back from memory, so it must not run now.
  ^osascript -e 'quit app "RustDesk"' | complete | ignore
  for _ in 1..20 {
    if (^pgrep -x RustDesk | complete | get exit_code) != 0 { break }
    sleep 250ms
  }
  ^pkill -x RustDesk | complete | ignore

  let dir = $PREFS | path expand
  mkdir $dir
  let f2 = $dir | path join "RustDesk2.toml"
  let cfg2 = if ($f2 | path exists) { open $f2 } else { {} }
  let options = $cfg2 | get -o options | default {} | merge {
    "custom-rendezvous-server": "127.0.0.1"
    "relay-server": "127.0.0.1"
    "api-server": "http://127.0.0.1"
    "direct-server": "Y"
    "whitelist": $whitelist
    "enable-lan-discovery": "N"
    "verification-method": "use-permanent-password"
    "approve-mode": "password"
  }
  $cfg2 | upsert options $options | to toml | save -f $f2

  let f1 = $dir | path join "RustDesk.toml"
  let cfg1 = if ($f1 | path exists) { open $f1 } else { {} }
  $cfg1 | upsert password $pw | to toml | save -f $f1
  ^chmod 600 $f1 $f2
  print $"RustDesk config written \(LAN only, direct IP, whitelist 1 address, permanent password); RustDesk is not running."
}

# --- helpers ---------------------------------------------------------------

def mac-ip [] {
  let ip = $env.BENCH_MAC? | default ""
  if ($ip | is-empty) { error make {msg: "set $env.BENCH_MAC to the Mac's address"} }
  $ip
}

# The stored password, or a new one (stored first).
def password [renew: bool] {
  let old = ^secret-tool lookup ...$KEYRING | complete | get stdout | str trim
  if not $renew and ($old | is-not-empty) { return $old }
  # A letter first: RustDesk reads values starting with 00/01 as encrypted.
  let pw = "R" + (random chars --length 31)
  $pw | ^secret-tool store --label "AetherDesk bench: RustDesk on the Mac" ...$KEYRING
  $pw
}

def ssh-host [] { $env.BENCH_SSH? | default "aether-mac" }
def repo [] { $env.BENCH_MAC_REPO? | default "~/Documents/projects/aetherdesk" }
def ssh-opts [] {
  [
    "-o" "BatchMode=yes" "-o" "ConnectTimeout=5"
    "-o" "ControlMaster=auto" "-o" "ControlPath=~/.ssh/cm-%C" "-o" "ControlPersist=120"
  ]
}

def remote [cmd: string] {
  if ($cmd | str contains "'") { error make {msg: "no single quotes in remote commands"} }
  let r = ^ssh ...(ssh-opts) (ssh-host) $"zsh -lc '($cmd)'" | complete
  if $r.exit_code != 0 { error make {msg: $"on the Mac: ($r.stderr | str trim) ($r.stdout | str trim)"} }
  $r.stdout | str trim --right
}
