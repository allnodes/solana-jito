#!/usr/bin/env bash

set -e

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

os_name="$1"

case "$os_name" in
"Windows")
  # initialize vcpkg.json. the builtin-baseline is the commit that contains required version.
  # the commit hash can be found in https://github.com/microsoft/vcpkg with `git log --pretty=format:"%H %s" | grep openssl`
  # can be verified with:
  # web:
  #     https://github.com/microsoft/vcpkg/blob/__COMMIT_HASH__/versions/o-/openssl.json
  # command:
  #     git show __COMMIT_HASH__:versions/o-/openssl.json
  cat > vcpkg.json <<EOL
{
  "dependencies": ["openssl"],
  "overrides": [
    {
      "name": "openssl",
      "version": "3.4.1"
    }
  ],
  "builtin-baseline": "5ee5eee0d3e9c6098b24d263e9099edcdcef6631"
}
EOL
  vcpkg install --triplet x64-windows-static-md
  rm vcpkg.json
  export OPENSSL_LIB_DIR="$PWD/vcpkg_installed/x64-windows-static-md/lib"
  export OPENSSL_INCLUDE_DIR="$PWD/vcpkg_installed/x64-windows-static-md/include"

  choco install protoc
  export PROTOC='C:\ProgramData\chocolatey\lib\protoc\tools\bin\protoc.exe'
  ;;
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
