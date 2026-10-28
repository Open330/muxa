# CLI dashboard

`muxa dashboard`는 `muxa watch`를 **Work board** 화면으로 엽니다. `Alt-3` /
`:screen board` / `muxa watch --screen board`와 같은 화면입니다. 웹 대시보드가 쓰는
canonical `WorkSnapshot`(저장된 Work, 외부 이슈, 모든 Run과 그 agent)을 읽습니다.
board 자체는 [WATCH.ko.md](WATCH.ko.md)를 보세요. 이 문서는 명령과, 이전 카드
콘솔을 대체하면서 바뀐 점을 다룹니다.

## 열기

```bash
muxa dashboard
muxa dashboard --since 7d        # 요약의 ACT/WACT 기간
muxa dashboard --sort activity   # attention(기본), activity, name
muxa dashboard --include-paneless
muxa dashboard --theme mono
```

`prefix + D`는 popup에서 `muxa dashboard`를 실행하며 `muxa init`이 설치합니다.
다른 watch 화면은 키 하나로 갑니다(`Alt-1` topology, `Alt-2` 협업).

플래그는 그 실행에만 적용됩니다. `--since`는 `muxa stats --since`와 같은 값을
받고(`today`, `24h`, `7d`, 로컬 날짜, RFC3339, `all`), 잘못된 값은 화면을 열기
전에 오류가 됩니다. board 안의 `:sort`는 순서를 바꾸고 저장합니다
(`[watch] board_sort`).

## 키

| 키 | 동작 |
| --- | --- |
| `j`/`k`, `g`/`G` | Work 이동 |
| `Tab` / `]`, `Shift-Tab` / `[` | Work 안 agent 커서(`▸`) 이동 |
| `Enter` / `o` | `▸` agent pane으로 이동 / 미리보기 |
| `p` / `P` | `▸` agent / Work의 모든 live agent에 프롬프트 |
| `R` / `A` | `▸` agent / 모든 live agent 중단(확인) |
| `K` | `▸` agent의 pane 닫기(확인) |
| `c` | `▸` agent의 마지막 프롬프트 복사 |
| `m` | `▸` agent에게 메시지(정확한 agent session) |
| `b` / `M` | 그 agent의 메일함. 안에서 `i` claim, `e` 답장, `x` 대기 중인 보낸 요청 철회 |
| `/` 또는 타이핑 | Work 필터 |
| `?` | 도움말 |

## 이전 dashboard와 달라진 점

- **`--sort active`는 거부됩니다.** 활동 시간으로 정렬했는데 그 시간을 tmux
  session 이름 기준으로 나눴기 때문에 한 session의 Work가 모두 같은 값을
  보였습니다. ACT/WACT를 Work별로 나눌 수 있을 때까지 board는 모든 세션 합계 하나를
  보여주고, `active`는 다른 뜻으로 바뀌는 대신 오류를 냅니다. 작업 중인 agent 수
  기준은 `:sort busy`로 쓸 수 있습니다.
- 같은 이유로 **카드별 ACT/WACT는 없습니다.** 모서리에 `--since` 기간의 모든 세션
  합계가 나옵니다.
- **muxa가 소유한 PTY session**은 Work가 아니어서 나오지 않습니다. `muxa watch`가
  보여줍니다.
- `muxa dashboard --legacy`는 한 릴리스 동안 이전 콘솔을 엽니다.
