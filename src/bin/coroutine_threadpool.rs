use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        mpsc::{self, channel},
    },
    time::{Duration, Instant},
};

use mlua::{FromLua, Function, Lua, Thread as LuaThread, UserData};
use threadpool::ThreadPool;

#[derive(Clone)]
struct EventLoop {
    to_wake: mpsc::Sender<LuaThread>,
    thread_pool: ThreadPool,
    timers: Arc<Mutex<TimerManager>>,
}

impl EventLoop {
    fn new() -> (Self, mpsc::Receiver<LuaThread>) {
        let (send, recv) = channel();
        (
            Self {
                to_wake: send,
                thread_pool: ThreadPool::with_name("eventloop threadpool".into(), 128),
                timers: Arc::new(Mutex::new(TimerManager::new())),
            },
            recv,
        )
    }
}

impl UserData for EventLoop {}

impl FromLua for EventLoop {
    fn from_lua(value: mlua::Value, _: &Lua) -> mlua::Result<Self> {
        Ok(value.as_userdata().unwrap().borrow::<Self>()?.clone())
    }
}

struct TimerManager {
    timers: BTreeMap<Instant, LuaThread>,
}

impl TimerManager {
    fn new() -> Self {
        Self {
            timers: BTreeMap::new(),
        }
    }

    fn add_timer(&mut self, when: Instant, thread: LuaThread) {
        self.timers.insert(when, thread);
    }

    fn poll_timers(&mut self, mut f: impl FnMut(LuaThread)) {
        let now = Instant::now();

        loop {
            let Some(entry) = self.timers.first_entry() else {
                break;
            };

            if *entry.key() > now {
                break;
            }

            f(self.timers.pop_first().unwrap().1);
        }
    }
}

fn main() -> anyhow::Result<()> {
    let lua = Lua::new();

    let (ev, recv) = EventLoop::new();

    let ev2 = ev.clone();
    lua.set_named_registry_value("ev", ev2)?;

    struct Async;

    impl UserData for Async {
        fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
            methods.add_meta_method("__call", move |l, _, f: Function| {
                let thread = l.create_thread(f)?;
                thread.resume::<()>(())?;
                Ok(thread)
            });

            methods.add_async_function(
                "all",
                async move |l, threads: mlua::Variadic<LuaThread>| {
                    println!("Waiting for {} threads to complete", threads.len());
                    let ev = l.named_registry_value::<EventLoop>("ev")?;
                    let current_thread = l.current_thread();
                    for thread in threads {
                        while matches!(thread.status(), mlua::ThreadStatus::Resumable) {
                            ev.to_wake.send(current_thread.clone()).unwrap();
                            l.yield_with::<()>(()).await?;
                        }
                    }
                    Ok(())
                },
            );
        }
    }

    let read_file_async = lua.create_async_function(async move |l, path: String| {
        let s: String = wrap_blocking(&l, move || {
            std::fs::read_to_string(&path).map_err(mlua::Error::external)
        })
        .await??;

        Ok(s)
    })?;

    let exit_game = lua.create_function(|_, code: i32| -> Result<(), mlua::Error> {
        std::process::exit(code);
    })?;

    let sleep = lua.create_async_function(async move |l, s: f32| {
        let ev = l.named_registry_value::<EventLoop>("ev")?;
        let thread = l.current_thread();
        let resume_at = Instant::now() + Duration::from_secs_f32(s);
        ev.timers.lock().unwrap().add_timer(resume_at, thread);

        while Instant::now() < resume_at {
            l.yield_with::<()>(()).await?;
        }

        Ok(())
    })?;

    lua.globals().set("read_file_async", read_file_async)?;
    lua.globals().set("exit_game", exit_game)?;
    lua.globals().set("async", lua.create_userdata(Async)?)?;
    lua.globals().set("sleep", sleep)?;

    let lua_code = r#"
        threads = {}
        for i = 1, 10 do
            table.insert(threads, async(function()
                print("Hello from coroutine " .. i)
                sleep(1)
                print("Coroutine " .. i .. " woke up after 1 second")
            end))
        end
        async(function()
            async.all(unpack(threads))
            print("All coroutines have completed")
        end)
    "#;

    lua.load(lua_code).exec()?;

    let mut wake_in_frame = vec![];

    // main game loop
    loop {
        // poll timers
        ev.timers.lock().unwrap().poll_timers(|to_wake| {
            ev.to_wake.send(to_wake).unwrap();
        });

        while let Ok(to_wake) = recv.try_recv() {
            wake_in_frame.push(to_wake);
        }

        for to_wake in wake_in_frame.drain(..) {
            to_wake.resume::<()>(())?;
        }

        // simulate frame time
        std::thread::sleep(std::time::Duration::from_secs_f32(0.1f32));
    }
}

async fn wrap_blocking<T: Send + 'static>(
    lua: &Lua,
    f: impl FnOnce() -> T + Send + 'static,
) -> anyhow::Result<T> {
    let ev = lua.named_registry_value::<EventLoop>("ev").unwrap();
    let current_thread = lua.current_thread();

    let (tx, rx) = channel();
    ev.thread_pool.execute(move || {
        let result = f();
        tx.send(result).unwrap();
        ev.to_wake.send(current_thread).unwrap();
    });

    let result = loop {
        if let Ok(to_wake) = rx.try_recv() {
            break to_wake;
        }
        lua.yield_with::<()>(()).await?;
    };

    Ok(result)
}
