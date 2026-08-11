#!/bin/sh
# Preview Muxa in any terminal without downloading or installing Muxa.
#
#   curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh
#
# Forward preview flags with `sh -s --`:
#
#   curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh -s -- --lang ko
#   curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh -s -- --print
#
# This file is the whole preview. It uses only POSIX shell builtins, renders an
# inert Muxa workflow, and never downloads a binary or touches tmux/config.

set -eu

language=auto
print_only=0

usage() {
  printf '%s\n' \
    'Usage: onboard.sh [--lang auto|en|ko] [--print] [--no-quiz]' \
    '' \
    'A dependency-free shell preview of the Muxa workflow.' \
    '' \
    'Options:' \
    '  --lang auto|en|ko  Display language (default: detect from locale)' \
    '  --print            Print every page without interactive prompts' \
    '  --no-quiz          Compatibility flag; the shell preview has no quiz' \
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
    --lang=*)
      language=${1#--lang=}
      shift
      ;;
    --print)
      print_only=1
      shift
      ;;
    --no-quiz)
      shift
      ;;
    --tmux)
      # Compatibility with the old full-tour flag; tmux is always represented.
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    --)
      shift
      break
      ;;
    *)
      fail "unknown option: $1"
      ;;
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

interactive=0
if [ "$print_only" -eq 0 ] && [ -r /dev/tty ] && [ -t 1 ]; then
  interactive=1
fi

if [ "$interactive" -eq 1 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-}" != dumb ]; then
  bold=$(printf '\033[1m')
  dim=$(printf '\033[2m')
  cyan=$(printf '\033[36m')
  green=$(printf '\033[32m')
  yellow=$(printf '\033[33m')
  red=$(printf '\033[31m')
  reset=$(printf '\033[0m')
else
  bold=
  dim=
  cyan=
  green=
  yellow=
  red=
  reset=
fi

clear_page() {
  if [ "$interactive" -eq 1 ]; then
    printf '\033[2J\033[H'
  fi
}

page_header() {
  clear_page
  if [ "$interactive" -eq 0 ] && [ "$1" -gt 1 ]; then
    printf '\n'
  fi
  printf '%sMuxa onboarding · %s/4%s\n' "$bold$cyan" "$1" "$reset"
  printf '%s%s%s\n\n' "$bold" "$2" "$reset"
}

continue_prompt() {
  [ "$interactive" -eq 1 ] || return 0

  if [ "$language" = ko ]; then
    printf '\n%sEnter%s 다음 · %sq + Enter%s 종료: ' "$yellow" "$reset" "$dim" "$reset"
  else
    printf '\n%sEnter%s next · %sq + Enter%s quit: ' "$yellow" "$reset" "$dim" "$reset"
  fi

  answer=
  IFS= read -r answer </dev/tty || exit 0
  case "$answer" in
    q | Q) clear_page; exit 0 ;;
  esac
}

page_one() {
  if [ "$language" = ko ]; then
    page_header 1 'AI coding agent를 작업 단위로 운영하세요'
    printf '%s\n' \
      'Muxa는 tmux를 단순한 pane 모음이 아닌 작업 실행 모델로 사용합니다.' \
      '' \
      '  workspace / project  =  tmux session' \
      '  work / ticket        =  tmux window' \
      '  agent                =  tmux pane' \
      ''
    printf '  %sMuxa%s  위치 · 상태 · attention · 협업 routing\n' "$green$bold" "$reset"
    printf '  %sAgent%s 코드 · Git · 테스트 · 추론\n' "$cyan$bold" "$reset"
  else
    page_header 1 'Run AI coding agents as durable units of work'
    printf '%s\n' \
      'Muxa turns tmux from a pile of panes into a work execution model.' \
      '' \
      '  workspace / project  =  tmux session' \
      '  work / ticket        =  tmux window' \
      '  agent                =  tmux pane' \
      ''
    printf '  %sMuxa%s  location · state · attention · collaboration routing\n' "$green$bold" "$reset"
    printf '  %sAgent%s code · Git · tests · reasoning\n' "$cyan$bold" "$reset"
  fi
  continue_prompt
}

page_two() {
  if [ "$language" = ko ]; then
    page_header 2 '모든 agent를 한 화면에서 관측하세요'
  else
    page_header 2 'See every agent on one screen'
  fi

  printf '%s┌─ WORKS ───────────────────────────────────────────────────────────────┐%s\n' "$dim" "$reset"
  printf '│ %s●%s platform › CAL-7187   work    02:14   implementing retry policy │\n' "$green" "$reset"
  printf '│ %s▶%s api › CAL-7177        input   00:43   choose migration strategy │\n' "$yellow" "$reset"
  printf '│ %s◆%s web › redesign        choice  00:18   approve the visual diff   │\n' "$yellow" "$reset"
  printf '│ %s■%s billing › stripe      error   00:07   integration test failed   │\n' "$red" "$reset"
  printf '│ ○ docs › onboarding     idle    05:31   README updated            │\n'
  printf '%s├─ INSPECTOR ───────────────────────────────────────────────────────────┤%s\n' "$dim" "$reset"
  printf '│ api › CAL-7177 › codex › %%42                                         │\n'
  if [ "$language" = ko ]; then
    printf '│ agent가 migration 방식을 선택해 달라고 기다리고 있습니다.            │\n'
  else
    printf '│ Waiting for you to choose a migration strategy.                      │\n'
  fi
  printf '%s└───────────────────────────────────────────────────────────────────────┘%s\n' "$dim" "$reset"
  printf '\n%sj/k%s move  %so%s preview  %s|%s inspector  %sEnter%s open\n' \
    "$yellow" "$reset" "$yellow" "$reset" "$yellow" "$reset" "$yellow" "$reset"
  continue_prompt
}

page_three() {
  if [ "$language" = ko ]; then
    page_header 3 '지금 사람의 판단이 필요한 곳으로 바로 이동하세요'
    printf '%s\n' \
      '  muxa attend' \
      '      ↓' \
      '  api › CAL-7177 › codex › waiting-input' \
      ''
    printf '  %sattention triage%s  오래 기다린 input · choice · error 우선\n' "$green$bold" "$reset"
    printf '  %scollaboration%s     같은 work의 agent에게 메시지와 review 요청\n' "$cyan$bold" "$reset"
    printf '  %sorchestration%s     work와 agent를 생성·관측·종료\n' "$yellow$bold" "$reset"
    printf '\n  n new work   m message   M mailbox   a ask   A answers\n'
  else
    page_header 3 'Jump straight to the agent that needs you'
    printf '%s\n' \
      '  muxa attend' \
      '      ↓' \
      '  api › CAL-7177 › codex › waiting-input' \
      ''
    printf '  %sattention triage%s  oldest input · choice · error first\n' "$green$bold" "$reset"
    printf '  %scollaboration%s     message and request review inside one work\n' "$cyan$bold" "$reset"
    printf '  %sorchestration%s     create, observe, and close work and agents\n' "$yellow$bold" "$reset"
    printf '\n  n new work   m message   M mailbox   a ask   A answers\n'
  fi
  continue_prompt
}

page_four() {
  if [ "$language" = ko ]; then
    page_header 4 '이 preview는 아무것도 다운로드하거나 변경하지 않았습니다'
    printf '%s\n' \
      '방금 본 화면은 이 shell script가 그린 안전한 dummy입니다.' \
      'Muxa binary, daemon, config, tmux session은 전혀 건드리지 않았습니다.' \
      ''
    printf '%s계속 사용해 보기%s\n\n' "$bold" "$reset"
    printf '  brew install open330/tap/muxa\n'
    printf '  muxa init\n'
    printf '  muxa onboard --lang ko\n'
    printf '\n%s설치된 muxa onboard에서는 전체 20단계 실습을 진행할 수 있습니다.%s\n' "$dim" "$reset"
  else
    page_header 4 'This preview downloaded and changed nothing'
    printf '%s\n' \
      'Everything you saw was an inert dummy drawn by this shell script.' \
      'No Muxa binary, daemon, config, or tmux session was touched.' \
      ''
    printf '%sKeep using Muxa%s\n\n' "$bold" "$reset"
    printf '  brew install open330/tap/muxa\n'
    printf '  muxa init\n'
    printf '  muxa onboard\n'
    printf '\n%sThe installed muxa onboard includes the complete 20-step hands-on tour.%s\n' "$dim" "$reset"
  fi
}

page_one
page_two
page_three
page_four

if [ "$interactive" -eq 1 ]; then
  printf '\n'
fi
