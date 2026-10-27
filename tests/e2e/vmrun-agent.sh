#!/usr/bin/env bash
# Runs vmrun guest operations for tests/e2e/windows.sh, so the tests need no SSH or dev tunnel.
# A person starts it in a terminal and types the VM password and the Windows login at hidden
# prompts. The secrets stay in this process. Jobs arrive as files in the queue directory: one
# vmrun argument per line. Only guest file copies and program runs are allowed.
# Usage: tests/e2e/vmrun-agent.sh <queue dir> [<vmx path>]
set -u
Q=${1:?queue dir}
VMX=${2:-"$HOME/Virtual Machines.localized/Windows 11 64-bit Arm.vmwarevm/Windows 11 64-bit Arm.vmx"}
V="/Applications/VMware Fusion.app/Contents/Library/vmrun"
mkdir -p "$Q" && chmod 700 "$Q"
read -r -s -p "VM encryption password: " VMPW; echo
read -r -p "Windows user: " GU
read -r -s -p "Windows password for $GU: " GP; echo
GU=$(printf '%s' "$GU" | tr -d '[:space:]')
if ! err=$("$V" -T fusion -vp "$VMPW" -gu "$GU" -gp "$GP" listProcessesInGuest "$VMX" 2>&1 >/dev/null); then
	# vmrun errors never contain the passwords.
	echo "vmrun could not log in to the guest as \"$GU\": $err" | tee "$Q/status"
	exit 1
fi
echo "ready: guest login works as $GU. Leave this running; Ctrl-C stops it." | tee "$Q/status"
: > "$Q/ready"
while :; do
	for job in "$Q"/*.job; do
		[ -e "$job" ] || continue
		args=()
		while IFS= read -r line; do args+=("$line"); done < "$job"
		case "${args[0]:-}" in
			copyFileFromHostToGuest|copyFileFromGuestToHost|runProgramInGuest|fileExistsInGuest) ;;
			*) echo "refused: ${args[0]:-empty}" > "${job%.job}.log"; echo 2 > "${job%.job}.rc"; rm -f "$job"; continue ;;
		esac
		"$V" -T fusion -vp "$VMPW" -gu "$GU" -gp "$GP" "${args[0]}" "$VMX" "${args[@]:1}" > "${job%.job}.log" 2>&1
		echo $? > "${job%.job}.rc"
		rm -f "$job"
	done
	sleep 0.2
done
