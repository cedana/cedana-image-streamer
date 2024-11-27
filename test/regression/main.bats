@test "Dump workload with --stream" {
    local task="./workload.sh"
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
