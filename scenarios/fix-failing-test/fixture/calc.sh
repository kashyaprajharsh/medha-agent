#!/bin/sh
# add <a> <b> — prints the sum of two integers.
# BUG: this subtracts instead of adding.
add() {
  echo $(( $1 - $2 ))
}

add "$1" "$2"
