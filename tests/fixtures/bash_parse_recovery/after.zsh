#!/bin/zsh
function check_revision() {
    case "$1" in
        9.<2->*) return 0 ;;
        *) return 1 ;;
    esac
}
function load_record() {
    local record
    if { IFS= read -r record } < <(printf 'sample\n'); then
        :
    fi
    print -r -- "$record"
}
