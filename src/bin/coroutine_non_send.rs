use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::AtomicBool,
        mpsc::{self, channel, sync_channel},
    },
    time::{Duration, Instant},
};

use mlua::{FromLua, Function, Lua, Table, Thread as LuaThread, UserData, Variadic};
use threadpool::ThreadPool;

#[derive(Clone)]
struct EventLoop {
    to_wake: mpsc::Sender<LuaThread>,
    thread_pool: ThreadPool,
    timers: Arc<Mutex<TimerManager>>,
    run_registry: Arc<Mutex<RunRegistry>>,
}

impl EventLoop {
    fn new() -> (Self, mpsc::Receiver<LuaThread>) {
        let (send, recv) = channel();
        let event_loop = Self {
            to_wake: send,
            thread_pool: ThreadPool::with_name("eventloop threadpool".into(), 128),
            timers: Arc::new(Mutex::new(TimerManager::new())),
            run_registry: Arc::new(Mutex::new(RunRegistry::new().0)),
        };

        (event_loop, recv)
    }
}

impl UserData for EventLoop {}

impl FromLua for EventLoop {
    fn from_lua(value: mlua::Value, _: &Lua) -> mlua::Result<Self> {
        Ok(value.as_userdata().unwrap().borrow::<Self>()?.clone())
    }
}

type Id = u32;

struct RunRegistry {
    next_id: Id,
    functions: BTreeMap<Id, Function>,
    channel: mpsc::Receiver<RunCommand>,
}

impl RunRegistry {
    fn new() -> (Self, mpsc::Sender<RunCommand>) {
        let (send, recv) = mpsc::channel();
        (
            Self {
                next_id: 0,
                functions: BTreeMap::new(),
                channel: recv,
            },
            send,
        )
    }

    fn register_fn(&mut self, f: Function) -> Id {
        let id = self.next_id;
        self.next_id += 1;
        self.functions.insert(id, f);
        id
    }

    fn register_thread(&mut self, l: &Lua, thread: LuaThread) -> mlua::Result<Id> {
        let f = l.create_function(move |_, ()| {
            thread.resume::<()>(())?;
            Ok(())
        })?;
        Ok(self.register_fn(f))
    }
}

enum RunCommand {
    Wake(Id),
    Drop(Id),
}

struct RunHandle {
    id: Id,
    registry: mpsc::Sender<RunCommand>,
}

impl RunHandle {
    fn wake(self) {
        let _ = self.registry.send(RunCommand::Wake(self.id));
    }
}

impl Drop for RunHandle {
    fn drop(&mut self) {
        let _ = self.registry.send(RunCommand::Drop(self.id));
    }
}

trait AsLua: Send {
    fn as_lua(self: Box<Self>, l: &Lua) -> mlua::Result<mlua::Value>;
}

#[derive(Clone)]
struct CancelationToken(Arc<AtomicBool>);

impl CancelationToken {
    fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn is_canceled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl UserData for CancelationToken {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("cancel", |_, this, ()| {
            this.cancel();
            Ok(())
        });
        methods.add_method("is_canceled", |_, this, ()| Ok(this.is_canceled()));
    }
}

struct RepeatTimer {
    interval: Duration,
    function: Function,
    immediate: bool,
    remaining: Option<u32>,
}

struct TimerManager {
    timers: BTreeMap<Instant, InnerTimer>,
}

struct Repeating {
    interval: Duration,
    function: Function,
    remaining: Option<u32>,
    ran: u32,
    notify_on_end: Box<dyn FnOnce() + Send + 'static>,
    token: CancelationToken,
}

enum InnerTimer {
    Oneshot(LuaThread, CancelationToken),
    Repeating(Repeating),
}

impl TimerManager {
    fn new() -> Self {
        Self {
            timers: BTreeMap::new(),
        }
    }

    fn add_timer(&mut self, when: Instant, thread: LuaThread) -> CancelationToken {
        let token = CancelationToken::new();
        self.timers
            .insert(when, InnerTimer::Oneshot(thread, token.clone()));
        token
    }

    fn add_repeating_timer(
        &mut self,
        RepeatTimer {
            interval,
            function,
            immediate,
            remaining,
        }: RepeatTimer,
        notify_end: impl FnOnce() + Send + 'static,
    ) -> CancelationToken {
        let when = if immediate {
            Instant::now()
        } else {
            Instant::now() + interval
        };

        let token = CancelationToken::new();

        self.timers.insert(
            when,
            InnerTimer::Repeating(Repeating {
                interval,
                function,
                remaining,
                ran: 0,
                notify_on_end: Box::new(notify_end),
                token: token.clone(),
            }),
        );

        token
    }

    fn poll_timers(&mut self, mut f: impl FnMut(&InnerTimer)) {
        let now = Instant::now();

        loop {
            let Some(entry) = self.timers.first_entry() else {
                break;
            };

            if *entry.key() > now {
                break;
            }

            let next = *entry.key();
            match entry.remove() {
                InnerTimer::Oneshot(thread, token) => {
                    if !token.is_canceled() {
                        f(&InnerTimer::Oneshot(thread, token));
                    }
                }
                InnerTimer::Repeating(mut repeating) => {
                    if repeating.token.is_canceled() {
                        (repeating.notify_on_end)();
                        continue;
                    }

                    if let Some(remaining) = &mut repeating.remaining {
                        if *remaining == 1 {
                            repeating.ran += 1;

                            let timer = InnerTimer::Repeating(repeating);

                            f(&timer);

                            match timer {
                                InnerTimer::Repeating(r) => repeating = r,
                                _ => unreachable!(),
                            }

                            (repeating.notify_on_end)();
                            continue;
                        }
                        *remaining -= 1;
                    }

                    // since `ran` starts at 0, we increment before calling to be consistent with lua's
                    // 1-based indexing
                    repeating.ran += 1;

                    let next_time = next + repeating.interval; // before for borrow checker
                    let timer = InnerTimer::Repeating(repeating);

                    f(&timer);
                    self.timers.insert(next_time, timer);
                }
            }
        }
    }
}

struct Async;

impl UserData for Async {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method("__call", move |l, _, f: Function| {
            let thread = l.create_thread(f)?;
            thread.resume::<()>(())?;
            Ok(thread)
        });

        methods.add_async_function("all", async move |l, threads: Variadic<LuaThread>| {
            let ev = get_ev(&l)?;
            let current_thread = l.current_thread();
            for thread in threads {
                while matches!(thread.status(), mlua::ThreadStatus::Resumable) {
                    ev.to_wake.send(current_thread.clone()).unwrap();
                    l.yield_with::<()>(()).await?;
                }
            }
            Ok(())
        });
    }
}

fn main() -> anyhow::Result<()> {
    let lua = Lua::new();

    let (ev, recv) = EventLoop::new();

    let ev2 = ev.clone();
    lua.set_named_registry_value("ev", ev2)?;

    let read_file_async = lua.create_async_function(async move |l, path: String| {
        let s = wrap_blocking(&l, move || {
            std::fs::read_to_string(&path).map_err(mlua::Error::external)
        })
        .await??;

        Ok(s)
    })?;

    let exit_game = lua.create_function(|_, code: i32| -> Result<(), mlua::Error> {
        println!("Exiting game with code {}", code);
        std::process::exit(code);
    })?;

    let sleep = lua.create_async_function(async move |l, s: f32| {
        let ev = get_ev(&l)?;
        let thread = l.current_thread();
        let resume_at = Instant::now() + Duration::from_secs_f32(s);
        println!("a");
        ev.timers.lock().unwrap().add_timer(resume_at, thread);

        let mut warn = false;

        while Instant::now() < resume_at {
            if warn {
                println!("Warning: sleep() thread woken up early");
            }
            l.yield_with::<()>(()).await?;
            warn = true;
        }

        Ok(())
    })?;

    let repeat_every = lua
        .create_async_function(|l, (f, interval, options)| repeat_every(l, f, interval, options))?;

    lua.globals().set("read_file_async", read_file_async)?;
    lua.globals().set("exit_game", exit_game)?;
    lua.globals().set("async", lua.create_userdata(Async)?)?;
    lua.globals().set("sleep", sleep)?;
    lua.globals().set("repeatEvery", repeat_every)?;

    // let lua_code = r#"
    //     threads = {}
    //     for i = 1, 10 do
    //         table.insert(threads, async(function()
    //             print("Hello from coroutine " .. i)
    //             sleep(1)
    //             print("Coroutine " .. i .. " woke up after 1 second")
    //         end))
    //     end
    //     async(function()
    //         async.all(unpack(threads))
    //         print("All coroutines have completed")
    //         exit_game(0)
    //     end)
    // "#;

    // let lua_code = r#"
    //     local flag = repeatEvery(function(i) print(i) end, 1.0)
    //     repeatEvery(function() print("aaaaaaa") end, 1.0)
    //     async(function()
    //         local interval = 0.5
    //         repeatEvery(function(i)
    //             print("Repeating every " .. interval .. " seconds, now at " .. i .. " iterations")
    //         end, interval, { immediate = false, max_count = 5, wait = true })
    //         sleep(2.0)
    //         flag:cancel()
    //         print("aa2")
    //         sleep(2.0)
    //         print("aa2")
    //         exit_game(0)
    //         -- exit_game(0)
    //     end)
    // "#;

    let lua_code = r#"
        async(function()
            print("1")
            sleep(1.0)
            print("2")
            sleep(1.0)
            print("3")
            sleep(1.0)
            print("4")
        end)
    "#;

    lua.load(lua_code).exec()?;

    let mut wake_in_frame = vec![];

    // main game loop
    loop {
        // poll timers
        println!("Polling timers...");
        ev.timers.lock().unwrap().poll_timers(|r| {
            match r {
                InnerTimer::Oneshot(f, _) => {
                    wake_in_frame.push(f.clone());
                }
                InnerTimer::Repeating(Repeating {
                    function,
                    ran,
                    token,
                    ..
                }) => function.call((*ran, token.clone())).unwrap(),
            };
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
    let ev = get_ev(lua)?;
    let current_thread = lua.current_thread();

    let (tx, rx) = channel();
    ev.thread_pool.execute(move || {
        // let result = f();
        // tx.send(result).unwrap();
        // ev.to_wake.send(current_thread).unwrap();


        let result = f();
        tx.send(result).unwrap();
        ev.to_wake.send
    });

    let result = loop {
        if let Ok(to_wake) = rx.try_recv() {
            break to_wake;
        }
        lua.yield_with::<()>(()).await?;
    };

    Ok(result)
}

async fn repeat_every(
    l: Lua,
    f: LuaRunnable,
    interval: f32,
    options: Option<Table>,
) -> Result<Option<CancelationToken>, mlua::Error> {
    let ev = get_ev(&l)?;

    let (now, max_count, wait) = if let Some(opts) = options {
        let now = opts.get("immediate").unwrap_or(false);
        let count = opts.get("max_count").ok();
        let wait = opts.get("wait").unwrap_or(false);
        (now, count, wait)
    } else {
        (false, None, false)
    };

    if max_count == Some(0) {
        return Ok(None);
    }

    if wait {
        if max_count.is_none() {
            return Err(mlua::Error::RuntimeError(
                "If 'wait' is true, 'count' must be specified".into(),
            ));
        }

        let current_thread = l.current_thread();
        let (tx, rx) = sync_channel(1);

        let to_wake = ev.to_wake.clone();
        ev.timers.lock().unwrap().add_repeating_timer(
            RepeatTimer {
                interval: Duration::from_secs_f32(interval),
                function: f,
                immediate: now,
                remaining: max_count,
            },
            move || {
                to_wake.send(current_thread).unwrap();
                let _ = tx.send(());
            },
        );

        l.yield_with::<()>(()).await?;
        while matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)) {
            ev.to_wake.send(l.current_thread()).unwrap();
            l.yield_with::<()>(()).await?;
        }

        Ok(None)
    } else {
        let stop_flag = ev.timers.lock().unwrap().add_repeating_timer(
            RepeatTimer {
                interval: Duration::from_secs_f32(interval),
                function: f,
                immediate: now,
                remaining: max_count,
            },
            || {},
        );

        Ok(Some(stop_flag))
    }
}

fn get_ev(l: &Lua) -> mlua::Result<EventLoop> {
    l.named_registry_value("ev")
}
