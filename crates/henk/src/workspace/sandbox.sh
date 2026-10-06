#!/bin/sh
# The sandbox script (#84): what Henk runs on a sandbox host, as root.
#
# Nothing is installed on the host for it. Henk keeps this script in its
# binary and sends it with every request, over an SSH session signed in as
# root (or as a user with passwordless sudo, through `sudo -n`):
#
#   sh -c '<this script>' henk-sandbox SUBCOMMAND TOKENS...
#
# Each workspace is a throwaway user `henk-<id>` with its own home, holding
# the run's checkout in ~/work, and a record of the checkout that only root
# can write, outside the home. Everything a run does in its tree is done as
# that user. Root never reads or walks that tree, since the user can change
# it at any moment: the record is fed from a tar the user makes of it and
# root unpacks into a copy only root can reach. A request is a subcommand,
# then tokens that are a workspace id, a number, a hex sha, or `b` and base64
# (the `b` keeps an empty value a token). Henk refuses any other character
# before it sends a request, and this script checks again, so no part of a
# request is ever shell-evaluated.
#
#   create ID                 make the user, ~/work and the record; print the
#                             real path of ~/work
#   import ID                 unpack a tar from stdin into ~/work, record it
#   as ID SECS CWD ARGV...    run ARGV (b+base64) in ~/work/CWD (b+base64) as the
#                             user, empty environment, at most SECS seconds
#                             (a decimal number)
#   record ID diff            stop the user's processes, then the changes since
#                             import, as `git diff --raw -z`
#   record ID blob SHA        the content of one changed file
#   record ID baseline        stop the user's processes, then record the tree
#                             as it is now: later diffs start from here
#   destroy ID                kill the user's processes, remove it and its files
#   sweep                     destroy every workspace on this host
#   probe                     print the version and which tools are present
#
# Single-user mode: run as a normal user with HENK_SANDBOX_BASE set, a
# workspace is a directory under it and nothing changes user. As root that
# variable is ignored. It is what a Pod of the kubernetes backend (#89)
# runs: the Pod is the workspace and its boundary, and HENK_SANDBOX_POD=1
# says so, which lets `stop` end every other process in it. The tests run
# the same mode on their own machine, without HENK_SANDBOX_POD.
set -eu
umask 022

VERSION=5
RUN_PATH=/usr/local/bin:/usr/bin:/bin
# Named after the runner this script replaced, so a sweep still finds what
# an older Henk left on the host.
RECORDS=/var/lib/henk-runner

die() {
    echo "henk-sandbox: $*" >&2
    exit 64
}

for token in "$@"; do
    case $token in
    "" | *[!A-Za-z0-9+/=_.-]*) die "refused: unexpected characters in the request" ;;
    esac
done

if [ "$(id -u)" -eq 0 ]; then
    BASE=
elif [ -n "${HENK_SANDBOX_BASE:-}" ]; then
    BASE=$HENK_SANDBOX_BASE
else
    die "the sandbox script needs root: sign in as root, or as a user with passwordless sudo"
fi

[ $# -ge 1 ] || die "no request"
command=$1
shift

# Whether $1 is a workspace id: `w` and twelve hex digits.
is_id() {
    case $1 in
    w[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]) return 0 ;;
    *) return 1 ;;
    esac
}

# The paths of workspace $1, checked to be one of ours.
workspace() {
    is_id "$1" || die "not a workspace id: $1"
    id=$1
    user=henk-$id
    if [ -n "$BASE" ]; then
        home=$BASE/$id/home
        record=$BASE/$id/record
    else
        home=/home/$user
        record=$RECORDS/$id
    fi
    work=$home/work
}

# Runs "$@" as the workspace's user.
as_user() {
    if [ -n "$BASE" ]; then
        "$@"
    else
        runuser -u "$user" -- "$@"
    fi
}

# git on the record, never on the tree's own .git, with no hooks, no
# fsmonitor and no system or global configuration. Its work tree is the copy
# `snapshot` makes, never the user's tree.
record_git() {
    env -i PATH="$RUN_PATH" HOME="$record" GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null \
        git -c core.hooksPath=/dev/null -c core.fsmonitor=false -c commit.gpgSign=false \
        -c user.name=Henk -c user.email=henk@workspace.invalid -c 'safe.directory=*' \
        --git-dir="$record/git" --work-tree="$record/tree" "$@"
}

# Kills every process of the workspace's user, until none is left; fails
# when they keep coming.
stop() {
    if [ -n "$BASE" ]; then
        [ "${HENK_SANDBOX_POD:-}" = 1 ] || return 0
        # In a Pod, every process but the Pod's first and this script is the
        # run's: end them all, as `pkill -u` does on a sandbox host.
        tries=0
        while :; do
            left=0
            for proc in /proc/[0-9]*; do
                pid=${proc#/proc/}
                case $pid in 1 | "$$") continue ;; esac
                # A killed process stays a zombie under the Pod's first
                # process, which reaps nothing: it is dead, not running.
                # The State line is the kernel's; a process's own name
                # cannot fake it.
                zombie=0
                while IFS= read -r line; do
                    case $line in "State:"*Z*) zombie=1 ;; esac
                done <"$proc/status" 2>/dev/null || continue
                [ "$zombie" -eq 0 ] || continue
                kill -KILL "$pid" 2>/dev/null && left=1
            done
            [ "$left" -eq 1 ] || return 0
            tries=$((tries + 1))
            [ "$tries" -lt 50 ] || return 1
            sleep 0.1
        done
    fi
    uid=$(id -u "$user" 2>/dev/null) || return 0
    tries=0
    while pkill -KILL -u "$uid" 2>/dev/null; do
        tries=$((tries + 1))
        [ "$tries" -lt 50 ] || return 1
        sleep 0.1
    done
}

# Copies the tree into $record/tree, which only root can reach. The user
# makes the tar, so it holds only what the user can read and a link is kept
# as a link; root unpacks it and never follows a path the user controls. The
# tree's own top-level .git is left out: the record never looks at it.
snapshot() {
    if [ -L "$work" ] || [ ! -d "$work" ]; then
        die "the tree is not a directory any more"
    fi
    rm -rf "${record:?}/tree" "$record/tree.tar"
    mkdir -m 700 "$record/tree"
    as_user tar -c -f - -C "$work" --anchored --exclude=./.git . >"$record/tree.tar" ||
        die "cannot read the tree"
    tar -x -f "$record/tree.tar" -C "$record/tree" \
        --no-same-owner --no-same-permissions --no-overwrite-dir ||
        die "cannot copy the tree"
    rm -f "$record/tree.tar"
}

# The value of one `b` and base64 token, trailing newlines kept.
decode() {
    case $1 in b*) ;; *) die "not a value token: $1" ;; esac
    value=$(printf %s "${1#b}" | base64 -d && printf x) || die "not base64: $1"
    value=${value%x}
}

destroy() {
    if [ -n "$BASE" ]; then
        rm -rf "${BASE:?}/$id"
        return 0
    fi
    if id -u "$user" >/dev/null 2>&1; then
        uid=$(id -u "$user")
        stop || true
        loginctl terminate-user "$user" 2>/dev/null || true
        for dir in /tmp /var/tmp /dev/shm; do
            find "$dir" -xdev -uid "$uid" -delete 2>/dev/null || true
        done
        userdel --remove "$user" 2>/dev/null ||
            { sleep 1 && pkill -KILL -u "$uid" 2>/dev/null; userdel --remove "$user"; } ||
            true
    fi
    rm -rf "${RECORDS:?}/$id"
}

case $command in
create)
    [ $# -eq 1 ] || die "usage: create ID"
    workspace "$1"
    if [ -n "$BASE" ]; then
        mkdir -p "$home"
    else
        useradd --create-home --home-dir "$home" --shell /bin/sh --user-group "$user"
    fi
    chmod 700 "$home"
    mkdir -p "$record"
    chmod 700 "$record"
    as_user mkdir "$work"
    # The real path: Henk compares it with what realpath prints in the tree,
    # and /home may itself be a link (/var/home on Fedora Atomic).
    realpath -e -- "$work"
    ;;
import)
    [ $# -eq 1 ] || die "usage: import ID"
    workspace "$1"
    as_user tar -x -f - -C "$work" --no-same-owner
    snapshot
    record_git init --quiet
    record_git add --all --force
    record_git commit --quiet --allow-empty --message import
    ;;
as)
    [ $# -ge 4 ] || die "usage: as ID SECS CWD ARGV..."
    workspace "$1"
    secs=$2
    case $secs in '' | *[!0-9.]* | *.*.*) die "not a number of seconds: $secs" ;; esac
    decode "$3"
    cwd=$value
    shift 3
    count=$#
    while [ "$count" -gt 0 ]; do
        decode "$1"
        shift
        set -- "$@" "$value"
        count=$((count - 1))
    done
    # The user changes directory, not root, so the kernel checks the user's
    # own permissions on every link in the way.
    # shellcheck disable=SC2016 # expanded by the inner shell
    enter='cd -- "$1" 2>/dev/null || { echo "henk-sandbox: no such directory: $2" >&2; exit 64; }
shift 2
exec env -i PATH="$PATH" HOME="$HOME" LANG=C.UTF-8 "$@"'
    if [ -n "$BASE" ]; then
        exec env -i PATH="$RUN_PATH" HOME="$home" LANG=C.UTF-8 \
            /bin/sh -c "$enter" sh "$work/$cwd" "$cwd" timeout --kill-after=5 "$secs" "$@"
    fi
    exec runuser -u "$user" -- env -i PATH="$RUN_PATH" HOME="$home" LANG=C.UTF-8 \
        /bin/sh -c "$enter" sh "$work/$cwd" "$cwd" timeout --kill-after=5 "$secs" "$@"
    ;;
record)
    [ $# -ge 2 ] || die "usage: record ID diff | record ID baseline | record ID blob SHA"
    workspace "$1"
    case $2 in
    diff)
        stop || die "the processes of $user do not stop"
        snapshot
        record_git add --all
        record_git diff --cached --raw -z --no-renames --no-ext-diff --no-textconv --no-abbrev HEAD
        ;;
    baseline)
        [ $# -eq 2 ] || die "usage: record ID baseline"
        stop || die "the processes of $user do not stop"
        snapshot
        record_git add --all
        record_git commit --quiet --allow-empty --message setup
        ;;
    blob)
        [ $# -eq 3 ] || die "usage: record ID blob SHA"
        case $3 in '' | *[!0-9a-f]*) die "not a sha: $3" ;; esac
        record_git cat-file blob "$3"
        ;;
    *) die "unknown record request: $2" ;;
    esac
    ;;
destroy)
    [ $# -eq 1 ] || die "usage: destroy ID"
    workspace "$1"
    destroy
    ;;
sweep)
    [ $# -eq 0 ] || die "usage: sweep"
    if [ -n "$BASE" ]; then
        found=$(ls "$BASE" 2>/dev/null || true)
    else
        found=$(getent passwd | cut -d: -f1 | sed -n 's/^henk-\(w[0-9a-f]\{12\}\)$/\1/p')
        found="$found $(ls "$RECORDS" 2>/dev/null || true)"
    fi
    for one in $found; do
        if is_id "$one"; then
            workspace "$one"
            destroy
        fi
    done
    ;;
probe)
    echo "henk-sandbox $VERSION"
    # What runs: on the PATH the run's commands get.
    for tool in git tar timeout realpath find grep stat mise; do
        if found=$(PATH=$RUN_PATH command -v "$tool"); then
            echo "$tool $found"
        else
            echo "$tool missing"
        fi
    done
    # The code tools' search hands grep -P a pattern that starts with
    # (*UCP), under LANG=C.UTF-8 as `as` runs it, so \w, \s and \b know
    # letters and spaces beyond ASCII whatever GNU grep's version. This
    # checks that very form: busybox's grep has no -P, and a PCRE built
    # without Unicode refuses (*UCP) or reads the bytes one by one.
    if printf 'caf\303\251\302\240x\n' |
        PATH=$RUN_PATH LANG=C.UTF-8 grep -qP '(*UCP)^caf\w\b\sx$' 2>/dev/null; then
        echo "grep-pcre yes"
    else
        echo "grep-pcre missing"
    fi
    # What the runner itself needs as root, on its own PATH.
    if [ -z "$BASE" ]; then
        for tool in runuser useradd userdel pkill; do
            if found=$(command -v "$tool"); then
                echo "$tool $found"
            else
                echo "$tool missing"
            fi
        done
    fi
    ;;
*) die "unknown request: $command" ;;
esac
