#!/usr/bin/env bash
# Stop hook — refuse to end a turn while a process this session backgrounded is
# still running.
#
# WHY. Under `claude -p` there is nothing to resume a session: it ends when the
# model stops. A command that exceeds the Bash tool's timeout is force-moved to
# the background by the harness, and the model has no working way to wait for it
# (completion notifications, Monitor and ScheduleWakeup are all interactive-only,
# and `sleep N; cat` is blocked). Sessions were repeatedly ending with "waiting
# for the background test to finish", losing the whole iteration — 4 of 37 ralph
# sessions, ~$36. Prompt wording alone did not stop it; this makes it structural.
#
# HOW. At Stop time no tool is executing except this hook, so any *other* live
# descendant of the claude process is by definition something the session
# backgrounded. Walk up to the claude process, walk down to its descendants,
# subtract this hook's own subtree, and block if anything is left.
#
# DEFENSIVE BY CONSTRUCTION — it must never be able to wedge a session:
#   * fails open on every error, missing tool, or unparseable input (exit 0)
#   * honours stop_hook_active, so it can never re-block its own continuation
#   * caps consecutive blocks per session (MAX_BLOCKS) and forgets the count
#     once a turn ends cleanly
#   * ignores processes older than MAX_AGE, so a leaked or wedged child cannot
#     hold a session hostage forever
#   * never kills anything and never touches the task itself — it only reports
set -uo pipefail

# Small on purpose. One block is enough to make the model wait instead of
# abandoning the work; repeated blocking adds little and risks dragging a
# session out. A 45s background task was observed blocking twice, so the cap
# bounds the worst case rather than relying on the model converging.
MAX_BLOCKS="${RALPH_STOP_HOOK_MAX_BLOCKS:-2}"    # consecutive blocks before giving up
MAX_AGE="${RALPH_STOP_HOOK_MAX_AGE:-3600}"       # seconds; older children are ignored

allow() { exit 0; }                               # the fail-open path
trap allow ERR

command -v ps >/dev/null 2>&1 || allow

# Bounded read. A bare `cat` blocks until EOF, and if the harness holds stdin
# open the hook never returns — which hangs the very session it exists to
# protect. Measured: an unclosed stdin wedged a headless session past 240s.
payload="$(timeout 2 cat 2>/dev/null || true)"

field() { # <json-key> — empty string on any parse failure
  printf '%s' "$payload" | python3 -c "
import json,sys
try:
    v = json.load(sys.stdin).get('$1')
    print('' if v is None else v)
except Exception:
    print('')
" 2>/dev/null || printf ''
}

# The harness sets this when the model is continuing *because* a stop hook
# blocked. Blocking again here is what creates an infinite loop.
[ "$(field stop_hook_active)" = "True" ] && allow

session="$(field session_id)"
[ -n "$session" ] || allow

# ── find the claude process this hook was spawned by ─────────────────────────
claude_pid=""
p=$$
for _ in $(seq 1 12); do
  [ -n "$p" ] && [ "$p" != "1" ] || break
  case "$(ps -o comm= -p "$p" 2>/dev/null)" in
    claude) claude_pid="$p"; break ;;
  esac
  p="$(ps -o ppid= -p "$p" 2>/dev/null | tr -d ' ')"
done
[ -n "$claude_pid" ] || allow

# Headless only. An interactive session can be resumed by its user and shows
# background tasks in the UI, so blocking there would be noise — and the ralph
# loop itself runs as a background task of an interactive session, which would
# block that session every turn. Only `claude -p` genuinely cannot be resumed.
case " $(ps -o args= -p "$claude_pid" 2>/dev/null) " in
  *" -p "*|*" --print "*|*" -p") ;;
  *) allow ;;
esac

# ── the branch this hook lives in, so we never count our own machinery ───────
# Not just our ancestors: the hook spawns ps and python3, and those are
# descendants of claude too. Prune the entire branch — the ancestor of $$ whose
# parent IS claude — so everything the hook itself creates is excluded.
hook_branch=""
p=$$
for _ in $(seq 1 12); do
  [ -n "$p" ] && [ "$p" != "1" ] || break
  parent="$(ps -o ppid= -p "$p" 2>/dev/null | tr -d ' ')"
  if [ "$parent" = "$claude_pid" ]; then hook_branch="$p"; break; fi
  p="$parent"
done
[ -n "$hook_branch" ] || allow

# ── live descendants of claude, minus our own chain ──────────────────────────
# Breadth-first over the ps snapshot; a process that exits mid-walk simply
# drops out, which is the safe direction (we under-report, never over-report).
snapshot="$(ps -eo pid=,ppid=,etimes=,args= 2>/dev/null)" || allow
[ -n "$snapshot" ] || allow

found="$(printf '%s\n' "$snapshot" | PRUNE="$hook_branch" ROOT="$claude_pid" MAX_AGE="$MAX_AGE" python3 -c "
import os, sys

prune = os.environ['PRUNE']
root = os.environ['ROOT']
max_age = int(os.environ['MAX_AGE'])

kids, info = {}, {}
for line in sys.stdin:
    parts = line.split(None, 3)
    if len(parts) < 4:
        continue
    pid, ppid, etimes, args = parts[0], parts[1], parts[2], parts[3].rstrip()
    kids.setdefault(ppid, []).append(pid)
    info[pid] = (int(etimes) if etimes.isdigit() else 0, args)

out, seen, stack = [], set(), list(kids.get(root, []))
while stack:
    pid = stack.pop()
    if pid in seen or pid == prune:   # prune the hook's own branch entirely
        continue
    seen.add(pid)
    stack.extend(kids.get(pid, []))
    age, args = info.get(pid, (0, ''))
    if age > max_age:            # stale or leaked — do not hold the session
        continue
    out.append(f'{pid} (running {age}s): {args[:110]}')
print('\n'.join(out[:10]))
" 2>/dev/null || printf '')"

[ -n "$found" ] || allow

# ── bounded blocking ─────────────────────────────────────────────────────────
state_dir="${TMPDIR:-/tmp}/claude-stop-hook"
mkdir -p "$state_dir" 2>/dev/null || allow
counter="$state_dir/$session.blocks"
count=$(( $(cat "$counter" 2>/dev/null || echo 0) + 1 ))
printf '%s' "$count" > "$counter" 2>/dev/null || true
[ "$count" -gt "$MAX_BLOCKS" ] && { rm -f "$counter" 2>/dev/null; allow; }

reason="You still have $(printf '%s\n' "$found" | wc -l) background process(es) running that this session started, so the work is not finished:

$found

Nothing will resume this session after you stop, and no notification will arrive — so ending now abandons that work and the whole iteration is retried from scratch. Wait for it in ONE self-contained Bash call that polls internally, for example:

  until ! kill -0 <pid> 2>/dev/null; do sleep 5; done

then read its output file and continue. (A chained \`sleep N; cat\` is blocked by the harness; poll inside a single call instead.) If the process is genuinely stuck, kill it explicitly and say so — do not simply stop."

python3 -c "
import json, sys
print(json.dumps({'decision': 'block', 'reason': sys.stdin.read()}))
" <<< "$reason" 2>/dev/null || allow
exit 0
