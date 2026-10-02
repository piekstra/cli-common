#!/bin/sh
# The fake `op` the pk-cli-secrets tests run, through a symlink named `op` in
# each test's own directory. It is checked in, not written by the test: a
# script the test process has just written can fail to exec with ETXTBSY
# (see `op::fake`). Logs its argv, then runs the test's `body`, which sits
# beside the symlink and is only ever read.
dir=$(dirname "$0")
printf '%s\n' "$@" >> "$dir/argv"
echo call >> "$dir/calls"
. "$dir/body"
