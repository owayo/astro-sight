#!/bin/bash
# Bash fixture: trap '<handler>' SIG 構文の handler 文字列内の関数参照を
# astro-sight が解決できるかを検証するためのサンプル。

cleanup_signal() {
    local sig_exit=$1
    echo "received signal: ${sig_exit}"
    exit "${sig_exit}"
}

cleanup_exit() {
    echo "exiting"
}

# シングルクォート / ダブルクォート両方の handler パターン
trap 'cleanup_signal 130' INT
trap "cleanup_signal 143" TERM
trap 'cleanup_exit' EXIT

# 通常の関数呼び出し
cleanup_signal 0
