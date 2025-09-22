use std::{sync::mpsc::channel, thread};

use mlua::{FromLua, Function, Lua, Thread, UserData};

fn main() {
    let lua = Lua::new();

    let (send, recv) = channel();

    #[derive(Clone)]
    struct EventLoop {
        to_wake: std::sync::mpsc::Sender<Thread>,
    }

    impl UserData for EventLoop {}

    impl FromLua for EventLoop {
        fn from_lua(value: mlua::Value, _: &Lua) -> mlua::Result<Self> {
            Ok(value.as_userdata().unwrap().borrow::<Self>()?.clone())
        }
    }

    let ev = EventLoop { to_wake: send };

    lua.set_named_registry_value("ev", ev.clone()).unwrap();

    let read_file_async = lua
        .create_async_function(async move |l, path: String| {
            let ev = l.named_registry_value::<EventLoop>("ev").unwrap();
            let current_thread = l.current_thread();
            println!("1");

            let handle = thread::spawn(move || -> Result<_, mlua::Error> {
                let s = std::fs::read_to_string(&path).map_err(mlua::Error::external)?;
                ev.to_wake.send(current_thread).unwrap();
                Ok(s)
            });

            while !handle.is_finished() {
                println!("2");
                let _: () = l.yield_with(()).await?;
            }

            println!("3");

            Ok(handle.join().unwrap()?)
        })
        .unwrap();

    let exit_game = lua
        .create_function(|_, code: i32| -> Result<(), mlua::Error> {
            std::process::exit(code);
        })
        .unwrap();

    let spawn_async = lua
        .create_function(|l, f: Function| -> Result<(), mlua::Error> {
            let thread = l.create_thread(f)?;
            let _: () = thread.resume(())?;
            Ok(())
        })
        .unwrap();

    lua.globals()
        .set("read_file_async", read_file_async)
        .unwrap();
    lua.globals().set("exit_game", exit_game).unwrap();
    lua.globals().set("async", spawn_async).unwrap();
    // lua.globals().set("print", print).unwrap();

    let lua_code = r#"
        async(function()
            content = read_file_async("Cargo.toml")
            print("4")
            print(content)
        end)
    "#;

    lua.load(lua_code).exec().unwrap();

    // main game loop
    loop {
        while let Ok(to_wake) = recv.try_recv() {
            let _: () = to_wake.resume(()).unwrap();
        }

        // simulate frame time
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}
