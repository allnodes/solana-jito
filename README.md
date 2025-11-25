<p align="center">
    <br /><br />
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="allnodes/images/jito-dark-mode.png">
      <img alt="Jito Allnodes Edition" src="allnodes/images/jito-light-mode.png" style="width: 16em">
    </picture>
</p>

# Jito's fork of the Solana validator with modifications from Allnodes

## Modifications made by Allnodes

This repository features the following enhancements to the Jito-Solana codebase:

### 1. Fast snapshot distribution

✅ Only on [Allnodes Bare-Metal Servers](https://www.allnodes.com/hosting)

Our infrastructure includes modifications that improve default snapshot downloading, which combined with
ultra-high-speed channels deliver ultra-fast snapshot downloads. This dramatically reduces the initial sync time for
new validators and enables faster deployment and recovery scenarios. The use of snapshot-finder or any other 3rd party 
download tools is no longer needed.

### 2. Enhanced voting logic modifications

✅ Only on [Allnodes Bare-Metal Servers](https://www.allnodes.com/hosting)

Our validator implementation includes voting modifications developed by **Zantetsu | Shinobi Systems** that enhance the
original voting logic.

These modifications work by:

- Taking the next votable slot that the original codebase identifies as potentially ready for voting
- Applying additional criteria before casting the vote
- Providing more sophisticated voting decision-making

This enhancement improves validator consensus participation through more intelligent vote timing and slot evaluation.

### 3. Automatic Performance Optimization for Proof-of-History

✅ Only on [Allnodes Bare-Metal Servers](https://www.allnodes.com/hosting)

Your Solana node will automatically select the fastest CPU core for Proof-of-History processing, maximizing performance
out of the box.

### 4. Hardware-optimized SHA256 patch

Our validator implementation includes a third-party performance patch developed by **kagren**. It optimizes SHA256
hashing operations using SHA-NI instructions available on modern AMD processors (Zen3, Zen4, and Zen5
architectures). This enhancement significantly improves hashing performance for block verification and other
cryptographic operations.

## Building and running

> [!NOTE]
> We recommend checking out Jito's [Gitbook](https://jito-foundation.gitbook.io/mev/jito-solana/building-the-software) 
> for more detailed instructions on building and running Jito-Solana.

### 1. Install rustc, cargo and rustfmt

```bash
$ curl https://sh.rustup.rs -sSf | sh
$ source $HOME/.cargo/env
$ rustup component add rustfmt
```

When building the master branch, please make sure you are using the latest stable rust version by running:

```bash
$ rustup update
```

On Linux systems you may need to install libssl-dev, pkg-config, zlib1g-dev, protobuf etc.

On Ubuntu:

```bash
$ sudo apt-get update
$ sudo apt-get install libssl-dev libudev-dev pkg-config zlib1g-dev llvm clang cmake make libprotobuf-dev protobuf-compiler libclang-dev curl git
```

On Fedora:

```bash
$ sudo dnf install openssl-devel systemd-devel pkg-config zlib-devel llvm clang cmake make protobuf-devel protobuf-compiler perl-core libclang-dev curl git
```

### 2. Download the source code

To download the source code, run (substitute `<version>` with the version tag you want to build):

```bash
$ git clone --recursive https://github.com/allnodes/solana-jito --branch <version>
$ cd solana-jito
```

### 3. Release build

```bash
$ ./cargo build --release
```

### 4. Voting mod configuration

Voting mod (also known as "mostly confirmed threshold" voting patch) is enabled by default and comes with a predefined 
configuration which should work for most users. If you wish to use a custom configuration:

1. create a configuration file (default filename is `mostly_confirmed_threshold` located in the current directory from 
   where you run the validator). Values in this example are defaults, their meanings will be explained in the next 
   section:

```bash
echo '0.45 4 0 24' > ./mostly_confirmed_threshold
```

2. optionally, you can provide a different filename and/or path for the config file using the 
  `--mostly-confirmed-threshold-config <path/to/config/file>` argument.

> In order to disable the voting mod, you need to add the `--disable-mostly-confirmed-threshold` flag to the validator
command.

## Mostly confirmed threshold configuration file format:

The `mostly_confirmed_threshold` file contains a simple whitespace-separated list of four values:

```
a b c d
```

### Parameters

#### *a* (float) - vote weight threshold
The minimum vote weight threshold required before voting on a slot. Slots that haven't achieved this vote weight will 
not be voted on, except for:

- Slots within the "vote ahead of threshold" region
- When the escape hatch distance has been reached

#### *b* (integer) - vote ahead of threshold
The number of slots ahead of the threshold slot to vote on, regardless of vote weight. This parameter reduces vote 
latency by allowing voting on recent slots even if they haven't met the threshold.

#### *c* (integer) - skip recovery mode
Controls the stake-weighted vote percentage required on a slot after skips have occurred. Must be one of:

- `0` - No restriction
- `1` - Slot after a skip must have `mostly_confirmed_threshold` before voting
- `2` - Slot after a skip must be confirmed before voting

#### *d* (integer) - escape hatch distance
The maximum number of slots to wait without voting while waiting for the threshold to be met. After this many slots of non-voting, the validator will vote anyway.

**Purpose**: This escape hatch prevents network deadlock by ensuring progress even when the threshold isn't being achieved. Without this mechanism, if multiple forks occur simultaneously and all have less than the threshold vote weight, validators could become stuck waiting indefinitely.

### Default values

When the configuration file is absent, the following default values are used:

```
0.45 4 0 24
```

- Threshold: 45% vote weight
- Vote ahead: 4 slots
- Skip recovery: No restriction
- Escape hatch: 24 slots