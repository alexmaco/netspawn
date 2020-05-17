# Netspawn

A small older project, used to work around an issue with `socat`.

Built directly on mio (in those days async was not yet featureful)

## What it does

- starts with a command (and arguments to spawn)
- listens on a local port
- for every incoming connection, spawns a new process from the given arguments
- the new process has its stdin and stdout wired to the tcp connection
- when the connection ends, the child is killed
- when the child exits, the connection is killed

## Installation

`cargo install -f --git https://github.com/alexmaco/netspawn`
