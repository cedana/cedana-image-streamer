#!/bin/bash


## NOTE: All scripts are being run by the makefile, which runs in the scripts/ci directory.
## As a result, where these functions are called rely on managing directory state using pushd/popd,
## which also means all these functions assume they're being run in the root directory.
## Look at regression-test main for an example.
##

APT_PACKAGES="wget git make curl libnl-3-dev libnet-dev \
    libbsd-dev libcap-dev libgpgme-dev \
    btrfs-progs libbtrfs-dev libseccomp-dev libapparmor-dev \
    libprotobuf-dev libprotobuf-c-dev protobuf-c-compiler \
    protobuf-compiler python3-protobuf software-properties-common \
    zip
"

install_apt_packages() {
    apt-get update
    for pkg in $APT_PACKAGES; do
        apt-get install -y $pkg || echo "failed to install $pkg"
    done
}

print_header() {
    echo "############### $1 ###############"
}

print_env() {
    set +x
    print_header "Environment variables"
    printenv
    print_header "uname -a"
    uname -a || :
    print_header "Mounted file systems"
    cat /proc/self/mountinfo || :
    print_header "Kernel command line"
    cat /proc/cmdline || :
    print_header "Kernel modules"
    lsmod || :
    print_header "Distribution information"
    [ -e /etc/lsb-release ] && cat /etc/lsb-release
    [ -e /etc/redhat-release ] && cat /etc/redhat-release
    [ -e /etc/alpine-release ] && cat /etc/alpine-release
    print_header "ulimit -a"
    ulimit -a
    print_header "Available memory"
    if [ -e /etc/alpine-release ]; then
        # Alpine's busybox based free does not understand -h
        free
    else
        free -h
    fi
    print_header "Available CPUs"
    lscpu || :
    set -x
}

setup_ci_build() {
    # only CI steps needed for building
    [ -n "$SKIP_CI_SETUP" ] && return
    install_apt_packages
}

source_env() {
    source /etc/environment
}

start_cedana() {
    ./build-start-daemon.sh --no-build
}

stop_cedana() {
    ./reset.sh
}
