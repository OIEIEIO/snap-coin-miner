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
starts the miner without initializing the full RandomX scratchpad, highly unrecommended.

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