#!/bin/bash
# Import Cali's signing identities into cali's LOGIN keychain on a runner Mac,
# so fucina jobs with FUCINA_SESSION: gui can sign there (written for the
# speedwagon -> trish takeover, 2026-11-07). Safe to re-run.
#
# Run AS cali, secrets on stdin ONLY, one per line, in this order:
#   1. cali's login password (the sudo password AND the login keychain password)
#   2. p12_base64     (vault infra/apple-cert-apple-distribution)
#   3. p12_password   (same path)
#
#   ssh trish 'umask 077; cat > ~/.fucina-import.sh' < scripts/import-signing-identities.sh
#   printf '%s\n%s\n%s\n' "$PW" "$P12B64" "$P12PW" | ssh trish 'bash ~/.fucina-import.sh; rm -f ~/.fucina-import.sh'
#
# (Ship the script and the secrets separately: with `bash -s` the script text
# and the secrets would share stdin.)
#
# Passwords never reach argv. Keychain commands go to `security -i` on stdin,
# and sudo reads the password with -S. Keychain writes run inside cali's Aqua
# session through `launchctl asuser`, which needs root, hence the sudo.
set -euo pipefail

IFS= read -r PW || true
IFS= read -r P12B64 || true
IFS= read -r P12PW || true
exec </dev/null # nothing else may read the secrets stream

die() { echo "import-signing-identities: $*" >&2; exit 1; }
[ "$(id -un)" = cali ] || die "run as cali"
[ -n "$PW" ] && [ -n "$P12B64" ] && [ -n "$P12PW" ] || die "expected 3 non-empty lines on stdin"
for v in "$PW" "$P12PW"; do
  case "$v" in *'"'* | *'\'*) die "a password contains \" or \\, which security -i quoting cannot carry" ;; esac
done

UID_CALI=$(id -u cali)
KC="$HOME/Library/Keychains/login.keychain-db"
W="$(mktemp -d "${TMPDIR:-/tmp}/fucina-import.XXXXXX")"
chmod 700 "$W"
cleanup() { rm -rf "$W"; sudo -k 2>/dev/null || true; }
trap cleanup EXIT

# One sudo authentication for the whole run; the password goes in on stdin.
printf '%s\n' "$PW" | sudo -S -p '' -v 2>/dev/null || die "sudo authentication failed"
# Run a command as cali inside cali's Aqua session.
in_aqua() { sudo -n /bin/launchctl asuser "$UID_CALI" sudo -n -u cali "$@"; }
# Keychain commands on stdin, never on argv. Exits non-zero if any command fails.
sec_i() { in_aqua /usr/bin/security -i >/dev/null; }

# --- preflight, on a scratch keychain with a dummy password -------------------
# If stdin didn't reach `security -i` through asuser, it would see EOF, do
# nothing and exit 0: a false pass. So check the scratch keychain was CREATED.
echo "session in the asuser chain: $(in_aqua /bin/launchctl managername)" # expect Aqua
S="$W/preflight.keychain-db"
printf 'create-keychain -p "pre?flight x" %s\n' "$S" | sec_i || die "preflight: create-keychain failed"
[ -f "$S" ] || die "preflight: stdin does not reach security -i through launchctl asuser"
printf 'lock-keychain %s\nunlock-keychain -p "pre?flight x" %s\n' "$S" "$S" | sec_i \
  || die "preflight: unlock with the right password failed"
in_aqua /usr/bin/security delete-keychain "$S"
echo "preflight: OK"

# --- public intermediates, pinned by SHA-256 ------------------------------------
fetch() { # name sha256
  curl -sfL -o "$W/$1.cer" "https://www.apple.com/certificateauthority/$1.cer" || die "$1: download failed"
  echo "$2  $W/$1.cer" | shasum -a 256 -c --status || die "$1: SHA-256 mismatch, refusing"
}
fetch AppleWWDRCAG3 dcf21878c77f4198e4b4614f03d696d89c66c66008d4244e1b99161aac91601f
fetch DeveloperIDG2CA f16cd3c54c7f83cea4bf1a3e6a0819c8aaa8e4a1528fd144715f350643d2df3a

# --- unlock the login keychain (fails loudly on a wrong password) --------------
printf 'unlock-keychain -p "%s" %s\n' "$PW" "$KC" | sec_i || die "login keychain unlock failed (wrong password?)"

# Intermediates: "already exists" is fine, so one call each and tolerate it.
for c in AppleWWDRCAG3 DeveloperIDG2CA; do
  printf 'import %s -k %s\n' "$W/$c.cer" "$KC" | sec_i || true
done

# --- identities: import only if one is missing -----------------------------------
IDS=(
  "Developer ID Application: Nico Bousquet (XJQQCN392F)"
  "Developer ID Installer: Nico Bousquet (XJQQCN392F)"
  "Apple Distribution: Nico Bousquet (XJQQCN392F)"
)
have() { in_aqua /usr/bin/security find-identity -v "$KC" 2>/dev/null | grep -qF "$1"; }
missing=0
for i in "${IDS[@]}"; do have "$i" || missing=$((missing + 1)); done
echo "before: $(in_aqua /usr/bin/security find-identity -v -p codesigning "$KC" | tail -1 | xargs); required identities missing: $missing/3"

if [ "$missing" -gt 0 ]; then
  printf '%s' "$P12B64" | base64 -D >"$W/id.p12"
  [ -s "$W/id.p12" ] || die "p12 decode failed"
  # A partly-present p12 reports "already exists"; the presence check below decides.
  printf 'import %s -k %s -P "%s" -T /usr/bin/codesign -T /usr/bin/productsign -T /usr/bin/productbuild -T /usr/bin/xcodebuild\n' \
    "$W/id.p12" "$KC" "$P12PW" | sec_i || true
  rm -f "$W/id.p12"
  for i in "${IDS[@]}"; do have "$i" || die "after import, still missing: $i"; done
else
  echo "all required identities already present, skipping the import"
fi

# --- partition list: what lets codesign use the keys without a UI prompt --------
printf 'set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "%s" %s\n' "$PW" "$KC" | sec_i \
  || die "set-key-partition-list failed"

# --- proof: sign a throwaway binary with both identities, headless ---------------
fail=0
for who in "Developer ID Application" "Apple Distribution"; do
  cp /usr/bin/true "$W/t"
  if in_aqua /usr/bin/codesign -f -s "$who: Nico Bousquet (XJQQCN392F)" --keychain "$KC" "$W/t" >/dev/null 2>&1 \
    && in_aqua /usr/bin/codesign --verify "$W/t" >/dev/null 2>&1; then
    echo "codesign with $who: OK"
  else
    echo "codesign with $who: FAILED"
    fail=1
  fi
done
echo "after: $(in_aqua /usr/bin/security find-identity -v -p codesigning "$KC" | tail -1 | xargs)"
in_aqua /usr/bin/security find-identity -v "$KC" | grep -E "Developer ID|Apple Distribution" | sed 's/^/  /'
[ "$fail" = 0 ] || die "signing proof failed"
echo "import-signing-identities: done"
