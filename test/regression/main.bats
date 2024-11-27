#!/usr/bin/env bats

load helper.bash

setup_file() {
    BATS_NO_PARALLELIZE_WITHIN_FILE=true
}

setup() {
    # assuming WD is the root of the project
    start_cedana

    # get the containing directory of this file
    # use $BATS_TEST_FILENAME instead of ${BASH_SOURCE[0]} or $0,
    # as those will point to the bats executable's location or the preprocessed file respectively
    DIR="$( cd "$( dirname "$BATS_TEST_FILENAME" )" >/dev/null 2>&1 && pwd )"
    TTY_SOCK=$DIR/tty.sock

    cedana debug recvtty "$TTY_SOCK" &
    sleep 1 3>-
}

teardown() {
    sleep 1 3>-
    rm -f $TTY_SOCK
    stop_cedana
    sleep 1 3>-
}

@test "Dump workload with --stream" {
    echo "PATH = $PATH"
    echo "where is cedana-image-streamer: $(whereis cedana-image-streamer)"
    echo "where is date: $(whereis date)"
    echo "where is sleep: $(whereis sleep)"
    echo "ls -l /usr/bin/cedana-image-streamer: $(ls -l /usr/bin/cedana-image-streamer)"
    echo "ls -l /var/log = $(ls -l /var/log)"
    local task="sh -x workload.sh"
    local job_id="workload-stream-1"

    # execute and checkpoint with streaming
    exec_task $task $job_id
    sleep 1 3>-
    checkpoint_task $job_id /tmp --stream 4
    [[ "$status" -eq 0 ]]
}

@test "Restore workload with --stream" {
    local task="./workload.sh"
    local job_id="workload-stream-2"

    # execute, checkpoint and restore with streaming
    exec_task $task $job_id
    sleep 1 3>-
    checkpoint_task $job_id /tmp --stream 4
    sleep 1 3>-
    run restore_task $job_id --stream 4
    [[ "$status" -eq 0 ]]
}
