# Snap Coin Miner
## Installation
To install Snap Coin Node, run:
```bash
cargo install snap-coin-miner
```
Make sure you have cargo, and rust installed.

## General Information
By default the miner pulls the `toml` config from the running directory as `miner.toml`

## Usage
```bash
snap-coin-node <args>
```
Available arguments:

1. `--config <path>`
Specifies path to a `toml` miner config.

2. `--pool`
Starts the miner in pool mode.

3. `--no-dataset`
Starts the miner without initializing the full RandomX scratchpad, highly unrecommended.

4. `--no-full-pages`
Starts the miner without utilizing full pages. Use this if your system crashes without it.

## Configuration
The miner configuration is stored in a toml file that is structured like this:
```toml
[node]
address = "<your Snap Coin API node address and port (eg. 127.0.0.1:3003) or pool address and port>"

[miner]
public = "<your public wallet address>"

[threads]
count = <amount of threads to run on, -1 for max>
```

## Huge pages
The miner support huge pages (a big optimization), however you must enable it for your OS. On linux run:
```bash
sudo sysctl -w vm.nr_hugepages=2048
```
*(this command does not persist during system reboots)*
to enable them. If your system does not support full pages, pass in `--no-full-pages`.