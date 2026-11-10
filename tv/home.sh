#!/bin/sh
# Optional native Home replacement. The webosbrew hook has no .sh suffix.
STATE=/var/lib/astro-home
# The cold-boot hook is a symlink to the copied $STATE/home.sh: find astro-helper beside its target.
SCRIPT=$0
if [ -L "$SCRIPT" ]; then
  LINK=$(readlink "$SCRIPT") || exit 1
  case "$LINK" in /*) SCRIPT=$LINK ;; *) SCRIPT=$(dirname -- "$SCRIPT")/$LINK ;; esac
fi
SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$SCRIPT")" && pwd) || exit 1
COMMAND=${1:-boot}
case "$COMMAND" in
  # fallback: LG Home until the next boot, started by Astro after repeated failed starts.
  enable|disable|restore|status|apply|boot|fallback) ;;
  *) printf '%s\n' 'Usage: home.sh enable [APP_DIR] [HOME_ID] | disable | restore | status' >&2; exit 1 ;;
esac
[ "$(id -u)" = 0 ] || { printf '%s\n' 'Run as root.' >&2; exit 1; }
# boot only schedules the copied $STATE/home.sh apply, which checks for the helper itself.
[ "$COMMAND" = boot ] || [ -x "$SCRIPT_DIR/astro-helper" ] ||
  { printf '%s\n' 'The native Astro TV helper is missing.' >&2; exit 1; }
if [ "$COMMAND" = enable ] && grep -q 'release [569]\.' /etc/starfish-release 2>/dev/null; then
  printf '%s\n' \
    'Enabling Astro Home (webOS 5/6/24) will:' \
    '  1. Close any running normal Astro instance.' \
    "  2. Create $STATE with integration scripts and saved state." \
    "  3. Bind-mount Astro's executable over the selected LG Home executable." \
    "     LG's Home manifest and role stay; Astro registers with SAM itself." \
    '  4. Stop the previous Home process and launch Astro as Home.' \
    '  5. Install /var/lib/webosbrew/init.d/64-astro-home to reapply the mount at cold boot.' \
    '' \
    "LG Home's original executable remains underneath the mount." \
    'Home Auto Launch and Quick Start+ retain their current settings.' \
    "Restore LG Home with: sh $STATE/home.sh disable" \
    ''
elif [ "$COMMAND" = enable ]; then
  printf '%s\n' \
    'Enabling Astro Home will:' \
    '  1. Close any running normal Astro instance.' \
    "  2. Create $STATE with integration scripts, saved state and a staged native Home app linked to Astro." \
    "  3. Bind-mount $STATE/app over /usr/palm/applications/com.webos.app.home." \
    '  4. Temporarily block and release Home so SAM rereads its manifest.' \
    '  5. Stop the previous Home process and launch Astro as Home.' \
    '  6. Install /var/lib/webosbrew/init.d/64-astro-home to reapply the mount at cold boot.' \
    '' \
    "LG Home's original files remain underneath the mount." \
    'Home Auto Launch and Quick Start+ retain their current settings.' \
    "Restore LG Home with: sh $STATE/home.sh disable" \
    ''
fi
if [ "$COMMAND" = enable ]; then
  printf '%s\n' "Selected Home target: ${3:-saved target, otherwise com.webos.app.home}"

  while :; do
    printf '%s' 'Apply these changes? [y/n]: '
    if ! IFS= read -r answer; then
      printf '\n%s\n' 'Cancelled. No Home integration changes applied.'
      exit 1
    fi
    case "$answer" in
      y|Y) break ;;
      n|N) printf '%s\n' 'Cancelled. No Home integration changes applied.'; exit 0 ;;
      *) printf '%s\n' 'Please answer y or n.' ;;
    esac
  done
fi
[ ! -L "$STATE" ] || exit 1
if [ ! -d "$STATE" ]; then mkdir -m 755 "$STATE" || exit 1; fi
[ "$(stat -c %u "$STATE")" = 0 ] || exit 1
if [ "$COMMAND" = boot ]; then
  # Seconds since boot in boot.log, to see where the delay before Astro appears goes.
  uptime_s() { cut -d' ' -f1 /proc/uptime 2>/dev/null; }
  # Homebrew Channel runs init.d late, with LG Home already shown, so apply starts at once.
  # A failed apply restores LG Home by itself; retry a few times in case SAM was still busy.
  (
    printf 'Astro Home: boot hook at %s s\n' "$(uptime_s)"
    attempt=1
    while :; do
      printf 'Astro Home: apply started at %s s (attempt %s)\n' "$(uptime_s)" "$attempt"
      sh "$STATE/home.sh" apply
      status=$?
      if [ "$status" = 0 ] || [ "$attempt" -ge 3 ]; then break; fi
      attempt=$((attempt + 1))
      sleep 2
    done
    printf 'Astro Home: apply ended at %s s (exit %s)\n' "$(uptime_s)" "$status"
  ) </dev/null >>"$STATE/boot.log" 2>&1 &
  exit 0
fi
exec 9>"$STATE/lock"
flock -n 9 || { printf '%s\n' 'Astro Home integration is already being changed.' >&2; exit 1; }
exec "$SCRIPT_DIR/astro-helper" home "$COMMAND" "${2:-$SCRIPT_DIR}" "${3:-}"
