#!/bin/bash
# mksandbox.sh <dir> — build a fresh single-file git repo sandbox for one spike turn.
set -e
d="$1"; rm -rf "$d"; mkdir -p "$d"; cd "$d"
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
: "${RHAPSODYD:=/Applications/Rhapsody.app/Contents/Resources/rhapsodyd}"
: "${WORKFLOW:=$HOME/.rhapsody/WORKFLOW.md}"
cat > .symphony-mcp.json <<EOF
{
  "mcpServers": {"symphony":{"args":["mcp","$WORKFLOW"],"command":"$RHAPSODYD","env":{}}}
}
EOF
git add -A; git -c user.email=spike@local -c user.name=spike commit -qm "sandbox"
