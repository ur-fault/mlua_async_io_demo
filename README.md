# Async IO demo for [mlua](https://github.com/mlua-rs/mlua)

Contained in this repository are experiments of implementing async IO for Lua in use by game mods.

Goal is basically mini reactor with event loop (= game loop).

## Rationale

I'm building [TMaze](https://github.com/ur-fault/tmaze), maze solving game, for which I plan to add modding support throught Lua.
Games need to use non-blocking API whenever possible, so of course I want to provide async IO to Lua scripts. 
For handling mainly FS, but networking too. For seamless communitaction between threads it's also useful.

## Current state

So in this repo are various ways of implementing async

- using callbacks
- using coroutines over spawning threads
- using coroutines over threadpool
- using coroutines but Lua state is non-`Send`

I also found SEGFAULT in one of them, so I'm condensing the example into a minimal one that reproduces the issue.

## References

- [mio](https://github.com/tokio-rs/mio#)
- [libuv](https://libuv.org/)
