## Jito's fork of the Solana validator with modifications from Allnodes

## Modifications made by Allnodes

This repository contains the following modifications from the original Solana repository with Jito's changes:

### 1. Enhanced Snapshot Download Performance

Improved snapshot downloading speed during the bootstrap process by providing fast nodes for snapshots delivery. 

### 2. Voting Modifications

Added voting modifications that enhance the original voting logic. These modifications work by:

- Taking the next votable slot that the original codebase identifies as potentially ready for voting
- Applying additional criteria before casting the vote
- Providing more sophisticated voting decision-making

# Building and Running

---
We recommend checking out Jito's [Gitbook](https://jito-foundation.gitbook.io/mev/jito-solana/building-the-software) for
more detailed instructions on building and running Jito-Solana.
---

## 1. Install rustc, cargo and rustfmt.

```bash
$ curl https://sh.rustup.rs -sSf | sh
$ source $HOME/.cargo/env
$ rustup component add rustfmt
```

When building the master branch, please make sure you are using the latest stable rust version by running:

```bash
$ rustup update
```

When building a specific release branch, you should check the rust version in `ci/rust-version.sh` and if necessary,
install that version by running:

```bash
$ rustup install VERSION
```

Note that if this is not the latest rust version on your machine, cargo commands may require
an [override](https://rust-lang.github.io/rustup/overrides.html) in order to use the correct version.

On Linux systems you may need to install libssl-dev, pkg-config, zlib1g-dev, protobuf etc.

On Ubuntu:

```bash
$ sudo apt-get update
$ sudo apt-get install libssl-dev libudev-dev pkg-config zlib1g-dev llvm clang cmake make libprotobuf-dev protobuf-compiler libclang-dev
```

On Fedora:

```bash
$ sudo dnf install openssl-devel systemd-devel pkg-config zlib-devel llvm clang cmake make protobuf-devel protobuf-compiler perl-core libclang-dev
```

## 2. Download the source code.

```bash
$ git clone --recursive https://github.com/allnodes/solana-jito
$ cd solana-jito
```

## **3. Release build.**

```bash
$ ./cargo build --release
```

## 4. Voting mods configuration.

Voting mods (also known as "mostly confirmed threshold" voting patch) is enabled by default and use the default mods 
configuration, which should work for most users. If you wish to use different configuration:

1. create a configuration file (default filename is `mostly_confirmed_threshold` located in the current directory from 
   where you run the validator). Values in this example are defaults, their meanings will be explained in the next 
   section:

```bash
echo '0.45 4 0 24' > ./mostly_confirmed_threshold
```

2. optionally, you can provide a different filename and/or path for the config file using the 
  `--mostly-confirmed-threshold-config <path/to/config/file>` argument.

> In order to disable voting mods, you need to add the `--disable-mostly-confirmed-threshold` flag to the validator
command.

## "Mostly confirmed threshold" configuration file format:

The `mostly_confirmed_threshold` file contains a simple whitespace-separated list of four values:

```
a b c d
```

### Parameters

#### *a* (float) - Vote Weight Threshold
The minimum vote weight threshold required before voting on a slot. Slots that haven't achieved this vote weight will 
not be voted on, except for:

- Slots within the "vote ahead of threshold" region
- When the escape hatch distance has been reached

#### *b* (integer) - Vote Ahead of Threshold
The number of slots ahead of the threshold slot to vote on, regardless of vote weight. This parameter reduces vote 
latency by allowing voting on recent slots even if they haven't met the threshold.

#### *c* (integer) - Skip Recovery Mode
Controls the stake-weighted vote percentage required on a slot after skips have occurred. Must be one of:

- `0` - No restriction
- `1` - Slot after a skip must have `mostly_confirmed_threshold` before voting
- `2` - Slot after a skip must be confirmed before voting

#### *d* (integer) - Escape Hatch Distance
The maximum number of slots to wait without voting while waiting for the threshold to be met. After this many slots of non-voting, the validator will vote anyway.

**Purpose**: This escape hatch prevents network deadlock by ensuring progress even when the threshold isn't being achieved. Without this mechanism, if multiple forks occur simultaneously and all have less than the threshold vote weight, validators could become stuck waiting indefinitely.

### Default Values

When the configuration file is absent, the following default values are used:

```
0.45 4 0 24
```

- Threshold: 45% vote weight
- Vote ahead: 4 slots
- Skip recovery: No restriction
- Escape hatch: 24 slots