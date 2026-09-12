#!/bin/bash
# mkslow.sh <sandbox-dir> — add the slow child that prompt-slow-child.txt tells the
# agent to run. mksandbox.sh does not create it, and the kill tests need it.
set -eu
d="${1:-}"
[ -d "$d" ] || { echo "mkslow.sh: '$d' is not a directory" >&2; exit 2; }
cat > "$d/slow.sh" <<'EOF'
#!/bin/bash
echo "slow start"
sleep 120
echo "slow done"
EOF
chmod +x "$d/slow.sh"
