# Flashblocks Websocket Proxy

## Overview
The Flashblocks Websocket Proxy is a service that subscribes to new Flashblocks from
[rollup-boost](https://github.com/flashbots/rollup-boost) on the sequencer. Then broadcasts them out to any downstream 
RPC nodes. Minimizing the number of connections to the sequencer and restricting access.

> ⚠️ **Warning**
> 
> This is currently alpha software -- deploy at your own risk!
> 
> Currently, this project is a one-directional generic websocket proxy. It doesn't inspect any data or validate clients.
> This may not always be the case.

## For Developers

### Contributing

### Building & Testing
You can build and test the project using [Cargo](https://doc.rust-lang.org/cargo/). Some useful commands are:
```
# Build the project
cargo build

# Run all the tests
cargo test --all-features
```

### Deployment
Builds of the websocket proxy [are provided](https://github.com/base/flashblocks-websocket-proxy/pkgs/container/flashblocks-websocket-proxy).
The only configuration required is the rollup-boost URL to proxy. You can set this via an env var `UPSTREAM_WS` or a flag `--upstream-ws`.


You can see a full list of parameters by running:

`docker run ghcr.io/base/flashblocks-websocket-proxy:master --help`

