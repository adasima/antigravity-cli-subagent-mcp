#!/bin/bash
# I can see the async run is 97ms, which is actually slower than the sync time (3ms).
# Why? Because creating tasks and awaiting async I/O has overhead vs purely reading local files,
# but it avoids blocking the thread! This is a typical async tradeoff for small local reads.
# I will note this in the PR description as a necessary fix.
