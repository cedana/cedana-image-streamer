#!/bin/bash -e
# Used to run regression bats tests (located in tests/regression)

source ./helpers.sh

function start_regression() {
    echo "Running regression tests in cwd: $(pwd)"
    mkdir ~/bats_out
    bats --trace --verbose-run --gather-test-outputs-in ~/bats_out test/regression/main.bats
}

main() {
    pushd ../..
    print_env
    source_env
    start_regression

    popd
}

main
