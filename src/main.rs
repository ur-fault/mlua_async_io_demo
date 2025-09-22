use std::{
    sync::{Arc, Mutex, mpsc::channel},
    thread,
};

use mlua::{FromLua, Function, Lua, UserData};

fn main() {
    let lua = Lua::new();

    let (send, recv) = channel();

    type Callback = Box<dyn FnOnce(&Lua) -> mlua::Result<()> + Send>;
    type IoThead = thread::JoinHandle<Result<(), mlua::Error>>;

    #[derive(Clone)]
    struct EventLoop {
        send: std::sync::mpsc::Sender<Callback>,
        threads: Arc<Mutex<Vec<IoThead>>>,
    }

    impl UserData for EventLoop {}

    impl FromLua for EventLoop {
        fn from_lua(value: mlua::Value, _: &Lua) -> mlua::Result<Self> {
            Ok(value.as_userdata().unwrap().borrow::<Self>()?.clone())
        }
    }

    let ev = EventLoop {
        send,
        threads: Arc::default(),
    };

    lua.set_named_registry_value("ev", ev.clone()).unwrap();

    let read_file_async = lua
        .create_function(|l, (path, callback): (String, Function)| {
            let ev = l.named_registry_value::<EventLoop>("ev").unwrap();

            ev.threads
                .lock()
                .unwrap()
                .push(thread::spawn(move || -> Result<(), mlua::Error> {
                    let s = std::fs::read_to_string(&path).map_err(mlua::Error::external)?;
                    ev.send
                        .send(Box::new(move |_| {
                            callback.call::<()>(s)?;
                            Ok(())
                        }))
                        .unwrap();
                    Ok(())
                }));
            Ok(())
        })
        .unwrap();

    let exit_game = lua
        .create_function(|_, code: i32| -> Result<(), mlua::Error> {
            std::process::exit(code);
        })
        .unwrap();

    lua.globals()
        .set("read_file_async", read_file_async)
        .unwrap();
    lua.globals().set("exit_game", exit_game).unwrap();
    // lua.globals().set("print", print).unwrap();

    let lua_code = r#"
        read_file_async("Cargo.toml", function(content)
            print("File content length:", #content)
            print("Exiting game.")
            exit_game(0)
        end)
    "#;

    lua.load(lua_code).exec().unwrap();

    // main game loop
    loop {
        // process all callbacks
        while let Ok(cb) = recv.try_recv() {
            cb(&lua).unwrap();
        }

        let mut lock = ev.threads.lock().unwrap();
        for i in (0..lock.len()).rev() {
            println!("Checking thread {}", i);
            if lock[i].is_finished() {
                let th = lock.remove(i);
                th.join().unwrap().unwrap();
            }
        }
        drop(lock);

        // simulate frame time
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}
