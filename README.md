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

✅ Only on [Allnodes Bare-Metal Servers](https://www.allnodes.com/hosting/solana)

Our infrastructure includes modifications that improve default snapshot downloading, which combined with
ultra-high-speed channels deliver ultra-fast snapshot downloads. This dramatically reduces the initial sync time for
new validators and enables faster deployment and recovery scenarios. The use of snapshot-finder or any other 3rd party 
download tools is no longer needed.

### 2. Hardware-optimized SHA256 patch

Our validator implementation includes a third-party performance patch developed by **kagren**. It optimizes SHA256
hashing operations using SHA-NI instructions available on modern AMD processors (Zen3, Zen4, and Zen5
architectures). This enhancement significantly improves hashing performance for block verification and other
cryptographic operations.

### 3. Improved XDP and added support for bonded interfaces

When XDP is enabled, your node receives transactions and blocks over AF_XDP, bypassing the operating system. Agave uses
XDP for sending only, so everything arriving at our modification of Agave is handled faster than on a stock client.

Bonded interfaces are supported: name the bond master and every slave is accelerated, and if one link goes down,
traffic keeps flowing over the others.

Now you can configure the validator to share a machine and a network card with other applications that use XDP: each
gets its own slice of the card's receive queues, so they do not interfere with each other. In this case, the validator
does not take over the interface, so another XDP program can keep using it.

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

The `rust-toolchain.toml` file pins a specific rust version and ensures that
cargo commands run with that version. Note that cargo will automatically install
the correct version if it is not already installed.

On Linux systems you may need to install libssl-dev, pkg-config, zlib1g-dev, protobuf etc.

On Ubuntu:

```bash
$ sudo apt-get update
$ sudo apt-get install libssl-dev libudev-dev pkg-config zlib1g-dev llvm clang cmake make libprotobuf-dev protobuf-compiler libclang-dev curl git hwdata
```

On Fedora:

```bash
$ sudo dnf install openssl-devel systemd-devel pkg-config zlib-devel llvm clang cmake make protobuf-devel protobuf-compiler perl-core clang-devel curl git hwdata
```

`hwdata` supplies `/usr/share/hwdata/pci.ids`, which the validator reads to report
the network adapter's vendor and model in its metrics. It is present on most
desktop and server installs but missing from slim container images, where its
absence produces an hourly warning and "unknown" hardware in the metrics. Nothing
else depends on it.

### 2. Download the source code

Clone the repository (substitute `<version>` with the version tag you want to build):

```bash
$ git clone https://github.com/allnodes/solana-jito --branch <version>
$ cd solana-jito
```

Then fetch the submodules. **This step is required**: the build depends on code
that lives in them, and `git clone` does not download it:

```bash
$ git submodule update --init --recursive
```

> [!IMPORTANT]
> Skipping this step makes the build fail with `failed to load manifest` errors
> for crates under `allnodes/protos` or `allnodes/sdk`. If you see them, run the
> command above and build again. It is safe to run more than once,
> and it must be run again after every `git checkout` of another version.

### 3. Release build

```bash
$ ./cargo build --release
```
> [!NOTE]
> Note that this builds a debug version that is **not suitable for running a testnet or mainnet validator**. Please read [the install guide](https://docs.anza.xyz/cli/install#build-from-source) for instructions to build a release version for test and production uses.

### 4. Grant capabilities for XDP (Linux-only)

XDP is enabled on Linux by default and needs extra capabilities to set up the
network interface at startup. Grant them to the built binary:

```bash
$ sudo setcap 'cap_net_admin,cap_net_raw,cap_bpf,cap_perfmon+p' <path-to-agave-validator-binary>
```

There is no need to run the validator as root. It raises these capabilities only
while it configures the interface, then drops them irreversibly before it starts
validating and locks down the corresponding syscalls. The threads it goes on to
spawn are ordinary unprivileged ones, with one exception: the thread that keeps the
kernel's neighbour table fresh holds `cap_net_admin` for the whole run. Linux
refreshes a neighbour entry by itself when an ordinary socket sends to it; over
AF_XDP it does not, so the entry has to be renewed explicitly, and netlink needs
that capability to do it.

Two options need more than the list above:

| also needed | when |
|---|---|
| `cap_sys_admin` | `--xdp-chain-loading`, which inspects an XDP program already attached to the interface in order to chain onto it |
| `cap_sys_nice` | a non-zero `--rpc-niceness-adjustment` or `--snapshot-packager-niceness-adjustment`; kept for the whole run, since it applies to threads started later |

Only the first pair is required:

| granted | what runs |
|---|---|
| `cap_net_admin,cap_net_raw` | shreds are sent over AF_XDP; everything is received through the kernel |
| the same, on a node that already ran with all four | the full thing, over the program the previous run installed and left behind |
| all four | the full thing — receive is accelerated too |

Without `cap_bpf` and `cap_perfmon` the validator starts anyway, accelerates
transmit only, and warns once with the complete `setcap` command for your build.
`--xdp-zero-copy` is the exception: it cannot work without the XDP program, so
there the missing capabilities are fatal and the validator stops.

To run without any of this, start the validator with `--no-xdp`: shreds go out
through ordinary UDP sockets and every port is received through the kernel.

The rest of the XDP options, none of which are required:

| option | what it does |
|---|---|
| `--xdp-interface <IF>` | the interface to accelerate; auto-detected from the default route when unset. A bond master accelerates every slave |
| `--xdp-zero-copy` | zero-copy sockets where the driver supports them. Makes the receive path mandatory: what would otherwise degrade becomes a refusal to start |
| `--xdp-chain-loading` | install through an XDP dispatcher so other XDP programs can share the interface. Needs `cap_sys_admin`; not needed to clean up after a previous run of your own |
| `--xdp-queue-base <N>` | pin the first NIC receive queue this instance may use, instead of finding a free range by probing. Honored or the start fails — never quietly replaced |
| `--xdp-cpu-cores <LIST>` | cores for the transmit loops; defaults to one per physical device |

To run more than one validator on a single card, give each its own
`--xdp-queue-base` so their queue ranges do not overlap; `ethtool -l <IF>` shows
how many queues there are to divide. Without it each instance finds a free range by
probing, which is reliable unless two of them start at the same moment.

The receive half also asks something of the card: it has to steer a UDP port to a
chosen receive queue, which ethtool calls `rx-ntuple-filter`. A card reporting it as
`off [fixed]` under `ethtool -k <IF>` cannot, and no ethtool setting changes that —
Mellanox ConnectX-3 under `mlx4` is the common case. Such a node starts, accelerates
transmit only, and says so once. Transmit itself asks nothing of the card beyond a
driver AF_XDP supports.

The mode the node ended up in — `exclusive`, `chained`, `adopted` or `off` — is
reported in the `xdp-network-config` metric.

If a previous run left the interface configured and you want it back as it was,
`agave-validator reset-xdp-interface --interface <IF>` removes the steering rules
and restores the queue count.

> [!NOTE]
> A binary carrying file capabilities is marked non-dumpable by the kernel, which
> disables core dumps. The validator restores dumpability once it has dropped the
> capabilities, so crash dumps still work on a running node.
