#!/bin/bash
# mksandbox.sh <dir> — build a fresh single-file git repo sandbox for one spike turn.
set -eu
# This wipes its argument, so refuse anything that is not a plausible scratch path.
d="${1:-}"
case "$d" in
  ""|"/"|"$HOME"|"$HOME/") echo "mksandbox.sh: refusing to wipe '${d:-<empty>}'" >&2; exit 2 ;;
  -*)                      echo "mksandbox.sh: <dir> must not start with '-'" >&2;      exit 2 ;;
esac
rm -rf -- "$d"; mkdir -p "$d"; cd "$d"
git init -q -b main .
cat > NOTES.md <<'EOF'
# Counter notes

The build counter lives in `counter.txt`. It is a single integer on one line.
`./check.sh` prints the current value. The counter must be bumped by exactly one
whenever the notes change.
EOF
printf '7\n' > counter.txt
cat > check.sh <<'EOF'
#!/bin/bash
echo "counter=$(cat counter.txt)"
EOF
chmod +x check.sh

# prompt-long.txt reads and edits these five; prompt-slow-child.txt runs slow.sh.
# Every committed prompt must find everything it names in a freshly built sandbox.
for n in 1 2 3 4 5; do
  printf 'module m%s\nvalue = %s0\n' "$n" "$n" > "mod$n.conf"
done
cat > slow.sh <<'EOF'
#!/bin/bash
# A child that outlives its parent's patience — the kill-path probe. Deliberately
# `sleep`s as a separate process so killtest.py has a grandchild to look for.
echo "slow: starting"
sleep 120
echo "slow: done"
EOF
chmod +x slow.sh
: "${RHAPSODYD:=/Applications/Rhapsody.app/Contents/Resources/rhapsodyd}"
: "${WORKFLOW:=$HOME/.rhapsody/WORKFLOW.md}"
cat > .symphony-mcp.json <<EOF
{
  "mcpServers": {"symphony":{"args":["mcp","$WORKFLOW"],"command":"$RHAPSODYD","env":{}}}
}
EOF
git add -A; git -c user.email=spike@local -c user.name=spike commit -qm "sandbox"
