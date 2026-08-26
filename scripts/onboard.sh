#!/bin/sh
# Fullscreen Muxa onboarding without installing Muxa.
#
#   curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh
#
# Forward flags with `sh -s --`:
#
#   curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh -s -- --lang ko
#   curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh -s -- --print
#
# The tour you get is `muxa onboard` itself: this script fetches the release
# binary for the host into a temporary directory, verifies its published
# SHA-256, runs the onboarding, and deletes it on exit. Nothing is installed —
# no daemon, no config, no real tmux session, nothing left behind.
#
# `--no-download` — or an unsupported platform, a missing checksum tool, or no
# network — falls back to the simulation embedded below, which draws the same
# shell → tmux → Muxa scenario with plain ANSI controls. The fallback tracks the
# real tour step for step; `scripts/onboarding-parity.sh` is what keeps it
# honest.

set -eu

language=auto
print_only=0
no_quiz=0
allow_download=1

usage() {
  printf '%s\n' \
    'Usage: onboard.sh [--lang auto|en|ko] [--print] [--no-quiz] [--no-download]' \
    '' \
    'Runs the real muxa onboard from a throwaway copy of the release binary,' \
    'or an embedded shell → tmux → Muxa simulation when that is unavailable.' \
    '' \
    'Options:' \
    '  --lang auto|en|ko  Display language (default: detect from locale)' \
    '  --print            Print the complete guide instead of opening the TUI' \
    '  --no-quiz          Use Enter to move through the fullscreen tour' \
    '  --no-download      Always use the embedded simulation' \
    '  -h, --help         Show this help'
}

fail() {
  printf 'muxa-onboard: %s\n' "$*" >&2
  exit 1
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --lang)
      [ "$#" -ge 2 ] || fail '--lang needs auto, en, or ko'
      language=$2
      shift 2
      ;;
    --lang=*) language=${1#--lang=}; shift ;;
    --print) print_only=1; shift ;;
    --no-quiz) no_quiz=1; shift ;;
    --no-download) allow_download=0; shift ;;
    --tmux) shift ;; # Compatibility: tmux is always part of this tour.
    -h | --help) usage; exit 0 ;;
    --) shift; break ;;
    *) fail "unknown option: $1" ;;
  esac
done

[ "$#" -eq 0 ] || fail "unexpected argument: $1"

case "$language" in
  auto)
    locale=${LC_ALL:-${LC_MESSAGES:-${LANG:-}}}
    case "$locale" in
      ko | ko_* | ko-* | ko.* | KO | KO_* | KO-* | KO.*) language=ko ;;
      *) language=en ;;
    esac
    ;;
  en | ko) ;;
  *) fail "unsupported language: $language (expected auto, en, or ko)" ;;
esac

print_guide() {
  if [ "$language" = ko ]; then
    printf '%s\n' \
      'Muxa 통합 온보딩 · shell → tmux → Muxa · 20단계' \
      '=================================================' \
      '' \
      '이 가이드는 실제 환경을 변경하지 않는 전체 화면 simulation입니다.' \
      '' \
      '  1. 가상 shell에서 연습용 session 생성' \
      '  2. tmux prefix 입력' \
      '  3. session/window tree' \
      '  4. 새 window 생성' \
      '  5. 좌우 pane 분할' \
      '  6. 상하 pane 분할' \
      '  7. pane 이동' \
      '  8. pane zoom 전환' \
      '  9. copy mode 진입과 종료' \
      ' 10. detach와 attach' \
      ' 11. Muxa managed binding과 watch 진입' \
      ' 12. work 선택' \
      ' 13. child agent 선택' \
      ' 14. attention/state 정렬' \
      ' 15. pane preview' \
      ' 16. 전체 shortcut 도움말' \
      ' 17. new work form과 안전한 취소' \
      ' 18. message composer와 mailbox' \
      ' 19. agent-side Muxa MCP pattern' \
      ' 20. 실제 설치로 이어지는 다음 단계' \
      '' \
      'Muxa binary, daemon, config, 실제 tmux session은 사용하지 않습니다.'
  else
    printf '%s\n' \
      'Muxa unified onboarding · shell → tmux → Muxa · 20 steps' \
      '=========================================================' \
      '' \
      'This is a fullscreen simulation that changes no real environment.' \
      '' \
      '  1. Create a practice session in the virtual shell' \
      '  2. Enter the tmux prefix' \
      '  3. Open the session/window tree' \
      '  4. Create a window' \
      '  5. Split a pane left/right' \
      '  6. Split a pane top/bottom' \
      '  7. Move between panes' \
      '  8. Toggle pane zoom' \
      '  9. Enter and leave copy mode' \
      ' 10. Detach and attach' \
      ' 11. Enter Muxa watch through managed bindings' \
      ' 12. Select work' \
      ' 13. Select a child agent' \
      ' 14. Sort by attention/state' \
      ' 15. Preview a pane' \
      ' 16. Open shortcut help' \
      ' 17. Open and safely cancel the new-work form' \
      ' 18. Use the message composer and mailbox' \
      ' 19. Learn the agent-side Muxa MCP pattern' \
      ' 20. Continue to a real installation' \
      '' \
      'No Muxa binary, daemon, config, or real tmux session is used.'
  fi
}

# --------------------------------------------------------------------------
# Preferred path: run the real `muxa onboard`.
#
# The release binary is fetched into a temp directory, checked against its
# published SHA-256, run, and deleted. This is a download, never an install:
# no daemon, no config, no PATH entry, nothing left behind.
# --------------------------------------------------------------------------

release_repo=${MUXA_ONBOARD_REPO:-Open330/muxa}
download_dir=

detect_release_target() {
  case "$(uname -s 2>/dev/null || printf unknown)" in
    Darwin) target_os=apple-darwin ;;
    Linux) target_os=unknown-linux-gnu ;;
    *) return 1 ;;
  esac
  case "$(uname -m 2>/dev/null || printf unknown)" in
    x86_64 | amd64) target_arch=x86_64 ;;
    arm64 | aarch64) target_arch=aarch64 ;;
    *) return 1 ;;
  esac
  printf '%s-%s' "$target_arch" "$target_os"
}

fetch_url() { # url [dest]; prints to stdout when no dest is given
  if command -v curl >/dev/null 2>&1; then
    if [ "$#" -ge 2 ]; then curl -fsSL "$1" -o "$2"; else curl -fsSL "$1"; fi
  elif command -v wget >/dev/null 2>&1; then
    if [ "$#" -ge 2 ]; then wget -qO "$2" "$1"; else wget -qO- "$1"; fi
  else
    return 1
  fi
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    return 1
  fi
}

discard_download() {
  [ -n "$download_dir" ] || return 0
  rm -rf "$download_dir"
  download_dir=
  trap - EXIT INT TERM HUP
}

# Exits the script on success; returns non-zero to fall back to the simulation.
run_release_onboarding() {
  [ "$allow_download" -eq 1 ] || return 1
  command -v tar >/dev/null 2>&1 || return 1
  command -v mktemp >/dev/null 2>&1 || return 1
  target=$(detect_release_target) || return 1

  version=${MUXA_ONBOARD_VERSION:-}
  if [ -z "$version" ]; then
    version=$(fetch_url "https://api.github.com/repos/$release_repo/releases/latest" 2>/dev/null |
      sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1) || return 1
  fi
  [ -n "$version" ] || return 1

  download_dir=$(mktemp -d 2>/dev/null) || { download_dir=; return 1; }
  trap 'rm -rf "$download_dir"' EXIT INT TERM HUP

  release=muxa-$version-$target
  base=https://github.com/$release_repo/releases/download/$version

  printf 'muxa-onboard: fetching muxa %s (%s) — runs from a temp dir, nothing is installed\n' \
    "$version" "$target" >&2
  fetch_url "$base/$release.tar.gz" "$download_dir/$release.tar.gz" || return 1
  fetch_url "$base/$release.sha256" "$download_dir/$release.sha256" || return 1

  published=$(sed -n 's/^\([0-9a-fA-F]\{64\}\).*/\1/p' "$download_dir/$release.sha256" | head -1)
  actual=$(sha256_of "$download_dir/$release.tar.gz") || return 1
  if [ -z "$published" ] || [ "$published" != "$actual" ]; then
    printf 'muxa-onboard: checksum mismatch for %s, not running it\n' "$release.tar.gz" >&2
    return 1
  fi

  tar -xzf "$download_dir/$release.tar.gz" -C "$download_dir" || return 1
  muxa_bin=$download_dir/$release/muxa
  [ -x "$muxa_bin" ] || muxa_bin=$(find "$download_dir" -type f -name muxa 2>/dev/null | head -1)
  [ -n "$muxa_bin" ] && [ -f "$muxa_bin" ] || return 1
  chmod +x "$muxa_bin" 2>/dev/null || :

  set -- onboard --lang "$language"
  [ "$print_only" -eq 1 ] && set -- "$@" --print
  [ "$no_quiz" -eq 1 ] && set -- "$@" --no-quiz

  # This script is usually piped into `sh`, so stdin is the pipe, not the
  # terminal the tour has to read keys from.
  # `-r` only checks permission bits; opening still fails with ENXIO when the
  # process has no controlling terminal, so probe the open itself.
  status=0
  if ( : </dev/tty ) 2>/dev/null; then
    "$muxa_bin" "$@" </dev/tty || status=$?
  else
    "$muxa_bin" "$@" || status=$?
  fi
  discard_download
  exit "$status"
}

if ! run_release_onboarding; then
  discard_download
fi

if [ "$print_only" -eq 1 ] || [ ! -r /dev/tty ] || [ ! -t 1 ]; then
  print_guide
  exit 0
fi

command -v stty >/dev/null 2>&1 || fail 'fullscreen mode needs stty'
command -v dd >/dev/null 2>&1 || fail 'fullscreen mode needs dd'

exec 3</dev/tty
saved_stty=$(stty -g <&3) || fail 'could not read terminal settings'
finished=0

esc=$(printf '\033')
normal=$(printf '\033[0;37;48;5;233m')
bold=$(printf '\033[1m')
dim=$(printf '\033[38;5;242m')
cyan=$(printf '\033[1;38;5;51m')
green=$(printf '\033[1;38;5;48m')
yellow=$(printf '\033[1;38;5;226m')
red=$(printf '\033[1;38;5;196m')
border=$(printf '\033[38;5;240m')
selected=$(printf '\033[1;37;48;5;24m')

cleanup() {
  stty "$saved_stty" <&3 2>/dev/null || :
  printf '%s[?25h%s[?1049l%s' "$esc" "$esc" "$(printf '\033[0m')"
  if [ "$finished" -eq 1 ]; then
    if [ "$language" = ko ]; then
      printf '%s\n' \
        '' \
        'Muxa onboarding 완료 — 실제 환경은 변경되지 않았습니다.' \
        '' \
        '설치하기 (pre-built, 권장):' \
        '  brew install open330/tap/muxa' \
        '  muxa init' \
        '' \
        '직접 다운로드: https://github.com/Open330/muxa/releases/latest' \
        '설치 안내:     https://github.com/Open330/muxa#install-muxa'
    else
      printf '%s\n' \
        '' \
        'Muxa onboarding complete — your real environment was not changed.' \
        '' \
        'Install (pre-built, recommended):' \
        '  brew install open330/tap/muxa' \
        '  muxa init' \
        '' \
        'Direct download: https://github.com/Open330/muxa/releases/latest' \
        'Install guide:   https://github.com/Open330/muxa#install-muxa'
    fi
  fi
}

trap cleanup 0
trap 'exit 129' 1
trap 'exit 130' 2
trap 'exit 143' 15

stty -echo -icanon min 1 time 0 <&3
printf '%s[?1049h%s[?25l' "$esc" "$esc"

rows=24
cols=80
refresh_size() {
  size=$(stty size <&3 2>/dev/null || printf '24 80')
  # shellcheck disable=SC2086 # stty returns exactly two whitespace fields.
  set -- $size
  rows=${1:-24}
  cols=${2:-80}
  [ "$rows" -gt 0 ] 2>/dev/null || rows=24
  [ "$cols" -gt 0 ] 2>/dev/null || cols=80
}

repeat() {
  repeat_char=$1
  repeat_count=$2
  repeat_value=
  while [ "$repeat_count" -gt 0 ]; do
    repeat_value=$repeat_value$repeat_char
    repeat_count=$((repeat_count - 1))
  done
  printf '%s' "$repeat_value"
}

move_to() {
  printf '%s[%s;%sH' "$esc" "$1" "$2"
}

put() {
  move_to "$1" "$2"
  printf '%s%s%s' "$3" "$4" "$normal"
}

clear_screen() {
  printf '%s%s[2J%s[H' "$normal" "$esc" "$esc"
}

draw_box() {
  box_top=$1
  box_left=$2
  box_width=$3
  box_height=$4
  box_title=$5
  box_color=$6

  [ "$box_width" -ge 4 ] || return 0
  [ "$box_height" -ge 3 ] || return 0

  move_to "$box_top" "$box_left"
  printf '%s┌' "$box_color"
  repeat '─' $((box_width - 2))
  printf '┐%s' "$normal"

  box_row=1
  while [ "$box_row" -lt $((box_height - 1)) ]; do
    move_to $((box_top + box_row)) "$box_left"
    printf '%s│%s' "$box_color" "$normal"
    repeat ' ' $((box_width - 2))
    printf '%s│%s' "$box_color" "$normal"
    box_row=$((box_row + 1))
  done

  move_to $((box_top + box_height - 1)) "$box_left"
  printf '%s└' "$box_color"
  repeat '─' $((box_width - 2))
  printf '┘%s' "$normal"

  if [ -n "$box_title" ]; then
    put "$box_top" $((box_left + 2)) "$box_color$bold" " $box_title "
  fi
}

# One byte at a time cannot tell a lone Esc from the leading byte of an arrow
# key or an Alt chord, so every arrow used to read as "quit". Read exactly the
# rest of the sequence — never past its final byte, or the next keystroke would
# be swallowed with it — and classify it. `key` ends up as the literal
# character, an arrow sentinel, or a whole multi-byte glyph; `key_alt` records
# whether the terminal sent Meta.
read_key() {
  key_alt=0
  key=$(read_byte)

  case "$key" in
    "$esc")
      key_tail=$(read_byte_pending)
      case "$key_tail" in
        '') ;; # Nothing followed: a real Esc.
        '[' | 'O')
          key_sequence=$key_tail
          while :; do
            sequence_byte=$(read_byte_pending)
            [ -n "$sequence_byte" ] || break
            key_sequence=$key_sequence$sequence_byte
            case "$sequence_byte" in [A-Za-z~]) break ;; esac
          done
          case "$key_sequence" in
            '[A' | 'OA') key=$key_up ;;
            '[B' | 'OB') key=$key_down ;;
            '[C' | 'OC') key=$key_right ;;
            '[D' | 'OD') key=$key_left ;;
            *) key=$key_nav ;; # Some other CSI/SS3 key, never a quit.
          esac
          ;;
        *) key=$key_tail; key_alt=1 ;; # Esc-prefixed Alt chord.
      esac
      ;;
    "$utf8_lead_two" | "$utf8_lead_three")
      # macOS composes Option into a glyph — Option+T is `†` — instead of
      # sending Meta, and a glyph arrives as several bytes.
      continuation=1
      [ "$key" = "$utf8_lead_three" ] && continuation=2
      while [ "$continuation" -gt 0 ]; do
        key=$key$(read_byte_pending)
        continuation=$((continuation - 1))
      done
      ;;
  esac
}

read_byte() {
  byte_with_sentinel=$(dd bs=1 count=1 <&3 2>/dev/null; printf '_')
  printf '%s' "${byte_with_sentinel%_}"
}

# One byte if the terminal already has one, empty otherwise.
read_byte_pending() {
  stty min 0 time 1 <&3
  byte_with_sentinel=$(dd bs=1 count=1 <&3 2>/dev/null; printf '_')
  stty min 1 time 0 <&3
  printf '%s' "${byte_with_sentinel%_}"
}

# True when the last key was the taught Alt chord for letter $1, whether the
# terminal sent Meta or only the macOS compose glyph.
key_is_alt_chord() {
  if [ "$key_alt" -eq 1 ]; then
    case "$1" in
      t) [ "$key" = t ] || [ "$key" = T ] ;;
      p) [ "$key" = p ] || [ "$key" = P ] ;;
      *) return 1 ;;
    esac
    return $?
  fi
  case "$1" in
    t) [ "$key" = '†' ] || [ "$key" = 'ˇ' ] ;;
    p) [ "$key" = 'π' ] || [ "$key" = '∏' ] ;;
    *) return 1 ;;
  esac
}

read_line() {
  stty "$saved_stty" <&3
  printf '%s[?25h' "$esc"
  input=
  IFS= read -r input <&3 || input=
  printf '%s[?25l' "$esc"
  stty -echo -icanon min 1 time 0 <&3
}

newline_with_sentinel=$(printf '\n_')
key_enter=${newline_with_sentinel%_}
key_escape=$esc
key_ctrl_b=$(printf '\002')
key_backspace=$(printf '\177')
key_ctrl_h=$(printf '\010')
# Navigation keys arrive as escape sequences; these stand in for them so a step
# can accept one by name. `key_nav` catches every other sequence, which no step
# accepts — it draws the "expected input" hint instead of tearing the tour down.
key_up='<up>'
key_down='<down>'
key_right='<right>'
key_left='<left>'
key_nav='<nav>'
# UTF-8 lead bytes of the macOS Option glyphs the tour has to recognise
# (`†` is E2 80 A0, `ˇ` is CB 87).
utf8_lead_three=$(printf '\342')
utf8_lead_two=$(printf '\313')
key_alt=0

step=1
error_hint=0
tree_open=0
windows=1
active_window=0
panes=1
selected_pane=0
zoomed=0
copy_open=0
step5_sub=0
step7_sub=0
step8_sub=0
step11_sub=0
watch_selection=0
sort_state=0
preview_open=0
help_open=0
step16_sub=0
step17_sub=0
step18_sub=0
blocked_attempts=0
inside_tmux=0
[ -n "${TMUX:-}" ] && inside_tmux=1

set_step_copy() {
  body1=
  body2=
  body3=
  expected=

  if [ "$language" = ko ]; then
    case "$step" in
      1) title='환영합니다'; body1='연습용 session에서 tmux와 Muxa를 안전하게 체험합니다.'; body2='가상 shell에 실제 명령을 입력해 session을 시작하세요.'; expected='입력: tmux new-session -s muxa-onboarding' ;;
      2) title='tmux prefix'; body1='모든 tmux 명령은 prefix를 누르고 뗀 뒤 suffix를 누릅니다.'; body2='이 simulation은 실제 tmux binding을 실행하지 않습니다.'; if [ "$inside_tmux" -eq 1 ]; then expected='입력: p (안전한 prefix simulation)'; else expected='입력: Ctrl-b'; fi ;;
      3) title='session과 window'; body1='session은 workspace, window는 독립된 work 화면입니다.'; body2='window tree에서 현재 session 구조를 확인합니다.'; expected='입력: w' ;;
      4) title='새 window'; body1='하나의 ticket은 하나의 안정적인 work window가 됩니다.'; body2='새 review window를 만들어 화면 변화를 확인하세요.'; expected='입력: c' ;;
      5) title='pane 분할'; body1='pane 하나는 agent 하나에 대응합니다.'; if [ "$step5_sub" -eq 0 ]; then body2='현재 window를 좌우 두 영역으로 나눕니다.'; expected='입력: %'; else body2='같은 work window에 reviewer 영역을 상하로 더합니다.'; expected='입력: "'; fi ;;
      6) title='pane 이동'; body1='선택 pane은 밝은 cyan border와 *로 표시됩니다.'; body2='오른쪽 codex agent pane으로 이동하세요.'; expected='입력: →' ;;
      7) title='pane zoom'; body1='zoom은 선택 pane에 집중하면서 layout을 보존합니다.'; if [ "$step7_sub" -eq 0 ]; then body2='codex pane을 전체 화면으로 확대하세요.'; expected='입력: z'; else body2='원래 세 pane layout으로 돌아가세요.'; expected='다시 입력: z'; fi ;;
      8) title='copy mode'; body1='copy mode에서는 terminal 출력을 스크롤하고 검색합니다.'; if [ "$step8_sub" -eq 0 ]; then body2='가상 copy mode를 여세요.'; expected='입력: ['; else body2='copy mode popup을 닫으세요.'; expected='입력: q'; fi ;;
      9) title='detach'; body1='detach는 client만 분리하며 session과 작업은 계속 실행됩니다.'; body2='가상 client를 session에서 분리하세요.'; expected='입력: d' ;;
      10) title='다시 attach'; body1='session은 그대로 실행 중입니다.'; body2='가상 shell에서 다시 연결하세요.'; expected='입력: tmux attach -t muxa-onboarding' ;;
      11) title='Muxa managed binding'; if [ "$step11_sub" -eq 0 ]; then body1='Muxa binding은 tmux session 위에서 관측 화면을 엽니다.'; expected='입력: s'; elif [ "$step11_sub" -eq 1 ]; then body1='watch는 같은 client를 유지한 채 전체 agent fleet를 보여줍니다.'; expected='입력: q'; else body1='pane별 상태 overlay에서도 필요한 agent가 바로 보입니다.'; expected='입력: s'; fi ;;
      12) title='work 선택'; body1='workspace/session 아래의 work/window를 한 줄로 관측합니다.'; body2='sandbox에서 onboarding work로 이동하세요.'; expected='입력: j' ;;
      13) title='agent 선택'; body1='work를 펼치면 같은 window의 agent pane들이 나타납니다.'; body2='codex child agent로 들어가세요.'; expected='입력: l' ;;
      14) title='attention 정렬'; body1='waiting-input, choice, error를 먼저 보도록 정렬할 수 있습니다.'; body2='Alt-T를 눌러 state와 attention이 필요한 순서로 정렬하세요.'; expected='입력: Alt-T' ;;
      15) title='pane preview'; body1='현재 화면을 떠나지 않고 agent terminal을 확인합니다.'; body2='선택한 pane의 preview를 여세요.'; expected='입력: o' ;;
      16) title='shortcut 도움말'; body1='실제 watch의 관측·협업·lifecycle key를 한곳에서 봅니다.'; case "$step16_sub" in 0) body2='먼저 preview를 닫습니다.'; expected='입력: o';; 1) body2='전체 도움말을 엽니다.'; expected='입력: ?';; *) body2='도움말을 닫고 계속합니다.'; expected='다시 입력: ?';; esac ;;
      17) title='new work form'; body1='n은 workspace, work, cwd, agent를 받는 form을 엽니다.'; if [ "$step17_sub" -eq 0 ]; then expected='입력: n'; else body2='실제 생성 없이 안전하게 form을 닫습니다.'; expected='입력: Esc'; fi ;;
      18) title='협업과 mailbox'; body1='같은 work의 agent에게 메시지를 보내고 reply를 추적합니다.'; case "$step18_sub" in 0) expected='입력: m';; 1) expected='빈 composer에서 Backspace';; 2) expected='입력: M';; *) expected='다시 입력: M';; esac ;;
      19) title='agent-side Muxa MCP'; body1='agent도 같은 work 경계 안에서 peer를 조회하고 기다립니다.'; body2='범용 shell 대신 좁은 Muxa operation을 사용합니다.'; expected='입력: l' ;;
      20) title='온보딩 완료'; body1='관측, attention, 협업과 명시적 lifecycle을 모두 확인했습니다.'; body2='지금까지 Muxa binary나 실제 tmux session은 사용하지 않았습니다.'; body3='화면을 닫으면 다운로드와 설치 방법을 안내합니다.'; expected='입력: q' ;;
    esac
  else
    case "$step" in
      1) title='Welcome'; body1='Practice tmux and Muxa safely inside an inert session.'; body2='Type the real command into the virtual shell to begin.'; expected='Type: tmux new-session -s muxa-onboarding' ;;
      2) title='tmux prefix'; body1='Every tmux command is prefix, release, then suffix.'; body2='This simulation never invokes a real tmux binding.'; if [ "$inside_tmux" -eq 1 ]; then expected='Press p (safe prefix simulation)'; else expected='Press Ctrl-b'; fi ;;
      3) title='Sessions and windows'; body1='A session is a workspace; a window is an independent work screen.'; body2='Open the tree to inspect the current session.'; expected='Press w' ;;
      4) title='Create a window'; body1='One ticket becomes one durable work window.'; body2='Create a review window and watch the status line change.'; expected='Press c' ;;
      5) title='Split into panes'; body1='One pane maps to one agent.'; if [ "$step5_sub" -eq 0 ]; then body2='Split the current window into two terminal regions.'; expected='Press %'; else body2='Add a reviewer region below the left one.'; expected='Press "'; fi ;;
      6) title='Move between panes'; body1='The selected pane has a bright cyan border and *.'; body2='Move to the codex agent pane on the right.'; expected='Press →' ;;
      7) title='Pane zoom'; body1='Zoom focuses one pane while preserving the layout.'; if [ "$step7_sub" -eq 0 ]; then body2='Expand the codex pane to the whole client.'; expected='Press z'; else body2='Return to the original three-pane layout.'; expected='Press z again'; fi ;;
      8) title='Copy mode'; body1='Copy mode scrolls and searches terminal output.'; if [ "$step8_sub" -eq 0 ]; then body2='Open the virtual copy mode.'; expected='Press ['; else body2='Close the copy-mode popup.'; expected='Press q'; fi ;;
      9) title='Detach'; body1='Detach removes only the client; the session keeps running.'; body2='Detach the virtual client now.'; expected='Press d' ;;
      10) title='Reattach'; body1='The session is still running.'; body2='Reconnect from the virtual shell.'; expected='Type: tmux attach -t muxa-onboarding' ;;
      11) title='Muxa managed binding'; if [ "$step11_sub" -eq 0 ]; then body1='A managed key opens observability over the tmux session.'; expected='Press s'; elif [ "$step11_sub" -eq 1 ]; then body1='Watch shows the whole agent fleet without leaving the client.'; expected='Press q'; else body1='Pane overlays make the agent needing attention visible.'; expected='Press s'; fi ;;
      12) title='Select work'; body1='Observe work/windows beneath each workspace/session.'; body2='Move from sandbox to the onboarding work.'; expected='Press j' ;;
      13) title='Select an agent'; body1='Expand work to see agent panes in that window.'; body2='Enter the codex child agent.'; expected='Press l' ;;
      14) title='Sort by attention'; body1='Put waiting-input, choice, and error ahead of passive work.'; body2='Press Alt-T to sort the watch by state and attention.'; expected='Press Alt-T' ;;
      15) title='Pane preview'; body1='Inspect an agent terminal without leaving the watch screen.'; body2='Open the preview for the selected pane.'; expected='Press o' ;;
      16) title='Shortcut help'; body1='See observation, collaboration, and lifecycle keys together.'; case "$step16_sub" in 0) body2='Close the preview first.'; expected='Press o';; 1) body2='Open the full help.'; expected='Press ?';; *) body2='Close help and continue.'; expected='Press ? again';; esac ;;
      17) title='New-work form'; body1='n opens fields for workspace, work, cwd, and agent.'; if [ "$step17_sub" -eq 0 ]; then expected='Press n'; else body2='Close it without creating anything.'; expected='Press Esc'; fi ;;
      18) title='Collaboration and mailbox'; body1='Message peers in one work and track their replies.'; case "$step18_sub" in 0) expected='Press m';; 1) expected='Press Backspace in the empty composer';; 2) expected='Press M';; *) expected='Press M again';; esac ;;
      19) title='Agent-side Muxa MCP'; body1='Agents inspect and wait for peers inside the same work boundary.'; body2='Narrow Muxa operations replace arbitrary shell control.'; expected='Press l' ;;
      20) title='Onboarding complete'; body1='You saw observability, attention, collaboration, and lifecycle.'; body2='No Muxa binary or real tmux session was used.'; body3='Close the tour to see download and install options.'; expected='Press q' ;;
    esac
  fi

  # Alt-T is the one gate with no Alt-free equivalent, and a terminal that
  # composes Option instead of sending Meta cannot produce it. Offer the
  # terminal fix and a way past only after the learner has actually tried.
  if [ "$step" -eq 14 ] && [ "$blocked_attempts" -ge 2 ]; then
    if [ "$language" = ko ]; then
      body3='Alt이 안 눌리나요? docs/WATCH.md의 터미널 설정을 보거나, →로 넘어가세요.'
    else
      body3='Alt not arriving? See docs/WATCH.md for the terminal setting, or press → to skip.'
    fi
  fi

  if [ "$no_quiz" -eq 1 ]; then
    if [ "$language" = ko ]; then expected='Enter: 다음 단계'; else expected='Enter: next step'; fi
  fi
}

render_callout() {
  set_step_copy
  callout_width=74
  [ "$cols" -ge 82 ] || callout_width=$((cols - 4))
  [ "$callout_width" -ge 60 ] || callout_width=60
  callout_height=9
  callout_left=$((cols - callout_width - 2))
  [ "$callout_left" -ge 2 ] || callout_left=2
  callout_top=$((rows - callout_height - 2))
  [ "$callout_top" -ge 7 ] || callout_top=7

  draw_box "$callout_top" "$callout_left" "$callout_width" "$callout_height" "$step/20 · $title" "$cyan"
  put $((callout_top + 2)) $((callout_left + 3)) "$normal" "$body1"
  [ -n "$body2" ] && put $((callout_top + 3)) $((callout_left + 3)) "$normal" "$body2"
  [ -n "$body3" ] && put $((callout_top + 4)) $((callout_left + 3)) "$normal" "$body3"
  put $((callout_top + callout_height - 2)) $((callout_left + 3)) "$yellow" "$expected"
  if [ "$error_hint" -eq 1 ]; then
    if [ "$language" = ko ]; then error_text='다른 입력입니다 — 노란색 안내를 따라주세요.'; else error_text='Different input — follow the yellow instruction.'; fi
    put $((callout_top + callout_height - 3)) $((callout_left + 3)) "$red" "$error_text"
  fi
}

render_shell() {
  clear_screen
  draw_box 1 1 "$cols" "$rows" 'shell · outside tmux' "$border"
  if [ "$step" -eq 10 ]; then
    put 3 3 "$dim" 'june@devbox:~/personal/muxa$'
    put 4 3 "$yellow" '[detached (from session muxa-onboarding)]'
    put 6 3 "$green" 'june@devbox:~/personal/muxa$ '
  else
    if [ "$language" = ko ]; then shell_label='Muxa 온보딩 · 안전한 가상 shell'; else shell_label='Muxa onboarding · safe virtual shell'; fi
    put 3 3 "$cyan" "$shell_label"
    put 5 3 "$green" 'june@devbox:~/personal/muxa$ '
  fi
  render_callout
}

render_pane() {
  pane_index=$1
  pane_top=$2
  pane_left=$3
  pane_width=$4
  pane_height=$5
  pane_is_selected=$6

  pane_color=$border
  pane_star=
  if [ "$pane_is_selected" -eq 1 ]; then pane_color=$cyan; pane_star=' *'; fi

  case "$pane_index" in
    0)
      if [ "$active_window" -eq 0 ]; then pane_title="shell$pane_star"; line1='june@devbox ~/personal/muxa'; line3='$ tmux display-message -p #S:#I.#P'; line4='muxa-onboarding:0.0'
      else pane_title="review · shell$pane_star"; line1='june@devbox ~/personal/muxa'; line3='$'; line4='review window · agent layout'
      fi
      ;;
    1) pane_title="codex · agent$pane_star"; line1='› implement muxa-onboarding'; line3='● working'; line4='editing onboarding shell tour' ;;
    *) pane_title="reviewer · agent$pane_star"; line1='› review the current changes'; line3='▶ waiting for input'; line4='findings: 0' ;;
  esac

  draw_box "$pane_top" "$pane_left" "$pane_width" "$pane_height" "$pane_title" "$pane_color"
  if [ "$pane_height" -ge 7 ] && [ "$pane_width" -ge 32 ]; then
    put $((pane_top + 2)) $((pane_left + 2)) "$normal" "$line1"
    put $((pane_top + 4)) $((pane_left + 2)) "$yellow" "$line3"
    put $((pane_top + 5)) $((pane_left + 2)) "$dim" "$line4"
  fi
}

render_status_line() {
  move_to "$rows" 1
  printf '%s' "$(printf '\033[1;30;48;5;48m')"
  repeat ' ' "$cols"
  move_to "$rows" 1
  if [ "$windows" -eq 1 ]; then status_left=' [muxa-onboarding] 0:shell*'; else status_left=' [muxa-onboarding] 0:shell  1:review*'; fi
  printf '%s' "$status_left"
  status_right="prefix Ctrl-b · $panes panes "
  right_col=$((cols - ${#status_right} + 1))
  [ "$right_col" -gt 1 ] && put "$rows" "$right_col" "$(printf '\033[1;37;48;5;238m')" "$status_right"
  printf '%s' "$normal"
}

render_tree() {
  tree_width=58
  [ "$cols" -ge 64 ] || tree_width=$((cols - 6))
  draw_box 3 4 "$tree_width" 10 'choose-tree -Zw' "$yellow"
  put 5 7 "$normal" 'muxa-onboarding: 1 windows'
  put 6 7 "$normal" '└─ 0: shell* (1 panes)'
  put 7 7 "$dim" '   └─ 0: zsh  june@devbox:~/personal/muxa'
  if [ "$language" = ko ]; then tree_hint='w로 연 session/window tree · c로 새 window'; else tree_hint='session/window tree opened by w · c creates a window'; fi
  put 10 7 "$yellow" "$tree_hint"
}

render_copy_mode() {
  popup_width=66
  [ "$cols" -ge 72 ] || popup_width=$((cols - 6))
  popup_left=$(((cols - popup_width) / 2 + 1))
  draw_box 4 "$popup_left" "$popup_width" 10 'copy mode · [0/120]' "$yellow"
  put 6 $((popup_left + 3)) "$normal" '$ cargo test -p muxa-cli onboarding'
  put 7 $((popup_left + 3)) "$normal" 'running 20 steps'
  put 8 $((popup_left + 3)) "$green" 'test result: ok. 20 passed'
  if [ "$language" = ko ]; then copy_hint='↑/↓ 스크롤 · / 검색 · q 종료'; else copy_hint='↑/↓ scroll · / search · q exit'; fi
  put 11 $((popup_left + 3)) "$yellow" "$copy_hint"
}

render_peek() {
  peek_width=34
  peek_left=3
  draw_box 4 "$peek_left" "$peek_width" 8 '1 · shell · ○ IDLE' "$yellow"
  put 6 $((peek_left + 2)) "$dim" 'no recent prompt'
  if [ "$cols" -ge 76 ]; then
    peek_left=$((cols - peek_width - 2))
    draw_box 4 "$peek_left" "$peek_width" 8 '2 · codex · ● WORKING' "$yellow"
    put 6 $((peek_left + 2)) "$normal" 'editing onboarding tour'
    put 8 $((peek_left + 2)) "$dim" 'last prompted: just now'
  fi
}

render_tmux() {
  clear_screen
  body_height=$((rows - 1))
  if [ "$zoomed" -eq 1 ]; then
    render_pane "$selected_pane" 1 1 "$cols" "$body_height" 1
  elif [ "$panes" -eq 1 ]; then
    render_pane 0 1 1 "$cols" "$body_height" 1
  elif [ "$panes" -eq 2 ]; then
    left_width=$((cols * 55 / 100))
    render_pane 0 1 1 "$left_width" "$body_height" 1
    render_pane 1 1 $((left_width + 1)) $((cols - left_width)) "$body_height" 0
  else
    left_width=$((cols * 55 / 100))
    upper_height=$((body_height * 55 / 100))
    if [ "$selected_pane" -eq 0 ]; then select0=1; else select0=0; fi
    if [ "$selected_pane" -eq 1 ]; then select1=1; else select1=0; fi
    if [ "$selected_pane" -eq 2 ]; then select2=1; else select2=0; fi
    render_pane 0 1 1 "$left_width" "$upper_height" "$select0"
    render_pane 1 1 $((left_width + 1)) $((cols - left_width)) "$body_height" "$select1"
    render_pane 2 $((upper_height + 1)) 1 "$left_width" $((body_height - upper_height)) "$select2"
  fi
  render_status_line
  [ "$tree_open" -eq 1 ] && render_tree
  [ "$copy_open" -eq 1 ] && render_copy_mode
  [ "$step" -eq 11 ] && [ "$step11_sub" -eq 2 ] && render_peek
  render_callout
}

render_watch_header() {
  move_to 1 1
  printf '%s muxa watch %s%s  2 works   %s▶ 1  %s● 1  %s○ 1%s   mail 0/1   sort %s' \
    "$(printf '\033[1;30;48;5;51m')" "$normal" "$bold" "$yellow" "$yellow" "$green" "$normal" "$(if [ "$sort_state" -eq 1 ]; then printf ST; else printf LATEST; fi)"
  put 2 1 "$dim" 'j/k move  ·  type or / filter  ·  : commands  ·  ? help'
  put 3 1 "$border" "$(repeat '─' "$cols")"
}

render_watch_rows() {
  works_width=$1
  draw_box 4 1 "$works_width" $((rows - 4)) 'Workspace › work › agent' "$border"
  put 6 3 "$dim" '  WORKSPACE › WORK       DUR   ACT   SUMMARY'

  if [ "$sort_state" -eq 0 ]; then first=sandbox; second=onboarding; else first=onboarding; second=sandbox; fi
  row=8
  for work in "$first" "$second"; do
    if [ "$work" = sandbox ]; then
      if [ "$watch_selection" -eq 0 ]; then row_style=$selected; marker='>'; else row_style=$normal; marker=' '; fi
      put "$row" 3 "$row_style" "$marker ○   muxa › sandbox       7m    2m   release checks complete"
      row=$((row + 2))
    else
      if [ "$watch_selection" -eq 1 ]; then row_style=$selected; marker='>'; else row_style=$normal; marker=' '; fi
      put "$row" 3 "$row_style" "$marker ▶ ● muxa › onboarding  12m    8s   harden checkout auth"
      row=$((row + 1))
      if [ "$watch_selection" -ge 1 ]; then
        if [ "$watch_selection" -eq 2 ]; then child_style=$selected; child_marker='>'; else child_style=$dim; child_marker=' '; fi
        put "$row" 5 "$child_style" "$child_marker └─ muxa-onboarding:0.0   codex · implementing"
        row=$((row + 1))
        put "$row" 5 "$dim" '  └─ muxa-onboarding:1.0   reviewer · waiting input'
        row=$((row + 1))
      fi
      row=$((row + 1))
    fi
  done
}

render_inspector() {
  inspector_left=$1
  inspector_width=$2
  draw_box 4 "$inspector_left" "$inspector_width" $((rows - 4)) 'Inspector · muxa-onboarding:0.0 · WORK' "$border"
  col=$((inspector_left + 2))
  put 6 "$col" "$dim" 'kind codex   model —   pane %42'
  put 8 "$col" "$cyan" '● codex'
  put 10 "$col" "$normal" '› implement checkout hardening'
  put 12 "$col" "$dim" '⚙ editing crates/muxa-cli/src/onboarding.rs'
  put 14 "$col" "$yellow" '● working…'
  if [ "$step" -eq 19 ]; then
    put 17 "$col" "$cyan" 'muxa_wait_for_change(pane="%42", until="settled")'
    put 18 "$col" "$cyan" 'muxa_status(pane="%42", include_capture=true)'
    put 20 "$col" "$dim" 'No arbitrary shell or generic tmux MCP.'
  fi
}

render_watch_footer() {
  move_to "$rows" 1
  printf '%s' "$(printf '\033[1;37;48;5;236m')"
  repeat ' ' "$cols"
  move_to "$rows" 1
  printf ' j/k move   h/l tree   o preview   n new   m message   M mailbox   ? help   q quit%s' "$normal"
}

render_preview_overlay() {
  overlay_width=70
  [ "$cols" -ge 76 ] || overlay_width=$((cols - 6))
  overlay_left=$(((cols - overlay_width) / 2 + 1))
  draw_box 5 "$overlay_left" "$overlay_width" 13 'Pane preview · codex · %42' "$yellow"
  put 7 $((overlay_left + 3)) "$cyan" '╭─ OpenAI Codex ─────────────────────────────────────────╮'
  put 8 $((overlay_left + 3)) "$normal" '│ › implement checkout hardening                         │'
  put 9 $((overlay_left + 3)) "$normal" '│                                                        │'
  put 10 $((overlay_left + 3)) "$yellow" '│ ● Running cargo test --workspace                       │'
  put 11 $((overlay_left + 3)) "$green" '│ ✓ 606 passed · 1 ignored                               │'
  put 12 $((overlay_left + 3)) "$normal" '│                                                        │'
  put 13 $((overlay_left + 3)) "$dim" '│ Editing crates/muxa-cli/src/onboarding.rs               │'
  put 14 $((overlay_left + 3)) "$cyan" '╰────────────────────────────────────────────────────────╯'
}

render_help_overlay() {
  overlay_width=78
  [ "$cols" -ge 84 ] || overlay_width=$((cols - 6))
  overlay_left=$(((cols - overlay_width) / 2 + 1))
  draw_box 4 "$overlay_left" "$overlay_width" 17 'Muxa watch shortcuts' "$cyan"
  col=$((overlay_left + 3))
  put 6 "$col" "$yellow" 'j/k  move       h/l  work tree      Alt-T  state sort'
  put 8 "$col" "$yellow" 'o    preview    |    inspector      Enter  prompt'
  put 10 "$col" "$yellow" 'n    new work   m    message        M      mailbox'
  put 12 "$col" "$yellow" 'a    ask agent  A    answer history ?      help'
  put 14 "$col" "$yellow" 'x    interrupt  q    close/quit     Esc    cancel'
  if [ "$language" = ko ]; then help_line='모든 destructive lifecycle 동작은 명시적 확인을 요구합니다.'; else help_line='Destructive lifecycle actions always require explicit confirmation.'; fi
  put 17 "$col" "$dim" "$help_line"
}

render_form_overlay() {
  overlay_width=76
  [ "$cols" -ge 82 ] || overlay_width=$((cols - 6))
  overlay_left=$(((cols - overlay_width) / 2 + 1))
  overlay_top=$((rows - 15))
  [ "$overlay_top" -ge 5 ] || overlay_top=5
  draw_box "$overlay_top" "$overlay_left" "$overlay_width" 12 'New work + first agent' "$cyan"
  col=$((overlay_left + 3))
  put $((overlay_top + 2)) "$col" "$normal" 'workspace  muxa'
  put $((overlay_top + 3)) "$col" "$normal" 'work       muxa-onboarding'
  put $((overlay_top + 4)) "$col" "$normal" 'cwd        ~/personal/muxa'
  put $((overlay_top + 5)) "$col" "$normal" 'agent      codex'
  put $((overlay_top + 6)) "$col" "$normal" 'role       implementer'
  put $((overlay_top + 8)) "$col" "$yellow" '[ Enter create ]     [ Esc cancel ]'
}

render_message_overlay() {
  overlay_width=76
  [ "$cols" -ge 82 ] || overlay_width=$((cols - 6))
  overlay_left=$(((cols - overlay_width) / 2 + 1))
  overlay_top=$((rows - 12))
  draw_box "$overlay_top" "$overlay_left" "$overlay_width" 9 'Message · reviewer · request' "$cyan"
  put $((overlay_top + 2)) $((overlay_left + 3)) "$dim" 'To: muxa-onboarding:1.0 · reviewer'
  put $((overlay_top + 4)) $((overlay_left + 3)) "$normal" '│ '
  put $((overlay_top + 6)) $((overlay_left + 3)) "$yellow" 'Enter send · Backspace on empty closes · Esc cancel'
}

render_mailbox_overlay() {
  overlay_width=84
  [ "$cols" -ge 90 ] || overlay_width=$((cols - 6))
  overlay_left=$(((cols - overlay_width) / 2 + 1))
  draw_box 5 "$overlay_left" "$overlay_width" 14 'Collaboration mailbox · 1 request' "$yellow"
  col=$((overlay_left + 3))
  put 7 "$col" "$dim" 'STATE      FROM                    KIND       AGE'
  put 9 "$col" "$selected" '> pending    muxa-onboarding:1.0   review     2m'
  put 11 "$col" "$normal" 'Review the public-read authentication boundary.'
  put 13 "$col" "$cyan" 'artifact: crates/muxa/src/dashboard/server.rs'
  put 16 "$col" "$yellow" 'j/k select · Enter open · M close'
}

render_watch() {
  clear_screen
  render_watch_header
  if [ "$cols" -ge 120 ]; then
    works_width=$((cols / 2))
    render_watch_rows "$works_width"
    render_inspector $((works_width + 1)) $((cols - works_width))
  else
    render_watch_rows "$cols"
  fi
  render_watch_footer

  [ "$preview_open" -eq 1 ] && render_preview_overlay
  [ "$help_open" -eq 1 ] && render_help_overlay
  [ "$step" -eq 17 ] && [ "$step17_sub" -eq 1 ] && render_form_overlay
  [ "$step" -eq 18 ] && [ "$step18_sub" -eq 1 ] && render_message_overlay
  [ "$step" -eq 18 ] && [ "$step18_sub" -eq 3 ] && render_mailbox_overlay
  render_callout
}

render_small_terminal() {
  clear_screen
  small_width=$((cols - 4))
  [ "$small_width" -ge 30 ] || small_width=30
  small_height=9
  small_left=3
  small_top=$(((rows - small_height) / 2 + 1))
  [ "$small_top" -ge 1 ] || small_top=1
  draw_box "$small_top" "$small_left" "$small_width" "$small_height" 'Muxa onboarding' "$cyan"
  if [ "$language" = ko ]; then small1='전체 화면 simulation을 표시할 공간이 부족합니다.'; small2='터미널을 최소 68 × 20으로 키운 뒤 Enter를 누르세요.'; else small1='The fullscreen simulation needs a little more room.'; small2='Resize to at least 68 × 20, then press Enter.'; fi
  put $((small_top + 3)) $((small_left + 3)) "$normal" "$small1"
  put $((small_top + 5)) $((small_left + 3)) "$yellow" "$small2"
}

render_current() {
  refresh_size
  if [ "$cols" -lt 68 ] || [ "$rows" -lt 20 ]; then
    render_small_terminal
    return
  fi
  if [ "$step" -eq 1 ] || [ "$step" -eq 10 ]; then
    render_shell
  elif [ "$step" -le 11 ]; then
    if [ "$step" -eq 11 ] && [ "$step11_sub" -eq 1 ]; then render_watch; else render_tmux; fi
  else
    render_watch
  fi
}

skip_step() {
  if [ "$step" -eq 20 ]; then
    finished=1
  fi
  case "$step" in
    1) : ;;
    3) tree_open=1 ;;
    4) tree_open=0; windows=2; active_window=1 ;;
    5) panes=3 ;;
    6) selected_pane=1 ;;
    7) zoomed=0 ;;
    8) copy_open=0 ;;
    9) : ;;
    10) : ;;
    11) watch_selection=0 ;;
    12) watch_selection=1 ;;
    13) watch_selection=2 ;;
    14) sort_state=1 ;;
    15) preview_open=1 ;;
    16) preview_open=0; help_open=0; step16_sub=0 ;;
    17) step17_sub=0 ;;
    18) step18_sub=0 ;;
  esac
  step=$((step + 1))
  error_hint=0
}

accept_key() {
  accepted=0
  case "$step" in
    2)
      if { [ "$inside_tmux" -eq 1 ] && [ "$key" = p ]; } || { [ "$inside_tmux" -eq 0 ] && [ "$key" = "$key_ctrl_b" ]; }; then accepted=1; step=3; fi
      ;;
    3) [ "$key" = w ] && { accepted=1; tree_open=1; step=4; } ;;
    4) [ "$key" = c ] && { accepted=1; tree_open=0; windows=2; active_window=1; step=5; } ;;
    5)
      if [ "$step5_sub" -eq 0 ] && [ "$key" = '%' ]; then accepted=1; panes=2; step5_sub=1
      elif [ "$step5_sub" -eq 1 ] && [ "$key" = '"' ]; then accepted=1; panes=3; step=6
      fi
      ;;
    6) [ "$key" = "$key_right" ] && { accepted=1; selected_pane=1; step=7; } ;;
    7)
      if [ "$key" = z ]; then accepted=1; if [ "$step7_sub" -eq 0 ]; then zoomed=1; step7_sub=1; else zoomed=0; step=8; fi; fi
      ;;
    8)
      if [ "$step8_sub" -eq 0 ] && [ "$key" = '[' ]; then accepted=1; copy_open=1; step8_sub=1
      elif [ "$step8_sub" -eq 1 ] && [ "$key" = q ]; then accepted=1; copy_open=0; step=9
      fi
      ;;
    9) [ "$key" = d ] && { accepted=1; step=10; } ;;
    11)
      if [ "$step11_sub" -eq 0 ] && [ "$key" = s ]; then accepted=1; step11_sub=1
      elif [ "$step11_sub" -eq 1 ] && [ "$key" = q ]; then accepted=1; step11_sub=2
      elif [ "$step11_sub" -eq 2 ] && [ "$key" = s ]; then accepted=1; step=12
      fi
      ;;
    12) [ "$key" = j ] && { accepted=1; watch_selection=1; step=13; } ;;
    13) [ "$key" = l ] && { accepted=1; watch_selection=2; step=14; } ;;
    14)
      if key_is_alt_chord t; then accepted=1; sort_state=1; step=15
      elif [ "$blocked_attempts" -ge 2 ] && { [ "$key" = "$key_right" ] || [ "$key" = "$key_enter" ]; }; then
        accepted=1; sort_state=1; step=15
      fi
      ;;
    15) [ "$key" = o ] && { accepted=1; preview_open=1; step=16; } ;;
    16)
      if [ "$step16_sub" -eq 0 ] && [ "$key" = o ]; then accepted=1; preview_open=0; step16_sub=1
      elif [ "$step16_sub" -eq 1 ] && [ "$key" = '?' ]; then accepted=1; help_open=1; step16_sub=2
      elif [ "$step16_sub" -eq 2 ] && [ "$key" = '?' ]; then accepted=1; help_open=0; step16_sub=0; step=17
      fi
      ;;
    17)
      if [ "$step17_sub" -eq 0 ] && [ "$key" = n ]; then accepted=1; step17_sub=1
      elif [ "$step17_sub" -eq 1 ] && [ "$key" = "$key_escape" ]; then accepted=1; step17_sub=0; step=18
      fi
      ;;
    18)
      if [ "$step18_sub" -eq 0 ] && [ "$key" = m ]; then accepted=1; step18_sub=1
      elif [ "$step18_sub" -eq 1 ] && { [ "$key" = "$key_backspace" ] || [ "$key" = "$key_ctrl_h" ]; }; then accepted=1; step18_sub=2
      elif [ "$step18_sub" -eq 2 ] && [ "$key" = M ]; then accepted=1; step18_sub=3
      elif [ "$step18_sub" -eq 3 ] && [ "$key" = M ]; then accepted=1; step18_sub=0; step=19
      fi
      ;;
    19) [ "$key" = l ] && { accepted=1; step=20; } ;;
    20) [ "$key" = q ] && { accepted=1; finished=1; exit 0; } ;;
  esac

  if [ "$accepted" -eq 1 ]; then
    error_hint=0
    blocked_attempts=0
  else
    error_hint=1
    blocked_attempts=$((blocked_attempts + 1))
  fi
}

while [ "$step" -le 20 ]; do
  render_current

  if [ "$cols" -lt 68 ] || [ "$rows" -lt 20 ]; then
    read_key
    [ "$key" = q ] || [ "$key" = "$key_escape" ] && exit 0
    continue
  fi

  if [ "$no_quiz" -eq 1 ]; then
    read_key
    if [ "$key" = q ] || [ "$key" = "$key_escape" ]; then
      exit 0
    elif [ "$key" = "$key_enter" ]; then
      skip_step
    else
      error_hint=1
    fi
    continue
  fi

  if [ "$step" -eq 1 ] || [ "$step" -eq 10 ]; then
    if [ "$step" -eq 1 ]; then prompt_row=5; else prompt_row=6; fi
    move_to "$prompt_row" 32
    read_line
    if [ "$input" = q ] || [ "$input" = quit ] || [ "$input" = exit ]; then
      exit 0
    elif [ "$step" -eq 1 ] && { [ "$input" = 'tmux new-session -s muxa-onboarding' ] || [ "$input" = 'tmux new -s muxa-onboarding' ]; }; then
      step=2
      error_hint=0
    elif [ "$step" -eq 10 ] && { [ "$input" = 'tmux attach -t muxa-onboarding' ] || [ "$input" = 'tmux attach-session -t muxa-onboarding' ]; }; then
      step=11
      error_hint=0
    else
      error_hint=1
    fi
    continue
  fi

  read_key
  if [ "$key" = "$key_escape" ] && ! { [ "$step" -eq 17 ] && [ "$step17_sub" -eq 1 ]; }; then
    exit 0
  fi
  if [ "$key" = q ] && ! { [ "$step" -eq 8 ] || { [ "$step" -eq 11 ] && [ "$step11_sub" -eq 1 ]; } || [ "$step" -eq 20 ]; }; then
    exit 0
  fi
  accept_key
done
