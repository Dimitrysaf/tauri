#!/usr/bin/env sh

getcpid() {
    cpids=$(pgrep -P $1|xargs)
    for cpid in $cpids;
    do
        echo "$cpid"
        getcpid $cpid
    done
}

# `getcpid` walks the tree parent-first, so the list is reversed to signal the
# deepest descendants first. Killing a parent ahead of its children leaves them
# orphaned (reparented to init) rather than terminated — for a `beforeDevCommand`
# like `turbo -> pnpm -> vite` that strands the dev server and keeps its port
# bound after the app exits.
#
# The reversal is done here rather than by recursing before echoing, because
# `cpid` is global in POSIX sh: recursing first would clobber it and skip every
# intermediate process.
kill $(getcpid $1 | awk '{ lines[NR] = $0 } END { for (i = NR; i > 0; i--) print lines[i] }')
