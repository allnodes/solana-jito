#!/usr/bin/env bash

set -e

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

os_name="$1"

case "$os_name" in
"macOS")
  brew install llvm protobuf
  LIBCLANG_PATH="$(brew --prefix llvm)/lib"
  export LIBCLANG_PATH
  ;;
"Linux")
  if grep "Alpine" /etc/os-release ; then
    apk update
    apk add \
      build-base \
      clang-libclang \
      eudev-dev \
      hidapi-dev \
      linux-headers \
      llvm-dev \
      musl-dev \
      perl
  else
    sudo apt update
    sudo apt install -y libclang-dev pkg-config libudev-dev protobuf-compiler
  fi
  ;;
*)
  echo "Unknown Operating System"
  ;;
esac
