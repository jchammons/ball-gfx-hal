#!/bin/bash

for i in $(seq 1 $1); do
    target/release/ball-gfx-hal --client $2 &
done

# wait for the most recently spawned process to terminate
wait $!

# kill all child processes of this process
pkill -s SIGINT -P $$
